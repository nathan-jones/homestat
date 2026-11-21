//! Title, big current value, sparkline (Grafana-style "singlestat").

use tiny_skia::Pixmap;

use super::{draw_series, draw_text, fit, fonts, Frame};

pub(super) fn draw(pm: &mut Pixmap, frame: &Frame) {
    let (w, h, p) = (frame.w, frame.h, &frame.palette);
    let f = fonts();
    frame.title(pm);

    let value = frame.value_text();
    let size = fit(&f.bold, &value, h * 0.24, w * 0.88);
    draw_text(pm, &f.bold, size, &value, w / 2.0, h * 0.62, p.value);

    // Zero-based like Grafana's sparklines, with headroom above the max.
    let samples = &frame.snap.samples;
    let max = samples.iter().copied().fold(f64::MIN, f64::max);
    let min = samples.iter().copied().fold(f64::MAX, f64::min);
    let lo = min.min(0.0);
    let hi = if max > lo { lo + (max - lo) * 1.15 } else { lo + 1.0 };
    let (sx, sy, sw, sh) = (w * 0.075, h * 0.70, w * 0.85, h * 0.235);
    draw_series(pm, samples, p, sx, sy, sw, sh, lo, hi, (sh * 0.025).max(1.5));
}
