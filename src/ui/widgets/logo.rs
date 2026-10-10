//! Channel logo / monogram (fit into a square, aspect kept, transparent bg; monogram = first
//! letters on fg@0.04 with fg@0.52 text when missing/failed). Owner: WP-10a.

use eframe::egui;

use super::paint;
use crate::images::{ImageCache, ImageKind, ImageState};
use crate::theme::Theme;

/// Square logo of `size` points. Off-screen logos are never requested (the image cache only
/// fetches what is drawn). While loading, a neutral square is shown (no spinner for logos).
pub fn logo(ui: &mut egui::Ui, theme: &Theme, images: &mut ImageCache, url: Option<&str>, name: &str, size: f32) -> egui::Response {
    let size = size.max(1.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, name));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let t = &theme.tokens;
    let painter = ui.painter();
    match url.filter(|u| !u.is_empty()).map(|u| images.get(u, ImageKind::Logo)) {
        Some(ImageState::Ready(tex)) => {
            let s = tex.size_vec2();
            let scale = (size / s.x.max(1.0)).min(size / s.y.max(1.0));
            let r = egui::Rect::from_center_size(rect.center(), s * scale);
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            painter.image(tex.id(), r, uv, egui::Color32::WHITE);
        }
        Some(ImageState::Loading) => {
            painter.rect_filled(rect, t.rounding, t.fill_field);
        }
        Some(ImageState::Failed) | None => {
            painter.rect_filled(rect, t.rounding, t.fill_field);
            let font = egui::FontId::new((size * 0.4).round().max(6.0), egui::FontFamily::Proportional);
            paint::text(painter, rect.center(), egui::Align2::CENTER_CENTER, monogram(name), font, t.text_secondary, size);
        }
    }
    resp
}

/// Monogram text for a name: "BBC One" → "BO", "CNN" → "CN", digits kept ("ESPN 2" → "E2").
/// Words are runs of letters/digits (punctuation like "|UK|" separates); two or more words give
/// their initials, a single word its first two characters; nothing usable gives "?".
pub fn monogram(name: &str) -> String {
    let mut words = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    let chars: Vec<char> = match (words.next(), words.next()) {
        (Some(a), Some(b)) => a.chars().take(1).chain(b.chars().take(1)).collect(),
        (Some(a), None) => a.chars().take(2).collect(),
        (None, _) => return "?".into(),
    };
    chars.into_iter().flat_map(char::to_uppercase).collect()
}

#[cfg(test)]
mod tests {
    use super::monogram;

    #[test]
    fn monograms() {
        assert_eq!(monogram("BBC One"), "BO");
        assert_eq!(monogram("CNN"), "CN");
        assert_eq!(monogram("ESPN 2"), "E2");
        assert_eq!(monogram("bbc one hd"), "BO");
        assert_eq!(monogram("|UK| Sky News"), "US");
        assert_eq!(monogram("X"), "X");
        assert_eq!(monogram("  "), "?");
        assert_eq!(monogram(""), "?");
        assert_eq!(monogram("ñandú tv"), "ÑT");
        assert_eq!(monogram("Первый канал"), "ПК");
    }
}
