//! A full time-series chart: title and current value on top, y-axis labels
//! and gridlines, auto-scaled to the data unless min/max are set.

use tiny_skia::{Paint, PathBuilder, Pixmap, Stroke, Transform};

use super::{color, draw_series, draw_text, draw_text_aligned, fit, fonts, format_value, text_width, Align, Frame};

pub(super) fn draw(pm: &mut Pixmap, frame: &Frame) {
    let (w, h, p) = (frame.w, frame.h, &frame.palette);
    let f = fonts();
    let def = &frame.snap.def;
    let (left, right) = (w * 0.05, w * 0.95);

    // Header: title on the left, current value on the right.
    let value = frame.value_text();
    let value_size = fit(&f.bold, &value, h * 0.12, w * 0.4);
    let value_w = super::text_width(&f.bold, value_size, &value);
    let title_size = fit(&f.semibold, &def.name, h * 0.085, (right - left - value_w - w * 0.04).max(w * 0.1));
    draw_text_aligned(pm, &f.semibold, title_size, &def.name, left, h * 0.15, p.text, Align::Left);
    draw_text_aligned(pm, &f.bold, value_size, &value, right, h * 0.155, p.text, Align::Right);
    frame.status(pm, left, h * 0.235, Align::Left);

    let samples = &frame.snap.samples;
    let (lo, hi, step) = scale(samples, def.min, def.max);

    // Y-axis labels decide the chart's left edge.
    let ls = h * 0.055;
    let ticks: Vec<f64> = (0..)
        .map(|i| lo + step * i as f64)
        .take_while(|v| *v <= hi + step * 1e-6)
        .take(12)
        .collect();
    let labels: Vec<String> = ticks.iter().map(|v| tick_label(*v, step)).collect();
    let label_w = labels.iter().map(|l| text_width(&f.semibold, ls, l)).fold(0.0, f32::max);
    let (x0, x1) = (left + label_w + w * 0.02, right);
    let (y0, y1) = (h * 0.30, h * 0.90);
    let y_of = |v: f64| y1 - ((v - lo) / (hi - lo)) as f32 * (y1 - y0);

    let mut grid = Paint::default();
    grid.set_color(color(p.track));
    let stroke = Stroke { width: (h * 0.004).max(1.0), ..Default::default() };
    for (v, label) in ticks.iter().zip(&labels) {
        let y = y_of(*v);
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y);
        pb.line_to(x1, y);
        if let Some(path) = pb.finish() {
            pm.stroke_path(&path, &grid, &stroke, Transform::identity(), None);
        }
        draw_text_aligned(pm, &f.semibold, ls, label, x0 - w * 0.015, y + ls * 0.35, p.muted, Align::Right);
    }

    if samples.len() < 2 {
        draw_text(pm, &f.semibold, ls, "collecting data…", (x0 + x1) / 2.0, (y0 + y1) / 2.0, p.muted);
        return;
    }
    draw_series(pm, samples, p, x0, y0, x1 - x0, y1 - y0, lo, hi, (h * 0.012).max(1.5));
}

/// Axis range and gridline step: the widget's min/max where set, otherwise
/// the data's range, padded and rounded out to "nice" numbers.
pub(super) fn scale(samples: &[f64], min: Option<f64>, max: Option<f64>) -> (f64, f64, f64) {
    let data_lo = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let data_hi = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (mut lo, mut hi) = if data_lo.is_finite() { (data_lo, data_hi) } else { (0.0, 1.0) };
    if hi - lo < 1e-9 {
        let pad = if lo.abs() > 1e-9 { lo.abs() * 0.1 } else { 1.0 };
        lo -= pad;
        hi += pad;
    } else {
        let pad = (hi - lo) * 0.1;
        lo -= pad;
        hi += pad;
    }
    lo = min.unwrap_or(lo);
    hi = max.unwrap_or(hi);
    if hi <= lo {
        hi = lo + 1.0;
    }
    let step = nice_step((hi - lo) / 4.0);
    if min.is_none() {
        lo = (lo / step).floor() * step;
    }
    if max.is_none() {
        hi = (hi / step).ceil() * step;
    }
    (lo, hi, step)
}

/// 1, 2, 2.5 or 5 times a power of ten, at least `raw`.
fn nice_step(raw: f64) -> f64 {
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw * 0.999).unwrap_or(10.0 * mag)
}

fn tick_label(v: f64, step: f64) -> String {
    if v.abs() >= 1e4 {
        return format_value(v, "", None);
    }
    // Just enough decimals to tell the ticks apart (step 0.25 → 2).
    let decimals = (0..6).find(|d| ((step * 10f64.powi(*d)).fract()).abs() < 1e-6).unwrap_or(6) as usize;
    let v = if v.abs() < step * 1e-6 { 0.0 } else { v };
    format!("{v:.decimals$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_scales() {
        let (lo, hi, step) = scale(&[19.2, 21.7, 20.4], None, None);
        assert_eq!(step, 1.0);
        assert!(lo <= 18.95 && lo >= 18.0 && hi >= 21.95 && hi <= 23.0, "{lo} {hi}");
        let (lo, hi, _) = scale(&[5.0, 7.0], Some(0.0), Some(100.0));
        assert_eq!((lo, hi), (0.0, 100.0));
        let (lo, hi, _) = scale(&[], None, None);
        assert!(hi > lo);
        assert_eq!(nice_step(0.3), 0.5);
        assert_eq!(nice_step(23.0), 25.0);
        assert_eq!(tick_label(0.5, 0.25), "0.50");
        assert_eq!(tick_label(1.5, 0.5), "1.5");
        assert_eq!(tick_label(20.0, 1.0), "20");
    }
}
