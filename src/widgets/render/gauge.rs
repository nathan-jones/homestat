//! Semicircular dial between min and max.

use tiny_skia::Pixmap;

use super::{draw_text, fit, fonts, format_value, stroke_arc, Align, Frame};

pub(super) fn draw(pm: &mut Pixmap, frame: &Frame) {
    let (w, h, p) = (frame.w, frame.h, &frame.palette);
    let f = fonts();
    let name = &frame.snap.def.name;
    let size = fit(&f.semibold, name, h * 0.09, w * 0.88);
    draw_text(pm, &f.semibold, size, name, w / 2.0, h * 0.15, p.text);
    frame.status(pm, w / 2.0, h * 0.245, Align::Center);

    let (cx, cy) = (w / 2.0, h * 0.78);
    let r = (h * 0.45).min(w * 0.40);
    let thick = r * 0.21;
    stroke_arc(pm, cx, cy, r, 180.0, 360.0, thick, p.track);
    let (lo, hi) = frame.range();
    if let Some(v) = frame.snap.samples.last() {
        let frac = ((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32;
        // Keep a sliver visible at the minimum so the dial never looks broken.
        stroke_arc(pm, cx, cy, r, 180.0, 180.0 + (180.0 * frac).max(0.5), thick, p.line);
    }

    let value = frame.value_text();
    let size = fit(&f.bold, &value, r * 0.48, (r - thick) * 1.7);
    draw_text(pm, &f.bold, size, &value, cx, cy, p.text);

    // Scale ends, under the arc.
    let label = |v: f64| format_value(v, "", None);
    let ls = h * 0.06;
    let below = cy + thick / 2.0 + ls * 1.25;
    draw_text(pm, &f.semibold, ls, &label(lo), cx - r, below, p.muted);
    draw_text(pm, &f.semibold, ls, &label(hi), cx + r, below, p.muted);
}
