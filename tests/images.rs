//! Image decoding limits, disk cache layout/pruning and the `ImageCache` disk path through the
//! public API. Owner: WP-09. DESIGN §18 / §24.2 WP-09. The loader internals (concurrency,
//! staleness, upload budget, LRU, negative cache) are unit-tested in `src/images.rs` with the
//! private `Fetch` seam.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32};
use tuner::images::{ImageCache, ImageKind, ImageState, cache_path, decode, prune_disk};

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn px(img: &egui::ColorImage, x: usize, y: usize) -> Color32 {
    img.pixels[y * img.size[0] + x]
}

fn fits(img: &egui::ColorImage, kind: ImageKind) -> bool {
    let (w, h) = kind.max_size();
    img.size[0] <= w as usize && img.size[1] <= h as usize && img.pixels.len() == img.size[0] * img.size[1]
}

#[test]
fn png_with_alpha_16_bit_downscales_into_the_logo_box() {
    let img = decode(&fixture("logo_600x300.png"), ImageKind::Logo).unwrap();
    assert_eq!(img.size, [128, 64]);
    // vertical gradient red (top) → blue (bottom), 80 % opaque
    let (top, bottom) = (px(&img, 64, 1), px(&img, 64, 62));
    assert!(top.a() > 150 && top.a() < 255, "translucent alpha kept: {top:?}");
    assert!(top.r() > top.b() && bottom.b() > bottom.r(), "{top:?} {bottom:?}");
}

#[test]
fn jpeg_poster_fits_the_poster_box_exactly() {
    let img = decode(&fixture("poster_1000x1500.jpg"), ImageKind::Poster).unwrap();
    assert_eq!(img.size, [342, 513]);
    assert_eq!(px(&img, 10, 10).a(), 255);
    let still = decode(&fixture("poster_1000x1500.jpg"), ImageKind::Still).unwrap();
    assert_eq!(still.size, [150, 225]);
}

#[test]
fn webp_and_gif_first_frame() {
    let webp = decode(&fixture("small_200x100.webp"), ImageKind::Logo).unwrap();
    assert_eq!(webp.size, [128, 64]);
    let c = px(&webp, 64, 32);
    assert!(c.b() > c.r() && c.b() > 150, "#3366cc-ish: {c:?}");

    // small images are never upscaled
    let gif = decode(&fixture("anim_64x48.gif"), ImageKind::Backdrop).unwrap();
    assert_eq!(gif.size, [64, 48]);
    let c = px(&gif, 10, 10);
    assert!(c.r() > 200 && c.b() < 50, "first frame is red: {c:?}");

    let tiny = decode(&fixture("tiny_40x20.png"), ImageKind::Logo).unwrap();
    assert_eq!(tiny.size, [40, 20]);
}

#[test]
fn svg_is_rasterized_at_the_box_without_fonts_or_external_images() {
    let logo = decode(&fixture("logo.svg"), ImageKind::Logo).unwrap();
    assert_eq!(logo.size, [128, 64]);
    let c = px(&logo, 100, 8);
    assert!(c.b() > 200 && c.r() < 60, "rect fill #1e88e5: {c:?}");
    // vector sources scale up to the kind's box
    let backdrop = decode(&fixture("logo.svg"), ImageKind::Backdrop).unwrap();
    assert_eq!(backdrop.size, [1280, 640]);
    assert!(fits(&backdrop, ImageKind::Backdrop));
}

/// Blue rect with a red raster `<image>` (`href`) covering its top-left quarter.
fn svg_with_image(href: &str) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">
  <rect width="200" height="100" fill="#1e88e5"/>
  <image href="{href}" x="0" y="0" width="100" height="50" preserveAspectRatio="none"/>
</svg>"##
    )
}

#[test]
fn svg_never_loads_local_files_or_embedded_images() {
    use base64::Engine as _;
    let tmp = tempfile::tempdir().unwrap();
    let red = png(40, 20, [255, 0, 0, 255]);
    let file = tmp.path().join("red.png");
    std::fs::write(&file, &red).unwrap();
    let data = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&red));

    for href in [file.to_str().unwrap().to_owned(), format!("file://{}", file.display()), data] {
        let svg = svg_with_image(&href);
        // sanity: a default renderer draws the image, so this SVG would expose a file read
        let opt = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_str(&svg, &opt).unwrap();
        let mut pixmap = resvg::tiny_skia::Pixmap::new(200, 100).unwrap();
        resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
        let drawn = pixmap.pixel(20, 10).unwrap();
        let default_draws = drawn.red() > 200 && drawn.blue() < 50;

        let img = decode(svg.as_bytes(), ImageKind::Logo).unwrap();
        assert_eq!(img.size, [128, 64]);
        let c = px(&img, 16, 8);
        assert!(c.b() > 200 && c.r() < 60, "{href:.40}: rect colour under the image, got {c:?}");
        if href.starts_with('/') || href.starts_with("data:") {
            assert!(default_draws, "{href:.40}: fixture must be drawable by default ({drawn:?})");
        }
    }
}

#[test]
fn svg_filter_primitives_are_capped() {
    let svg = |uses: usize| {
        let mut s = String::from(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="20" height="20">
  <defs><filter id="b"><feGaussianBlur stdDeviation="1"/></filter></defs>
  <rect id="r" width="10" height="10" fill="#1e88e5" filter="url(#b)"/>"##,
        );
        for i in 0..uses {
            s.push_str(&format!(r##"<use xlink:href="#r" x="{}" y="1"/>"##, i % 10));
        }
        s.push_str("</svg>");
        s
    };
    assert!(decode(svg(3).as_bytes(), ImageKind::Logo).is_ok(), "a few filters are fine");
    // one filter definition, applied 1 + 20 times through `<use>`
    let err = decode(svg(20).as_bytes(), ImageKind::Logo).unwrap_err();
    assert!(err.to_string().contains("filter"), "{err}");
}

#[test]
fn every_kind_respects_its_box() {
    for kind in [ImageKind::Logo, ImageKind::Poster, ImageKind::Backdrop, ImageKind::Still] {
        for f in ["logo_600x300.png", "poster_1000x1500.jpg", "small_200x100.webp", "anim_64x48.gif", "logo.svg"] {
            let img = decode(&fixture(f), kind).unwrap_or_else(|e| panic!("{f} as {kind:?}: {e}"));
            assert!(fits(&img, kind), "{f} as {kind:?}: {:?}", img.size);
            assert!(img.size[0] > 0 && img.size[1] > 0);
        }
    }
}

fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba(rgba));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A PNG whose IHDR claims `w`×`h` RGBA but which carries a single pixel of data.
fn png_header_only(w: u32, h: u32) -> Vec<u8> {
    let mut b = png(1, 1, [0, 0, 0, 255]);
    // signature (8) + chunk length (4) + "IHDR" (4) + width/height; CRC over type + 13 data bytes
    assert_eq!(&b[12..16], b"IHDR");
    b[16..20].copy_from_slice(&w.to_be_bytes());
    b[20..24].copy_from_slice(&h.to_be_bytes());
    let mut crc = flate2::Crc::new();
    crc.update(&b[12..29]);
    b[29..33].copy_from_slice(&crc.sum().to_be_bytes());
    b
}

fn is_limit_error(err: &tuner::error::Error) -> bool {
    matches!(err, tuner::error::Error::BadPayload(m) if m.to_lowercase().contains("limit"))
}

#[test]
fn oversized_dimensions_are_rejected_before_allocating() {
    let t = Instant::now();
    let err = decode(&fixture("huge_header_20000x20000.png"), ImageKind::Backdrop).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(2), "rejected from the header");
    assert!(matches!(err, tuner::error::Error::BadPayload(_)), "{err}");

    // our own limits, not the image crate's defaults (512 MiB alloc, no dimension cap):
    // 9000 px wide but tiny in bytes, rejected by the 8192 px cap …
    let err = decode(&png(9000, 8, [1, 2, 3, 255]), ImageKind::Backdrop).unwrap_err();
    assert!(is_limit_error(&err), "{err}");
    // … the cap itself is allowed …
    assert_eq!(decode(&png(8192, 8, [1, 2, 3, 255]), ImageKind::Backdrop).unwrap().size, [1280, 1]);
    // … and 5000×5000 RGBA (≈ 95 MiB) is over the 64 MiB allocation limit
    let err = decode(&png_header_only(5000, 5000), ImageKind::Backdrop).unwrap_err();
    assert!(is_limit_error(&err), "{err}");
}

#[test]
fn large_svg_and_garbage_are_rejected() {
    let mut big = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><!-- "#);
    big.push_str(&"x".repeat(1 << 20));
    big.push_str(r#" --><rect width="10" height="10"/></svg>"#);
    assert!(decode(big.as_bytes(), ImageKind::Logo).is_err(), "SVG > 1 MiB");
    // the same SVG under the limit decodes
    let small = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#;
    assert_eq!(decode(small.as_bytes(), ImageKind::Logo).unwrap().size, [128, 128]);

    assert!(decode(&fixture("garbage.bin"), ImageKind::Logo).is_err());
    assert!(decode(b"", ImageKind::Logo).is_err());
    assert!(decode(b"<html><body>404</body></html>", ImageKind::Logo).is_err());
    let mut truncated = fixture("logo_600x300.png");
    truncated.truncate(100);
    assert!(decode(&truncated, ImageKind::Logo).is_err());

    // svgz (gzip) is not accepted: no decompression of untrusted vector data
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(small.as_bytes()).unwrap();
    assert!(decode(&gz.finish().unwrap(), ImageKind::Logo).is_err());
}

#[test]
fn cache_path_layout() {
    let dir = Path::new("/c/img");
    let p = cache_path(dir, "http://logos.example/a.png");
    let hex = blake3::hash(b"http://logos.example/a.png").to_hex().to_string();
    assert_eq!(p, dir.join(&hex[..2]).join(format!("{hex}.bin")));
    assert_ne!(p, cache_path(dir, "http://logos.example/b.png"));
}

fn write_aged(path: &Path, len: usize, age_s: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![0u8; len]).unwrap();
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(age_s)).unwrap();
}

#[test]
fn prune_disk_removes_least_recently_modified_first() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("img");
    let urls: Vec<String> = (0..5).map(|i| format!("http://x.example/{i}.png")).collect();
    // url i is (5 - i) hours old, 1000 bytes each
    for (i, u) in urls.iter().enumerate() {
        write_aged(&cache_path(&dir, u), 1000, (5 - i as u64) * 3600);
    }
    let marker = cache_path(&dir, &urls[4]).with_extension("fail");
    write_aged(&marker, 0, 0);

    assert_eq!(prune_disk(&dir, 10_000).unwrap(), 0, "under the limit");
    assert_eq!(prune_disk(&dir, 2500).unwrap(), 3000);
    for (i, u) in urls.iter().enumerate() {
        assert_eq!(cache_path(&dir, u).exists(), i >= 3, "url {i}");
    }
    assert!(marker.exists());
    assert_eq!(prune_disk(&dir, 0).unwrap(), 2000);
    assert_eq!(prune_disk(&tmp.path().join("missing"), 0).unwrap(), 0);
}

#[test]
fn image_cache_serves_disk_hits_without_network() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = tuner::paths::AppPaths::rooted(tmp.path());
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let http = tuner::sources::http::HttpClient::new(&Default::default()).unwrap();
    let mut cache = ImageCache::new(&paths, http, rt.handle().clone(), egui::Context::default(), 16 << 20);
    // unroutable host: a network attempt could never succeed, so Ready proves the disk path
    let url = "http://192.0.2.1/logo.png";
    let p = cache_path(&paths.image_cache(), url);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, fixture("logo_600x300.png")).unwrap();

    assert!(matches!(cache.get("", ImageKind::Logo), ImageState::Failed));
    let deadline = Instant::now() + Duration::from_secs(10);
    let tex = loop {
        match cache.get(url, ImageKind::Logo) {
            ImageState::Ready(t) => break t,
            ImageState::Failed => panic!("disk hit failed"),
            ImageState::Loading => assert!(Instant::now() < deadline, "still loading"),
        }
        cache.pump();
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(tex.size(), [128, 64]);
    assert_eq!(cache.stats(), (1, 128 * 64 * 4));
    cache.clear_memory();
    assert_eq!(cache.stats(), (0, 0));
}

/// [e2e after WP-04] the real HTTP path: download, cache on disk, decode.
#[test]
fn image_cache_downloads_over_http_and_caches_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = tuner::paths::AppPaths::rooted(tmp.path());
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let png = fixture("logo_600x300.png");
    let addr = rt.block_on(async move {
        let app = axum::Router::new().route("/logo.png", axum::routing::get(move || async move { png }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    });
    let http = tuner::sources::http::HttpClient::new(&Default::default()).unwrap();
    let mut cache = ImageCache::new(&paths, http, rt.handle().clone(), egui::Context::default(), 16 << 20);
    let url = format!("http://{addr}/logo.png");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match cache.get(&url, ImageKind::Logo) {
            ImageState::Ready(t) => {
                assert_eq!(t.size(), [128, 64]);
                break;
            }
            ImageState::Failed => panic!("download failed"),
            ImageState::Loading => assert!(Instant::now() < deadline, "still loading"),
        }
        cache.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(cache_path(&paths.image_cache(), &url).is_file());
    let missing = format!("http://{addr}/missing.png");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(cache.get(&missing, ImageKind::Logo), ImageState::Failed) {
        assert!(Instant::now() < deadline);
        cache.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(cache_path(&paths.image_cache(), &missing).with_extension("fail").is_file(), "404 negative-cached on disk");
}
