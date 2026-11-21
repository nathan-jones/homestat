//! A word for the current value (0 → Closed, 1 → Open), in the state's
//! colour on a background tinted with it.

use tiny_skia::Pixmap;

use super::{draw_text, fit, fonts, mix, Frame};

/// The label and colour for the current value.
fn current(frame: &Frame) -> (String, Option<[u8; 3]>) {
    let def = &frame.snap.def;
    let Some(&v) = frame.snap.samples.last() else { return ("–".into(), None) };
    if let Some(s) = def.states.iter().find(|s| (s.value - v).abs() < 1e-9) {
        return (s.label.clone(), s.color.map(|c| c.0));
    }
    if def.states.is_empty() && (v == 0.0 || v == 1.0) {
        // Sensible default for binary sensors.
        return if v == 1.0 { ("On".into(), Some(frame.palette.line)) } else { ("Off".into(), None) };
    }
    (frame.value_text(), None)
}

pub(super) fn background(frame: &Frame) -> [u8; 3] {
    match current(frame).1 {
        Some(c) => mix(frame.palette.background, c, 0.16),
        None => frame.palette.background,
    }
}

pub(super) fn draw(pm: &mut Pixmap, frame: &Frame) {
    let (w, h, p) = (frame.w, frame.h, &frame.palette);
    let f = fonts();
    frame.title(pm);
    let (label, rgb) = current(frame);
    let size = fit(&f.bold, &label, h * 0.30, w * 0.88);
    draw_text(pm, &f.bold, size, &label, w / 2.0, h * 0.70, rgb.unwrap_or(p.text));
}
