//! Widget rendering with tiny-skia (shapes) + ab_glyph (text).
//!
//! Layout is proportional to the frame, so the same widget renders at any
//! resolution HomeKit asks for. Each style lives in its own module; this one
//! holds the shared pieces (palette, panel, text, series drawing).

mod bar;
mod gauge;
mod graph;
mod stat;
mod state;

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use super::{Colors, Rgb, Style, Threshold, WidgetSnapshot};

struct Fonts {
    semibold: FontRef<'static>,
    bold: FontRef<'static>,
}

fn fonts() -> &'static Fonts {
    static FONTS: OnceLock<Fonts> = OnceLock::new();
    FONTS.get_or_init(|| Fonts {
        semibold: FontRef::try_from_slice(include_bytes!("../../../assets/fonts/OpenSans-SemiBold.ttf")).unwrap(),
        bold: FontRef::try_from_slice(include_bytes!("../../../assets/fonts/OpenSans-Bold.ttf")).unwrap(),
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
    /// Secondary text (axis labels, min/max).
    muted: [u8; 3],
    /// Unfilled part of gauges and bars, gridlines.
    track: [u8; 3],
    line: [u8; 3],
    fill: [u8; 3],
    warn: [u8; 3],
    /// The big current value: `text`, or the active threshold's colour.
    value: [u8; 3],
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
            muted: mix(background, text, 0.6),
            track: mix(background, text, 0.13),
            line,
            fill: c.fill.map(|c| c.0).unwrap_or_else(|| mix(background, line, 0.14)),
            // Amber reads on dark backgrounds; use a darker one on light ones.
            warn: if light { [0xA8, 0x5D, 0x00] } else { [0xF2, 0xB0, 0x3C] },
            value: text,
        }
    }

    /// Recolours the value and the line (and the default fill) with the
    /// threshold the current value has reached, if any.
    fn with_threshold(mut self, c: &Colors, thresholds: &[Threshold], value: Option<f64>) -> Palette {
        if let Some(t) = value.and_then(|v| active(thresholds, v)) {
            self.line = t.color.0;
            self.value = t.color.0;
            if c.fill.is_none() {
                self.fill = mix(self.background, self.line, 0.14);
            }
        }
        self
    }
}

/// The highest threshold `v` has reached.
fn active(thresholds: &[Threshold], v: f64) -> Option<&Threshold> {
    thresholds.iter().filter(|t| v >= t.from).max_by(|a, b| a.from.total_cmp(&b.from))
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

/// Everything a style needs to draw one frame.
struct Frame<'a> {
    snap: &'a WidgetSnapshot,
    palette: Palette,
    w: f32,
    h: f32,
}

impl Frame<'_> {
    /// Current value, formatted with the widget's unit and decimals.
    fn value_text(&self) -> String {
        self.snap
            .samples
            .last()
            .map(|v| format_value(*v, &self.snap.def.unit, self.snap.def.decimals))
            .unwrap_or_else(|| "–".into())
    }

    /// Scale for gauge/bar: the widget's min/max, else 0–100.
    fn range(&self) -> (f64, f64) {
        let lo = self.snap.def.min.unwrap_or(0.0);
        let hi = self.snap.def.max.unwrap_or(100.0);
        if hi > lo { (lo, hi) } else { (lo, lo + 1.0) }
    }

    /// Threshold bands across `lo..hi` as (start, end, colour) fractions,
    /// for marking the scale of gauges and bars.
    fn bands(&self, lo: f64, hi: f64) -> Vec<(f32, f32, [u8; 3])> {
        let mut ts: Vec<&Threshold> = self.snap.def.thresholds.iter().collect();
        ts.sort_by(|a, b| a.from.total_cmp(&b.from));
        let frac = |v: f64| ((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32;
        ts.iter()
            .enumerate()
            .map(|(i, t)| {
                let end = ts.get(i + 1).map(|n| frac(n.from)).unwrap_or(1.0);
                (frac(t.from), end, t.color.0)
            })
            .filter(|(a, b, _)| b > a)
            .collect()
    }

    /// Centered title at the top, plus the source-problem line if any.
    fn title(&self, pm: &mut Pixmap) {
        let f = fonts();
        let (w, h) = (self.w, self.h);
        let name = &self.snap.def.name;
        let size = fit(&f.semibold, name, h * 0.10, w * 0.88);
        draw_text(pm, &f.semibold, size, name, w / 2.0, h * 0.17, self.palette.text);
        self.status(pm, w / 2.0, h * 0.29, Align::Center);
    }

    /// The source problem ("entity not found", "broker unreachable", ...).
    fn status(&self, pm: &mut Pixmap, x: f32, baseline: f32, align: Align) {
        if let Some(status) = &self.snap.status {
            let f = fonts();
            let status: String = status.chars().take(60).collect();
            let size = fit(&f.semibold, &status, self.h * 0.065, self.w * 0.88);
            draw_text_aligned(pm, &f.semibold, size, &status, x, baseline, self.palette.warn, align);
        }
    }
}

pub fn render(snap: &WidgetSnapshot, width: u32, height: u32) -> Pixmap {
    let mut pm = Pixmap::new(width.max(2), height.max(2)).expect("non-zero size");
    let def = &snap.def;
    let palette = match def.style {
        Style::State => Palette::new(&def.colors),
        _ => Palette::new(&def.colors).with_threshold(&def.colors, &def.thresholds, snap.samples.last().copied()),
    };
    let frame = Frame { snap, palette, w: pm.width() as f32, h: pm.height() as f32 };
    panel(&mut pm, &frame);
    match snap.def.style {
        Style::Stat => stat::draw(&mut pm, &frame),
        Style::Gauge => gauge::draw(&mut pm, &frame),
        Style::Bar => bar::draw(&mut pm, &frame),
        Style::Graph => graph::draw(&mut pm, &frame),
        Style::State => state::draw(&mut pm, &frame),
    }
    pm
}

/// Background (optionally tinted, see `state`) and a subtle border.
fn panel(pm: &mut Pixmap, frame: &Frame) {
    let background = match frame.snap.def.style {
        Style::State => state::background(frame),
        _ => frame.palette.background,
    };
    pm.fill(color(background));
    let (w, h) = (frame.w, frame.h);
    let bw = (h * 0.006).max(1.0);
    if let Some(r) = Rect::from_xywh(bw / 2.0, bw / 2.0, w - bw, h - bw) {
        let path = PathBuilder::from_rect(r);
        let mut paint = Paint::default();
        paint.set_color(color(frame.palette.border));
        let stroke = Stroke { width: bw, ..Default::default() };
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
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

// ---- shapes --------------------------------------------------------------------------

fn paint(rgb: [u8; 3]) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    p.set_color(color(rgb));
    p
}

/// A line chart of `samples` in the box (x, y, w, h), scaled to `lo..hi`.
#[allow(clippy::too_many_arguments)]
fn draw_series(pm: &mut Pixmap, samples: &[f64], palette: &Palette, x: f32, y: f32, w: f32, h: f32, lo: f64, hi: f64, stroke: f32) {
    if samples.len() < 2 {
        return;
    }
    let n = samples.len();
    let px = |i: usize| x + w * i as f32 / (n - 1) as f32;
    let py = |v: f64| y + h - (((v.clamp(lo, hi) - lo) / (hi - lo)) as f32) * h;

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

    if let Some(path) = area.finish() {
        pm.fill_path(&path, &paint(palette.fill), FillRule::Winding, Transform::identity(), None);
    }
    if let Some(path) = line.finish() {
        let stroke = Stroke { width: stroke, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
        pm.stroke_path(&path, &paint(palette.line), &stroke, Transform::identity(), None);
    }
}

/// A circular arc (angles in degrees, 0 = 3 o'clock, clockwise as y grows down).
#[allow(clippy::too_many_arguments)]
fn stroke_arc(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, from: f32, to: f32, width: f32, rgb: [u8; 3], line_cap: LineCap) {
    if (to - from).abs() < 0.01 {
        return;
    }
    let steps = ((to - from).abs() / 2.0).ceil().max(2.0) as usize;
    let mut pb = PathBuilder::new();
    for i in 0..=steps {
        let a = (from + (to - from) * i as f32 / steps as f32).to_radians();
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
    }
    if let Some(path) = pb.finish() {
        let stroke = Stroke { width, line_cap, ..Default::default() };
        pm.stroke_path(&path, &paint(rgb), &stroke, Transform::identity(), None);
    }
}

fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, rgb: [u8; 3]) {
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        pm.fill_rect(r, &paint(rgb), Transform::identity(), None);
    }
}

/// A filled rectangle with fully rounded ends (a "pill").
fn fill_pill(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, rgb: [u8; 3]) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let r = (h / 2.0).min(w / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, &paint(rgb), FillRule::Winding, Transform::identity(), None);
    }
}

// ---- text ------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
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
    draw_text_aligned(pm, font, size, text, center_x, baseline, rgb, Align::Center);
}

/// Draws text whose left edge, center or right edge (per `align`) is at `x`.
#[allow(clippy::too_many_arguments)]
fn draw_text_aligned(pm: &mut Pixmap, font: &FontRef, size: f32, text: &str, x: f32, baseline: f32, rgb: [u8; 3], align: Align) {
    let sf = font.as_scaled(PxScale::from(size));
    let width = text_width(font, size, text);
    let mut x = match align {
        Align::Left => x,
        Align::Center => x - width / 2.0,
        Align::Right => x - width,
    };
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
    fn thresholds_recolour_value() {
        let def: crate::widgets::WidgetDef = serde_json::from_value(serde_json::json!({
            "id": "t", "aid": 2, "name": "CPU",
            "thresholds": [{"from": 95, "color": "#ff0000"}, {"from": 80, "color": "#ffaa00"}]
        }))
        .unwrap();
        let pal = |v: f64| Palette::new(&def.colors).with_threshold(&def.colors, &def.thresholds, Some(v));
        assert_eq!(pal(50.0).value, defaults::TEXT);
        assert_eq!(pal(50.0).line, defaults::LINE);
        assert_eq!(pal(80.0).value, [0xff, 0xaa, 0x00]);
        assert_eq!(pal(99.0).line, [0xff, 0, 0]);
        assert_eq!(pal(99.0).fill, mix(defaults::BACKGROUND, [0xff, 0, 0], 0.14));
        let snap = WidgetSnapshot { def, samples: vec![10.0, 97.0], status: None, version: 0 };
        let f = Frame { palette: Palette::new(&snap.def.colors), snap: &snap, w: 100.0, h: 100.0 };
        assert_eq!(f.bands(0.0, 100.0), vec![(0.8, 0.95, [0xff, 0xaa, 0]), (0.95, 1.0, [0xff, 0, 0])]);
    }

    #[test]
    fn formats_values() {
        assert_eq!(format_value(88.0, "", None), "88");
        assert_eq!(format_value(42.37, "%", None), "42.4%");
        assert_eq!(format_value(12345.0, "", None), "12.3k");
        assert_eq!(format_value(3.0, "", Some(2)), "3.00");
    }
}
