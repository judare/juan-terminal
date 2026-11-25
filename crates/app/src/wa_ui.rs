//! Small pieces shared by the WhatsApp sidebar and chat tabs.

use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, TimeZone};
use gpui::{IntoElement, ParentElement, Styled, StyledImage, div, px};

use whatsapp::MsgStatus;

use crate::theme;

/// WhatsApp green (unread badges, own bubbles accent).
pub const GREEN: u32 = 0x25d366;
pub const OWN_BUBBLE: u32 = 0x1c4f3a;
pub const OTHER_BUBBLE: u32 = 0x26292e;
/// Conversation background (under the wallpaper doodles).
pub const CHAT_BG: u32 = 0x101215;
/// Selected filter chip in the chat list.
pub const CHIP_ACTIVE_BG: u32 = 0x1c4f3a;
pub const CHIP_ACTIVE_TEXT: u32 = 0xd9fdd3;
/// Logical size of one wallpaper tile.
pub const WALLPAPER_TILE: f32 = 360.;
pub const READ_BLUE: u32 = 0x53bdeb;

fn local(ts: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

/// Chat list time: "14:05", "Ayer", "lunes", "03/11/26".
pub fn list_time(ts: i64) -> String {
    let (Some(t), now) = (local(ts), Local::now()) else { return String::new() };
    let days = (now.date_naive() - t.date_naive()).num_days();
    match days {
        0 => t.format("%H:%M").to_string(),
        1 => "Ayer".into(),
        2..=6 => weekday(t.weekday()).into(),
        _ => t.format("%d/%m/%y").to_string(),
    }
}

/// Day separator in a conversation: "Hoy", "Ayer", "lunes", "3 de noviembre de 2026".
pub fn day_label(ts: i64) -> String {
    let (Some(t), now) = (local(ts), Local::now()) else { return String::new() };
    let days = (now.date_naive() - t.date_naive()).num_days();
    match days {
        0 => "Hoy".into(),
        1 => "Ayer".into(),
        2..=6 => weekday(t.weekday()).into(),
        _ => format!("{} de {} de {}", t.day(), month(t.month()), t.year()),
    }
}

pub fn same_day(a: i64, b: i64) -> bool {
    match (local(a), local(b)) {
        (Some(a), Some(b)) => a.date_naive() == b.date_naive(),
        _ => false,
    }
}

pub fn clock(ts: i64) -> String {
    local(ts).map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

fn weekday(w: chrono::Weekday) -> &'static str {
    use chrono::Weekday::*;
    match w {
        Mon => "lunes",
        Tue => "martes",
        Wed => "miércoles",
        Thu => "jueves",
        Fri => "viernes",
        Sat => "sábado",
        Sun => "domingo",
    }
}

fn month(m: u32) -> &'static str {
    ["enero", "febrero", "marzo", "abril", "mayo", "junio", "julio", "agosto", "septiembre", "octubre", "noviembre", "diciembre"][(m.clamp(1, 12) - 1) as usize]
}

pub fn duration(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Delivery ticks for our own messages.
pub fn ticks(status: MsgStatus) -> Option<impl IntoElement> {
    let (path, color) = match status {
        MsgStatus::None => return None,
        MsgStatus::Pending => ("icons/wa-clock.svg", theme::TEXT_FAINT),
        MsgStatus::Sent => ("icons/wa-check.svg", theme::TEXT_MUTED),
        MsgStatus::Delivered => ("icons/wa-check2.svg", theme::TEXT_MUTED),
        MsgStatus::Read | MsgStatus::Played => ("icons/wa-check2.svg", READ_BLUE),
        MsgStatus::Failed => ("icons/wa-alert.svg", theme::DELETED),
    };
    Some(icon(path, 15., color))
}

/// One of our monochrome icons, tinted.
pub fn icon(path: &'static str, size: f32, color: u32) -> gpui::Svg {
    gpui::svg().path(path).flex_none().size(px(size)).text_color(theme::rgb(color))
}

/// Icon shown before a message of this kind in previews (none for plain text).
pub fn kind_icon(kind: whatsapp::MsgKind) -> Option<&'static str> {
    use whatsapp::MsgKind as K;
    Some(match kind {
        K::Image => "icons/wa-photo.svg",
        K::Video => "icons/wa-video.svg",
        K::Audio => "icons/wa-audio.svg",
        K::Voice => "icons/mic.svg",
        K::Sticker => "icons/wa-sticker.svg",
        K::Document => "icons/doc-file.svg",
        K::Location => "icons/wa-location.svg",
        K::Contact => "icons/wa-contact.svg",
        K::Poll => "icons/wa-poll.svg",
        K::Revoked => "icons/wa-revoked.svg",
        K::Text | K::Other => return None,
    })
}

fn initials(name: &str) -> String {
    let name = name.trim_start_matches('~').trim_start_matches('+');
    let mut it = name.split_whitespace().filter_map(|w| w.chars().next()).filter(|c| c.is_alphanumeric());
    let s: String = it.by_ref().take(2).collect();
    if s.is_empty() { "?".into() } else { s.to_uppercase() }
}

fn color_for(name: &str) -> u32 {
    const COLORS: [u32; 8] = [0x6a7fdb, 0xd9822b, 0x3aa58a, 0xc7527a, 0x8e6ad8, 0x2f8fbf, 0xb5953a, 0x5c9e4e];
    let h = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    COLORS[(h % COLORS.len() as u32) as usize]
}

/// Round profile picture, or coloured initials when there's none.
pub fn avatar(path: Option<&PathBuf>, name: &str, size: f32) -> gpui::AnyElement {
    match path {
        Some(p) => gpui::img(p.clone()).size(px(size)).flex_none().rounded_full().object_fit(gpui::ObjectFit::Cover).into_any_element(),
        None => div()
            .size(px(size))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::rgb(color_for(name)))
            .text_size(px(size * 0.38))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(gpui::white())
            .child(initials(name))
            .into_any_element(),
    }
}

/// Render a QR payload to a PNG in the WhatsApp data dir (named by content, so gpui's
/// image cache never shows a stale code).
pub fn qr_png(code: &str, dir: &Path) -> Option<PathBuf> {
    let qr = qrcode::QrCode::new(code.as_bytes()).ok()?;
    let w = qr.width();
    let colors = qr.to_colors();
    let scale = 8u32;
    let border = 4u32;
    let side = (w as u32 + border * 2) * scale;
    let mut img = image::GrayImage::from_pixel(side, side, image::Luma([255]));
    for (i, c) in colors.iter().enumerate() {
        if *c == qrcode::Color::Dark {
            let (x, y) = ((i % w) as u32 + border, (i / w) as u32 + border);
            for dy in 0..scale {
                for dx in 0..scale {
                    img.put_pixel(x * scale + dx, y * scale + dy, image::Luma([0]));
                }
            }
        }
    }
    let hash = code.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let _ = std::fs::create_dir_all(dir);
    // Old codes are useless: keep only the current one.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("qr-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let path = dir.join(format!("qr-{hash:016x}.png"));
    img.save(&path).ok()?;
    Some(path)
}

/// Human file size.
pub fn file_size(path: &Path) -> String {
    let n = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64;
    if n >= 1_048_576. {
        format!("{:.1} MB", n / 1_048_576.)
    } else if n >= 1024. {
        format!("{:.0} KB", n / 1024.)
    } else {
        format!("{n} B")
    }
}

/// Subtle doodle wallpaper tile for conversations (generated once, cached as a PNG).
pub fn wallpaper() -> Option<PathBuf> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let path = whatsapp::WhatsApp::try_global()?.dir().join("wallpaper-v1.png");
        if path.exists() {
            return Some(path);
        }
        let _ = std::fs::create_dir_all(path.parent()?);
        wallpaper_image().save(&path).ok()?;
        Some(path)
    })
    .clone()
}

/// Seamless tile (2× the logical size) with scattered outline shapes in faint white.
fn wallpaper_image() -> image::RgbaImage {
    let side = (WALLPAPER_TILE * 2.) as u32;
    let s = side as f32;
    let mut alpha = vec![0f32; (side * side) as usize];
    let mut seed = 0x2545f491u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed % 10_000) as f32 / 10_000.
    };
    // Signed distance of each shape (negative inside, the outline is |d| < stroke).
    #[derive(Clone, Copy)]
    enum Shape {
        Ring(f32),
        Dot(f32),
        Plus(f32),
        Square(f32),
        Heart(f32),
        Wave(f32),
    }
    fn dist(shape: Shape, x: f32, y: f32) -> (f32, bool) {
        match shape {
            Shape::Ring(r) => ((x.hypot(y) - r).abs(), false),
            Shape::Dot(r) => (x.hypot(y) - r, true),
            Shape::Plus(r) => {
                let a = (x.abs() - r).max(y.abs() - r * 0.22);
                let b = (y.abs() - r).max(x.abs() - r * 0.22);
                (a.min(b), true)
            }
            Shape::Square(r) => {
                let q = (x.abs() - r * 0.7, y.abs() - r * 0.7);
                let out = q.0.max(0.).hypot(q.1.max(0.)) + q.0.max(q.1).min(0.) - r * 0.3;
                (out.abs(), false)
            }
            Shape::Heart(r) => {
                let (x, y) = (x.abs() / r, -y / r + 0.35);
                let d = if y + x > 1. {
                    (x - 0.25).hypot(y - 0.75) - 2f32.sqrt() / 4.
                } else {
                    let a = x.hypot(y - 1.);
                    let m = 0.5 * (x + y).max(0.);
                    a.min((x - m).hypot(y - m)) * (x - y).signum()
                };
                ((d * r).abs(), false)
            }
            Shape::Wave(r) => {
                if x.abs() > r {
                    return (f32::MAX, false);
                }
                ((y - (x / r * std::f32::consts::PI * 2.).sin() * r * 0.25).abs(), false)
            }
        }
    }
    let stroke = 2.2f32;
    let cells = 9;
    let cell = s / cells as f32;
    for gy in 0..cells {
        for gx in 0..cells {
            let cx = (gx as f32 + 0.2 + rnd() * 0.6) * cell;
            let cy = (gy as f32 + 0.2 + rnd() * 0.6) * cell;
            let r = cell * (0.12 + rnd() * 0.12);
            let shape = match (rnd() * 6.) as u32 {
                0 => Shape::Ring(r),
                1 => Shape::Dot(r * 0.35),
                2 => Shape::Plus(r),
                3 => Shape::Square(r),
                4 => Shape::Heart(r),
                _ => Shape::Wave(r * 1.3),
            };
            let reach = r * 1.6 + stroke * 2.;
            let (x0, x1, y0, y1) = ((cx - reach) as i32, (cx + reach) as i32, (cy - reach) as i32, (cy + reach) as i32);
            for py in y0..=y1 {
                for px in x0..=x1 {
                    let (d, filled) = dist(shape, px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
                    let a = if filled { (0.5 - d).clamp(0., 1.) } else { (stroke / 2. + 0.5 - d).clamp(0., 1.) };
                    if a > 0. {
                        let (wx, wy) = (px.rem_euclid(side as i32) as u32, py.rem_euclid(side as i32) as u32);
                        let i = (wy * side + wx) as usize;
                        alpha[i] = alpha[i].max(a);
                    }
                }
            }
        }
    }
    image::RgbaImage::from_fn(side, side, |x, y| image::Rgba([255, 255, 255, (alpha[(y * side + x) as usize] * 14.) as u8]))
}

/// The wallpaper laid out as a grid of tiles covering up to 4K-ish logical pixels.
pub fn wallpaper_layer() -> gpui::AnyElement {
    let Some(path) = wallpaper() else { return div().into_any_element() };
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .bg(theme::rgb(CHAT_BG))
        .child(div().flex().flex_wrap().w(px(WALLPAPER_TILE * 11.)).children((0..11 * 7).map(move |_| gpui::img(path.clone()).size(px(WALLPAPER_TILE)).flex_none())))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    #[test]
    fn wallpaper_tile() {
        let img = super::wallpaper_image();
        assert_eq!(img.width(), (super::WALLPAPER_TILE * 2.) as u32);
        assert!(img.pixels().any(|p| p.0[3] > 0));
        if let Ok(out) = std::env::var("WA_WALLPAPER_OUT") {
            img.save(out).unwrap();
        }
    }
}
