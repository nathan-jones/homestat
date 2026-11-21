//! Widget rendering with tiny-skia (shapes) + ab_glyph (text).
//!
//! Layout is proportional to the frame, so the same widget renders at any
//! resolution HomeKit asks for. New styles plug in via `render`.

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use super::{Colors, Rgb, Style, WidgetSnapshot};

struct Fonts {
    semibold: FontRef<'static>,
    bold: FontRef<'static>,
}

fn fonts() -> &'static Fonts {
    static FONTS: OnceLock<Fonts> = OnceLock::new();
    FONTS.get_or_init(|| Fonts {
        semibold: FontRef::try_from_slice(include_bytes!("../../assets/fonts/OpenSans-SemiBold.ttf")).unwrap(),
        bold: FontRef::try_from_slice(include_bytes!("../../assets/fonts/OpenSans-Bold.ttf")).unwrap(),
    })
}

/// Default theme (Grafana-like dark panel, blue sparkline).
pub mod defaults {
    pub const BACKGROUND: [u8; 3] = [0x21, 0x20, 0x22];
    pub const TEXT: [u8; 3] = [0xD8, 0xD9, 0xDA];
    pub const LINE: [u8; 3] = [0x4F, 0x8F, 0xE0];
}

/// Resolved colours for one render.
struct Palette {
    background: [u8; 3],
    border: [u8; 3],
    text: [u8; 3],
    line: [u8; 3],
    fill: [u8; 3],
    warn: [u8; 3],
}

impl Palette {
    fn new(c: &Colors) -> Palette {
        let pick = |c: Option<Rgb>, d: [u8; 3]| c.map(|c| c.0).unwrap_or(d);
        let background = pick(c.background, defaults::BACKGROUND);
        let text = pick(c.text, defaults::TEXT);
        let line = pick(c.line, defaults::LINE);
        let light = luminance(background) > 0.5;
        Palette {
            background,
            border: mix(background, text, 0.08),
            text,
            line,
            fill: c.fill.map(|c| c.0).unwrap_or_else(|| mix(background, line, 0.14)),
            // Amber reads on dark backgrounds; use a darker one on light ones.
            warn: if light { [0xA8, 0x5D, 0x00] } else { [0xF2, 0xB0, 0x3C] },
        }
    }
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    std::array::from_fn(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8)
}

/// Relative luminance (sRGB), 0 = black, 1 = white.
fn luminance(c: [u8; 3]) -> f32 {
    let ch = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2])
}

fn color([r, g, b]: [u8; 3]) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

pub fn render(snap: &WidgetSnapshot, width: u32, height: u32) -> Pixmap {
    let mut pm = Pixmap::new(width.max(2), height.max(2)).expect("non-zero size");
    match snap.def.style {
        Style::Stat => render_stat(&mut pm, snap),
    }
    pm
}

pub fn render_png(snap: &WidgetSnapshot, width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    Ok(render(snap, width, height).encode_png()?)
}

pub fn render_jpeg(snap: &WidgetSnapshot, width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let pm = render(snap, width, height);
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 85).encode(
        pm.data(),
        pm.width() as u16,
        pm.height() as u16,
        jpeg_encoder::ColorType::Rgba,
    )?;
    Ok(out)
}

fn render_stat(pm: &mut Pixmap, snap: &WidgetSnapshot) {
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    let palette = Palette::new(&snap.def.colors);
    pm.fill(color(palette.background));

    // Subtle panel border.
    let bw = (h * 0.006).max(1.0);
    if let Some(r) = Rect::from_xywh(bw / 2.0, bw / 2.0, w - bw, h - bw) {
        let path = PathBuilder::from_rect(r);
        let mut paint = Paint::default();
        paint.set_color(color(palette.border));
        let stroke = Stroke { width: bw, ..Default::default() };
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    let f = fonts();
    let max_text_w = w * 0.88;

    // Title
    let title_size = fit(&f.semibold, &snap.def.name, h * 0.10, max_text_w);
    draw_text(pm, &f.semibold, title_size, &snap.def.name, w / 2.0, h * 0.17, palette.text);

    // Source problem ("entity not found", "broker unreachable", ...)
    if let Some(status) = &snap.status {
        let status: String = status.chars().take(60).collect();
        let size = fit(&f.semibold, &status, h * 0.065, max_text_w);
        draw_text(pm, &f.semibold, size, &status, w / 2.0, h * 0.29, palette.warn);
    }

    // Big value
    let value = snap.samples.last().map(|v| format_value(*v, &snap.def.unit, snap.def.decimals));
    let value = value.as_deref().unwrap_or("–");
    let value_size = fit(&f.bold, value, h * 0.24, max_text_w);
    draw_text(pm, &f.bold, value_size, value, w / 2.0, h * 0.62, palette.text);

    // Sparkline
    draw_sparkline(pm, &snap.samples, &palette, w * 0.075, h * 0.70, w * 0.85, h * 0.235);
}

fn draw_sparkline(pm: &mut Pixmap, samples: &[f64], palette: &Palette, x: f32, y: f32, w: f32, h: f32) {
    if samples.len() < 2 {
        return;
    }
    let max = samples.iter().copied().fold(f64::MIN, f64::max);
    let min = samples.iter().copied().fold(f64::MAX, f64::min);
    // Zero-based like Grafana's sparklines, with headroom above the max.
    let lo = min.min(0.0);
    let hi = if max > lo { lo + (max - lo) * 1.15 } else { lo + 1.0 };
    let n = samples.len();
    let px = |i: usize| x + w * i as f32 / (n - 1) as f32;
    let py = |v: f64| y + h - (((v - lo) / (hi - lo)) as f32) * h;

    let mut line = PathBuilder::new();
    let mut area = PathBuilder::new();
    area.move_to(px(0), y + h);
    for (i, v) in samples.iter().enumerate() {
        let (sx, sy) = (px(i), py(*v));
        if i == 0 {
            line.move_to(sx, sy);
        } else {
            line.line_to(sx, sy);
        }
        area.line_to(sx, sy);
    }
    area.line_to(px(n - 1), y + h);
    area.close();

    let mut paint = Paint::default();
    paint.anti_alias = true;
    if let Some(path) = area.finish() {
        paint.set_color(color(palette.fill));
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
    if let Some(path) = line.finish() {
        paint.set_color(color(palette.line));
        let stroke = Stroke { width: (h * 0.025).max(1.5), ..Default::default() };
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

pub fn format_value(v: f64, unit: &str, decimals: Option<u8>) -> String {
    let (scaled, suffix) = match v.abs() {
        a if a >= 1e9 => (v / 1e9, "B"),
        a if a >= 1e6 => (v / 1e6, "M"),
        a if a >= 1e4 => (v / 1e3, "k"),
        _ => (v, ""),
    };
    let decimals = decimals.map(usize::from).unwrap_or(
        if !suffix.is_empty() || (scaled.fract().abs() > 1e-9 && scaled.abs() < 100.0) { 1 } else { 0 },
    );
    format!("{scaled:.decimals$}{suffix}{unit}")
}

fn text_width(font: &FontRef, size: f32, text: &str) -> f32 {
    let sf = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            width += sf.kern(p, id);
        }
        width += sf.h_advance(id);
        prev = Some(id);
    }
    width
}

/// Shrinks `size` until `text` fits within `max_w`.
fn fit(font: &FontRef, text: &str, size: f32, max_w: f32) -> f32 {
    let w = text_width(font, size, text);
    if w > max_w { size * max_w / w } else { size }
}

fn draw_text(pm: &mut Pixmap, font: &FontRef, size: f32, text: &str, center_x: f32, baseline: f32, rgb: [u8; 3]) {
    let sf = font.as_scaled(PxScale::from(size));
    let mut x = center_x - text_width(font, size, text) / 2.0;
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let stride = pm.width() as usize * 4;
    let data = pm.data_mut();
    let mut prev = None;
    for c in text.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        let glyph = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(x, baseline));
        if let Some(outline) = font.outline_glyph(glyph) {
            let b = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let px = b.min.x as i32 + gx as i32;
                let py = b.min.y as i32 + gy as i32;
                if px < 0 || py < 0 || px >= pw || py >= ph {
                    return;
                }
                let i = py as usize * stride + px as usize * 4;
                let a = cov.clamp(0.0, 1.0);
                for k in 0..3 {
                    data[i + k] = (rgb[k] as f32 * a + data[i + k] as f32 * (1.0 - a)).round() as u8;
                }
            });
        }
        x += sf.h_advance(id);
        prev = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_status_line() {
        let def: crate::widgets::WidgetDef =
            serde_json::from_value(serde_json::json!({"id": "t", "aid": 2, "name": "Temp"})).unwrap();
        let mut snap = WidgetSnapshot { def, samples: vec![1.0, 2.0], status: None, version: 0 };
        let plain = render(&snap, 320, 180);
        snap.status = Some("entity not found".into());
        assert_ne!(plain.data(), render(&snap, 320, 180).data());
    }

    #[test]
    fn applies_colors() {
        let def: crate::widgets::WidgetDef = serde_json::from_value(serde_json::json!({
            "id": "t", "aid": 2, "name": "T",
            "colors": {"background": "#fff", "text": "#102030", "line": "#e5484d"}
        }))
        .unwrap();
        assert_eq!(def.colors.background, Some(Rgb([255, 255, 255])));
        let snap = WidgetSnapshot { def, samples: vec![1.0, 5.0, 2.0], status: None, version: 0 };
        let pm = render(&snap, 320, 180);
        // Background corner pixel (inside the border).
        let px = pm.pixel(10, 10).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (255, 255, 255));
        let p = Palette::new(&snap.def.colors);
        assert_eq!(p.fill, mix([255, 255, 255], [0xe5, 0x48, 0x4d], 0.14));
        assert_eq!(p.warn, [0xA8, 0x5D, 0x00], "dark warning colour on light background");
        assert!(serde_json::from_value::<crate::widgets::Colors>(serde_json::json!({"line": "blue"})).is_err());
    }

    #[test]
    fn default_colors_unchanged_in_storage() {
        let def: crate::widgets::WidgetDef =
            serde_json::from_value(serde_json::json!({"id": "t", "aid": 2, "name": "T"})).unwrap();
        assert!(serde_json::to_value(&def).unwrap().get("colors").is_none());
    }

    #[test]
    fn formats_values() {
        assert_eq!(format_value(88.0, "", None), "88");
        assert_eq!(format_value(42.37, "%", None), "42.4%");
        assert_eq!(format_value(12345.0, "", None), "12.3k");
        assert_eq!(format_value(3.0, "", Some(2)), "3.00");
    }
}
