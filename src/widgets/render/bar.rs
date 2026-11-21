//! Big value over a horizontal bar between min and max.

use tiny_skia::Pixmap;

use super::{draw_text, fill_rect, mix, draw_text_aligned, fill_pill, fit, fonts, format_value, Align, Frame};

pub(super) fn draw(pm: &mut Pixmap, frame: &Frame) {
    let (w, h, p) = (frame.w, frame.h, &frame.palette);
    let f = fonts();
    frame.title(pm);

    let value = frame.value_text();
    let size = fit(&f.bold, &value, h * 0.24, w * 0.88);
    draw_text(pm, &f.bold, size, &value, w / 2.0, h * 0.60, p.value);

    let (bx, by, bw, bh) = (w * 0.075, h * 0.69, w * 0.85, h * 0.085);
    fill_pill(pm, bx, by, bw, bh, p.track);
    let (lo, hi) = frame.range();
    if let Some(v) = frame.snap.samples.last() {
        let frac = ((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32;
        if frac > 0.0 {
            fill_pill(pm, bx, by, (bw * frac).max(bh), bh, p.line);
        }
    }

    // Threshold zones as a thin strip just under the bar.
    for (a, b, c) in frame.bands(lo, hi) {
        let (x0, x1) = (bx + bw * a, bx + bw * b);
        fill_rect(pm, x0, by + bh * 1.35, x1 - x0, (bh * 0.22).max(1.5), mix(p.background, c, 0.8));
    }

    let ls = h * 0.06;
    let unit = &frame.snap.def.unit;
    draw_text_aligned(pm, &f.semibold, ls, &format_value(lo, unit, None), bx, h * 0.89, p.muted, Align::Left);
    draw_text_aligned(pm, &f.semibold, ls, &format_value(hi, unit, None), bx + bw, h * 0.89, p.muted, Align::Right);
}
