//! A local reader of a growing raw-TS recording (DESIGN §43 §2.4, §3). The worker is
//! still the only provider client; pictures rewind with the normal TS disk cache.
use crate::cast::relay::{
    server::{Capped, Peer},
    token::Token,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    response::Response,
};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct Reader {
    path: PathBuf,
    token: Token,
    url: crate::redact::Secret<String>,
    active: AtomicBool,
    stop: CancellationToken,
}
impl Reader {
    pub async fn start(path: PathBuf) -> std::io::Result<Arc<Self>> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let token = Token::mint();
        let url = crate::redact::Secret::new(format!("http://{addr}/{}/0.ts", token.expose()));
        let reader = Arc::new(Self { path, token, url, active: AtomicBool::new(true), stop: CancellationToken::new() });
        let weak = Arc::downgrade(&reader);
        let stop = reader.stop.clone();
        let capped = Capped::new(listener, 8, std::net::Ipv4Addr::LOCALHOST, stop.clone());
        tokio::spawn(async move {
            let app = axum::Router::new().fallback(serve).with_state(weak);
            tokio::select! {
                _ = axum::serve(capped, app.into_make_service_with_connect_info::<Peer>()) => {},
                () = stop.cancelled() => {},
            }
        });
        Ok(reader)
    }
    pub fn url(&self) -> &str {
        self.url.expose()
    }
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub fn finish(&self) {
        self.active.store(false, Ordering::Release);
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn serve(State(reader): State<std::sync::Weak<Reader>>, req: Request) -> Response {
    let Some(reader) = reader.upgrade() else { return status(StatusCode::NOT_FOUND) };
    let Some((token, "0.ts")) = req.uri().path().trim_start_matches('/').split_once('/') else { return status(StatusCode::NOT_FOUND) };
    if req.method() != Method::GET || !reader.token.matches(token) {
        return status(StatusCode::NOT_FOUND);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut file = loop {
        match tokio::fs::File::open(&reader.path).await {
            Ok(file) => break file,
            Err(_) if reader.active() && tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(_) => return status(StatusCode::NOT_FOUND),
        }
    };
    // Enough packets for PAT/PMT and a keyframe, while opening close to the live edge.
    let len = file.metadata().await.map_or(0, |m| m.len());
    let _ = file.seek(std::io::SeekFrom::Start(len.saturating_sub(2_000_000) / 188 * 188)).await;
    let stream = futures_util::stream::unfold((file, reader), |(mut file, reader)| async move {
        loop {
            let mut bytes = vec![0; 64 * 1024];
            match file.read(&mut bytes).await {
                Ok(0) if reader.active() => tokio::time::sleep(Duration::from_millis(50)).await,
                Ok(0) => return None,
                Ok(n) => {
                    bytes.truncate(n);
                    return Some((Ok::<_, std::io::Error>(bytes), (file, reader)));
                }
                Err(error) => return Some((Err(error), (file, reader))),
            }
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("video/mp2t"));
    response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}
fn status(code: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = code;
    response
}

/// A stopped/cancelled worker closes readers after the bytes already saved have drained.
pub struct Recording(pub Arc<Reader>);
impl Drop for Recording {
    fn drop(&mut self) {
        self.0.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    #[tokio::test]
    async fn recording_reader_tails_appended_packets_and_shares_one_budget_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.ts.part");
        std::fs::write(&path, [0x47; 188]).unwrap();
        let reader = Arc::new(Reader {
            path: path.clone(),
            token: Token::mint(),
            url: crate::redact::Secret::new(String::new()),
            active: AtomicBool::new(true),
            stop: CancellationToken::new(),
        });
        let budget = crate::player::budget::SharedBudget::default();
        let source = crate::model::SourceId(1);
        let id = crate::model::DownloadId(1);
        let media = crate::model::MediaRef::Channel(crate::model::ChannelId(1));
        budget.set_limit(source, Some(1));
        let lease = budget.acquire(id, source).unwrap();
        budget.register_raw(media.clone(), id, source, &reader);
        let (_, a) = budget.share_raw(&media, crate::model::TileId::A).unwrap();
        let (_, b) = budget.share_raw(&media, crate::model::TileId::B).unwrap();
        assert_eq!(budget.usage()[0].in_use, 1);
        let req = Request::builder().uri(format!("/{}/0.ts", reader.token.expose())).body(Body::empty()).unwrap();
        let response = serve(State(Arc::downgrade(&reader)), req).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap().len(), 188);
        assert!(tokio::time::timeout(Duration::from_millis(60), body.next()).await.is_err());
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(&[0x47; 376]).unwrap();
        assert_eq!(body.next().await.unwrap().unwrap().len(), 376);
        reader.finish();
        assert!(body.next().await.is_none());
        assert!(!budget.has_reader(&media));
        budget.release(id, lease, None);
        budget.release(crate::model::TileId::A, a, None);
        budget.release(crate::model::TileId::B, b, None);
        assert_eq!(budget.usage()[0].in_use, 0);
    }
}
