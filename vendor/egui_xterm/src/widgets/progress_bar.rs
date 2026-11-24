//! Top-edge `OSC 9;4` task progress bar overlay.

use std::time::Duration;

use egui::{Color32, Painter, Rect, Ui, pos2, vec2};

use crate::config::ProgressIndicator;
use crate::layout::{snap_rect, snap_size_at_least_1px};

/// Normalized horizontal `[start, end]` fractions (`0.0..=1.0`) for an `OSC 9;4` progress bar,
/// or `None` when the indicator is hidden (`state == 0`) or out of range (`state > 4`).
#[must_use]
pub fn progress_bar_span(state: u8, progress: u8, now_secs: f64) -> Option<(f32, f32)> {
    match state {
        1 | 4 => Some((0.0, f32::from(progress.min(100)) / 100.0)),
        2 => {
            // `OSC 9;4;2` without an explicit percentage defaults to `progress == 0`; show a full
            // error bar unless a concrete percentage was provided.
            let frac = if progress == 0 {
                1.0
            } else {
                f32::from(progress.min(100)) / 100.0
            };
            Some((0.0, frac))
        }
        3 => {
            let seg_w = 0.25_f32;
            let phase = if now_secs.is_finite() {
                (now_secs * 0.8).rem_euclid(1.0) as f32
            } else {
                0.0
            };
            let start = phase * (1.0 - seg_w);
            Some((start, start + seg_w))
        }
        _ => None,
    }
}

/// Paints a subtle, pixel-snapped top-edge `OSC 9;4` progress bar over `term_rect` clipped to
/// `clip_rect` without shifting layout coordinates.
///
/// Only state `3` (indeterminate) schedules timed animation repaints (`~60 Hz`); states `0`, `1`,
/// `2`, and `4` schedule zero idle repaints.
#[expect(clippy::too_many_arguments, reason = "overlay progress bar parameters")]
pub fn paint_progress_bar(
    ui: &Ui,
    painter: &Painter,
    term_rect: Rect,
    clip_rect: Rect,
    ppp: f32,
    indicator: ProgressIndicator,
    default_fg: Color32,
    now_secs: f64,
) {
    let Some((start_frac, end_frac)) =
        progress_bar_span(indicator.state, indicator.progress, now_secs)
    else {
        return;
    };

    let bar_h = snap_size_at_least_1px(3.0, ppp);
    let raw_track =
        Rect::from_min_size(term_rect.min, vec2(term_rect.width(), bar_h)).intersect(clip_rect);
    if !raw_track.is_positive() {
        return;
    }

    let track_rect = snap_rect(raw_track, ppp).intersect(clip_rect);
    if !track_rect.is_positive() {
        return;
    }

    let track_color =
        Color32::from_rgba_unmultiplied(default_fg.r(), default_fg.g(), default_fg.b(), 28);
    painter.rect_filled(track_rect, 0.0, track_color);

    let fill_color = match indicator.state {
        1 | 3 => Color32::from_rgb(66, 165, 245),
        2 => Color32::from_rgb(239, 83, 80),
        4 => Color32::from_rgb(255, 183, 77),
        _ => return,
    };

    let track_w = track_rect.width();
    let x0 = track_rect.min.x + track_w * start_frac.clamp(0.0, 1.0);
    let x1 = track_rect.min.x + track_w * end_frac.clamp(0.0, 1.0);
    if x1 > x0 {
        let fill_rect = snap_rect(
            Rect::from_min_max(pos2(x0, track_rect.min.y), pos2(x1, track_rect.max.y)),
            ppp,
        )
        .intersect(track_rect);
        if fill_rect.is_positive() {
            painter.rect_filled(fill_rect, 0.0, fill_color);
        }
    }

    if indicator.state == 3 {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use egui::{CentralPanel, Context, Frame, Pos2, RawInput};

    use super::*;

    #[test]
    fn progress_bar_span_covers_all_states() {
        assert_eq!(progress_bar_span(0, 50, 0.0), None);
        assert_eq!(progress_bar_span(5, 50, 0.0), None);
        assert_eq!(progress_bar_span(1, 0, 0.0), Some((0.0, 0.0)));
        assert_eq!(progress_bar_span(1, 40, 0.0), Some((0.0, 0.4)));
        assert_eq!(progress_bar_span(1, 200, 0.0), Some((0.0, 1.0)));
        assert_eq!(progress_bar_span(2, 0, 0.0), Some((0.0, 1.0)));
        assert_eq!(progress_bar_span(2, 65, 0.0), Some((0.0, 0.65)));
        assert_eq!(progress_bar_span(4, 75, 0.0), Some((0.0, 0.75)));

        let indet_0 = progress_bar_span(3, 0, 0.0).expect("indeterminate span");
        assert!((indet_0.0 - 0.0).abs() < 1e-6);
        assert!((indet_0.1 - 0.25).abs() < 1e-6);

        let indet_half = progress_bar_span(3, 0, 0.625).expect("indeterminate span");
        assert!((indet_half.0 - 0.375).abs() < 1e-5);
        assert!((indet_half.1 - 0.625).abs() < 1e-5);

        let indet_nan = progress_bar_span(3, 0, f64::NAN).expect("nan safe");
        assert_eq!(indet_nan, (0.0, 0.25));
    }

    #[test]
    fn paint_progress_bar_schedules_repaint_only_for_indeterminate() {
        let ctx = Context::default();
        let (tx, rx) = mpsc::channel();
        ctx.set_repaint_observer(move |info| tx.send(info.delay).expect("send delay"));

        let rect = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
        let run_state = |indicator: ProgressIndicator| {
            while rx.try_recv().is_ok() {}
            let raw = RawInput {
                screen_rect: Some(rect),
                time: Some(0.25),
                ..Default::default()
            };
            let out = ctx.run_ui(raw, |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let painter = ui.painter_at(rect);
                    paint_progress_bar(
                        ui,
                        &painter,
                        rect,
                        rect,
                        2.0,
                        indicator,
                        Color32::WHITE,
                        0.25,
                    );
                });
            });
            let delays: Vec<Duration> = rx.try_iter().collect();
            let bar_shapes: Vec<Rect> = out
                .shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::Shape::Rect(r) if r.rect.height() <= 4.0 && r.rect.min.y == 0.0 => {
                        Some(r.rect)
                    }
                    _ => None,
                })
                .collect();
            out.drop_without_applying_deltas();
            (bar_shapes, delays)
        };

        let (shapes_hidden, delays_hidden) = run_state(ProgressIndicator {
            state: 0,
            progress: 50,
        });
        assert!(shapes_hidden.is_empty());
        assert!(!delays_hidden.iter().any(|d| *d > Duration::ZERO));

        let (shapes_normal, delays_normal) = run_state(ProgressIndicator {
            state: 1,
            progress: 50,
        });
        assert_eq!(shapes_normal.len(), 2);
        assert!((shapes_normal[1].width() - 200.0).abs() <= 0.5);
        assert!(!delays_normal.iter().any(|d| *d > Duration::ZERO));

        let (shapes_err, delays_err) = run_state(ProgressIndicator {
            state: 2,
            progress: 0,
        });
        assert_eq!(shapes_err.len(), 2);
        assert!((shapes_err[1].width() - 400.0).abs() <= 0.5);
        assert!(!delays_err.iter().any(|d| *d > Duration::ZERO));

        let (shapes_paused, delays_paused) = run_state(ProgressIndicator {
            state: 4,
            progress: 30,
        });
        assert_eq!(shapes_paused.len(), 2);
        assert!((shapes_paused[1].width() - 120.0).abs() <= 0.5);
        assert!(!delays_paused.iter().any(|d| *d > Duration::ZERO));

        let (shapes_indet, delays_indet) = run_state(ProgressIndicator {
            state: 3,
            progress: 0,
        });
        assert_eq!(shapes_indet.len(), 2);
        assert!((shapes_indet[1].width() - 100.0).abs() <= 0.5);
        assert!(
            delays_indet
                .iter()
                .any(|d| *d > Duration::ZERO && *d <= Duration::from_millis(20))
        );
    }
}
