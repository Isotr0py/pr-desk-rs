//! Frame-aware auto-layout, DPI pixel snapping, font-size auto-scaling, and debounced
//! backend resize propagation.

use egui::{
    Event, FontFamily, FontId, Key, Pos2, Rect, Role, Sense, Ui, Vec2, WidgetInfo, pos2, vec2,
};
use egui_xterm_core::{Terminal, TerminalInput, TerminalIoError, TerminalSize};

use crate::config::{ScaleMode, TerminalConfig};

const MIN_FONT_SIZE: f32 = 1.0;
const MAX_FONT_SIZE: f32 = 128.0;
const FALLBACK_COLS: f32 = 80.0;
const FALLBACK_ROWS: f32 = 24.0;
const MAX_DEBOUNCE_SECS: f64 = 86_400.0;

/// Snaps a scalar coordinate to the nearest physical pixel boundary for `pixels_per_point`.
#[inline]
#[must_use]
pub(crate) fn snap_px(value: f32, pixels_per_point: f32) -> f32 {
    let ppp = sanitize_ppp(pixels_per_point);
    if !value.is_finite() {
        return 0.0;
    }
    (value * ppp).round() / ppp
}

/// Snaps a scalar coordinate upward (inward from min edge) to a physical pixel boundary.
#[inline]
#[must_use]
pub(crate) fn ceil_px(value: f32, pixels_per_point: f32) -> f32 {
    let ppp = sanitize_ppp(pixels_per_point);
    if !value.is_finite() {
        return 0.0;
    }
    (value * ppp).ceil() / ppp
}

/// Snaps a scalar coordinate downward (inward from max edge) to a physical pixel boundary.
#[inline]
#[must_use]
pub(crate) fn floor_px(value: f32, pixels_per_point: f32) -> f32 {
    let ppp = sanitize_ppp(pixels_per_point);
    if !value.is_finite() {
        return 0.0;
    }
    (value * ppp).floor() / ppp
}

/// Snaps a non-negative thickness/dimension to at least one physical pixel (`>= 1.0 / ppp`).
#[inline]
#[must_use]
pub(crate) fn snap_size_at_least_1px(value: f32, pixels_per_point: f32) -> f32 {
    let ppp = sanitize_ppp(pixels_per_point);
    let safe = if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0 / ppp
    };
    (safe * ppp).round().max(1.0) / ppp
}

/// Snaps all four edges of `rect` to physical pixels while ensuring `min <= max`.
#[inline]
#[must_use]
pub(crate) fn snap_rect(rect: Rect, pixels_per_point: f32) -> Rect {
    let min_x = snap_px(rect.min.x, pixels_per_point);
    let min_y = snap_px(rect.min.y, pixels_per_point);
    let max_x = snap_px(rect.max.x, pixels_per_point).max(min_x);
    let max_y = snap_px(rect.max.y, pixels_per_point).max(min_y);
    Rect::from_min_max(pos2(min_x, min_y), pos2(max_x, max_y))
}

#[inline]
#[must_use]
pub(crate) fn sanitize_ppp(pixels_per_point: f32) -> f32 {
    if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellMeasureKey {
    font_size_bits: u32,
    ppp_bits: u32,
}

#[derive(Debug, Clone, Copy)]
struct CellMeasureCache {
    key: CellMeasureKey,
    raw_w: f32,
    cell_size: Vec2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutoScaleCacheKey {
    inner_w_bits: u32,
    inner_h_bits: u32,
    target_cols: u16,
    target_rows: u16,
    ppp_bits: u32,
}

#[derive(Debug, Clone, Copy)]
struct AutoScaleCache {
    key: AutoScaleCacheKey,
    effective_font_size: f32,
    raw_cell_w: f32,
    cell_size: Vec2,
}

/// Persistent per-runtime layout and debounced backend resize state.
#[derive(Debug, Default)]
pub(crate) struct TerminalLayoutState {
    last_sent_size: Option<TerminalSize>,
    pending_resize: Option<(TerminalSize, f64)>,
    cell_measure_cache: Option<CellMeasureCache>,
    auto_scale_cache: Option<AutoScaleCache>,
}

/// Result of laying out and allocating the terminal widget for one frame.
pub(crate) struct FrameLayout {
    pub response: egui::Response,
    pub term_rect: Rect,
    pub clip_rect: Rect,
    pub cols: u16,
    pub rows: u16,
    pub cell_size: Vec2,
    pub raw_cell_w: f32,
    pub effective_font_size: f32,
    pub pixels_per_point: f32,
    pub resized_this_frame: bool,
    pub io_error: Option<TerminalIoError>,
}

fn measure_cell(ctx: &egui::Context, font_size: f32, ppp: f32) -> (f32, Vec2) {
    let font_id = FontId::new(font_size, FontFamily::Monospace);
    let (raw_w, raw_h) = ctx.fonts_mut(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
    let fallback_w = (font_size * 0.6).max(1.0);
    let fallback_h = (font_size * 1.2).max(1.0);
    let raw_w = if raw_w.is_finite() && raw_w > 0.0 {
        raw_w
    } else {
        fallback_w
    };
    let raw_h = if raw_h.is_finite() && raw_h > 0.0 {
        raw_h
    } else {
        fallback_h
    };
    let cell_w = snap_size_at_least_1px(raw_w, ppp);
    let cell_h = snap_size_at_least_1px(raw_h, ppp);
    (raw_w, vec2(cell_w, cell_h))
}

impl TerminalLayoutState {
    fn measure_cell_cached(
        &mut self,
        ctx: &egui::Context,
        font_size: f32,
        ppp: f32,
    ) -> (f32, Vec2) {
        let key = CellMeasureKey {
            font_size_bits: font_size.to_bits(),
            ppp_bits: ppp.to_bits(),
        };
        if let Some(cached) = self.cell_measure_cache
            && cached.key == key
        {
            return (cached.raw_w, cached.cell_size);
        }
        let (raw_w, cell_size) = measure_cell(ctx, font_size, ppp);
        self.cell_measure_cache = Some(CellMeasureCache {
            key,
            raw_w,
            cell_size,
        });
        (raw_w, cell_size)
    }

    /// Computes grid dimensions, allocates space using stable `id_salt`, updates `terminal.resize`,
    /// and debounces `input.resize`.
    #[expect(clippy::too_many_arguments, reason = "per-frame layout coordinator")]
    pub(crate) fn layout_and_resize<I: TerminalInput>(
        &mut self,
        ui: &mut Ui,
        id_salt: u64,
        config: &TerminalConfig,
        terminal: &mut Terminal,
        input: &mut I,
        now_secs: f64,
        pending_focus_lock_settle: &mut bool,
        search_bar_blocks_focus: bool,
        prev_search_bar_rect: Option<Rect>,
    ) -> FrameLayout {
        let ppp = sanitize_ppp(ui.ctx().pixels_per_point());
        let base_font = if config.font_size.is_finite() && config.font_size > 0.0 {
            config.font_size
        } else {
            14.0
        };
        let zoom = if config.zoom_factor.is_finite() && config.zoom_factor > 0.0 {
            config.zoom_factor
        } else {
            1.0
        };

        let avail = ui.available_rect_before_wrap();
        let origin = Pos2::new(
            ceil_px(
                if avail.min.x.is_finite() {
                    avail.min.x
                } else {
                    0.0
                },
                ppp,
            ),
            ceil_px(
                if avail.min.y.is_finite() {
                    avail.min.y
                } else {
                    0.0
                },
                ppp,
            ),
        );

        let base_effective = (base_font * zoom).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        let finite_bounds = avail.min.x.is_finite()
            && avail.max.x.is_finite()
            && avail.min.y.is_finite()
            && avail.max.y.is_finite();

        let (avail_w, avail_h) = if finite_bounds {
            (
                (floor_px(avail.max.x, ppp) - origin.x).max(0.0),
                (floor_px(avail.max.y, ppp) - origin.y).max(0.0),
            )
        } else {
            let (_, fallback_cell) = self.measure_cell_cached(ui.ctx(), base_effective, ppp);
            let w = if avail.min.x.is_finite() && avail.max.x.is_finite() {
                (floor_px(avail.max.x, ppp) - origin.x).max(0.0)
            } else {
                FALLBACK_COLS * fallback_cell.x
            };
            let h = if avail.min.y.is_finite() && avail.max.y.is_finite() {
                (floor_px(avail.max.y, ppp) - origin.y).max(0.0)
            } else {
                FALLBACK_ROWS * fallback_cell.y
            };
            (w, h)
        };

        let (cols, rows, effective_font_size, raw_cell_w, cell_size) = match config.scale_mode {
            ScaleMode::ReflowGrid => {
                let (base_raw_w, base_cell) =
                    self.measure_cell_cached(ui.ctx(), base_effective, ppp);
                let cols_f = (avail_w / base_cell.x).floor();
                let rows_f = (avail_h / base_cell.y).floor();
                let cols = if cols_f.is_finite() && cols_f >= 1.0 {
                    cols_f.min(f32::from(u16::MAX)) as u16
                } else {
                    1
                };
                let rows = if rows_f.is_finite() && rows_f >= 1.0 {
                    rows_f.min(f32::from(u16::MAX)) as u16
                } else {
                    1
                };
                (cols, rows, base_effective, base_raw_w, base_cell)
            }
            ScaleMode::AutoScaleToFit { cols, rows } => {
                let target_cols = cols.max(1);
                let target_rows = rows.max(1);
                let key = AutoScaleCacheKey {
                    inner_w_bits: avail_w.to_bits(),
                    inner_h_bits: avail_h.to_bits(),
                    target_cols,
                    target_rows,
                    ppp_bits: ppp.to_bits(),
                };
                if let Some(cached) = self.auto_scale_cache
                    && cached.key == key
                {
                    (
                        target_cols,
                        target_rows,
                        cached.effective_font_size,
                        cached.raw_cell_w,
                        cached.cell_size,
                    )
                } else {
                    let (solved_font, solved_raw_w, solved_cell) = solve_auto_scale_font(
                        ui.ctx(),
                        avail_w,
                        avail_h,
                        target_cols,
                        target_rows,
                        ppp,
                    );
                    self.auto_scale_cache = Some(AutoScaleCache {
                        key,
                        effective_font_size: solved_font,
                        raw_cell_w: solved_raw_w,
                        cell_size: solved_cell,
                    });
                    (
                        target_cols,
                        target_rows,
                        solved_font,
                        solved_raw_w,
                        solved_cell,
                    )
                }
            }
        };

        let grid_size = vec2(f32::from(cols) * cell_size.x, f32::from(rows) * cell_size.y);
        let term_rect = Rect::from_min_size(origin, grid_size);

        let finite_avail = Rect::from_min_size(origin, vec2(avail_w, avail_h));
        let mut clip_rect = finite_avail.intersect(ui.clip_rect());
        if avail.min.x.is_finite() && avail.max.x.is_finite() {
            clip_rect.min.x = clip_rect.min.x.max(avail.min.x);
            clip_rect.max.x = clip_rect.max.x.min(avail.max.x);
            clip_rect.min.x = clip_rect.min.x.min(clip_rect.max.x);
        }
        if avail.min.y.is_finite() && avail.max.y.is_finite() {
            clip_rect.min.y = clip_rect.min.y.max(avail.min.y);
            clip_rect.max.y = clip_rect.max.y.min(avail.max.y);
            clip_rect.min.y = clip_rect.min.y.min(clip_rect.max.y);
        }

        #[cfg(feature = "search")]
        let (search_bar_blocks_focus, prev_search_bar_rect) =
            if crate::widgets::search_bar::compute_bar_rect(term_rect, clip_rect, false).is_some() {
                (search_bar_blocks_focus, prev_search_bar_rect)
            } else {
                (false, None)
            };

        // Allocate layout space inside the parent Ui using a bounded size so tiny containers don't
        // unexpectedly push siblings, while keeping `response.rect == term_rect` (cell (0,0) at min).
        let space_size = vec2(
            grid_size.x.min(avail_w.max(0.0)),
            grid_size.y.min(avail_h.max(0.0)),
        );
        ui.allocate_space(space_size);

        let id = ui.make_persistent_id(id_salt);
        let clamped_interact = term_rect.intersect(clip_rect);
        let interact_rect = if clamped_interact.is_positive() {
            clamped_interact
        } else {
            Rect::from_min_size(origin, Vec2::ZERO)
        };
        // Protect Frame 2 immediate Escape/Tab/Arrows from egui's 1-frame focus-lock gap:
        // `Memory::set_focus_lock_filter` requires `had_focus_last_frame(id)`, so Frame 1
        // clicks cannot lock the filter before Frame 2's `Focus::begin_pass`.
        if *pending_focus_lock_settle
            && !search_bar_blocks_focus
            && ui.memory(|m| m.had_focus_last_frame(id))
        {
            let (outside_pointer_press, has_escape, has_nav_key) = ui.input(|i| {
                let mut outside = false;
                let mut esc = false;
                let mut nav = false;
                for ev in &i.events {
                    match ev {
                        Event::PointerButton {
                            pos, pressed: true, ..
                        } if !(interact_rect.is_positive() && interact_rect.contains(*pos))
                            || prev_search_bar_rect.is_some_and(|r| r.contains(*pos)) =>
                        {
                            outside = true;
                        }
                        Event::Key {
                            key: Key::Escape,
                            pressed: true,
                            modifiers,
                            ..
                        } if !modifiers.any() => {
                            esc = true;
                            nav = true;
                        }
                        Event::Key {
                            key:
                                Key::Tab
                                | Key::ArrowUp
                                | Key::ArrowDown
                                | Key::ArrowLeft
                                | Key::ArrowRight,
                            pressed: true,
                            ..
                        } => {
                            nav = true;
                        }
                        _ => {}
                    }
                }
                (outside, esc, nav)
            });

            if outside_pointer_press {
                *pending_focus_lock_settle = false;
            } else if has_nav_key {
                let filter = crate::input_map::TERMINAL_FOCUS_FILTER;
                ui.memory_mut(|m| {
                    let curr = m.focused();
                    if curr.is_none() && has_escape {
                        m.request_focus(id);
                        m.move_focus(egui::FocusDirection::None);
                        m.set_focus_lock_filter(id, filter);
                    } else if curr == Some(id) {
                        m.move_focus(egui::FocusDirection::None);
                        m.set_focus_lock_filter(id, filter);
                    }
                });
            }
        }

        let sense = if interact_rect.is_positive() {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        };
        let mut response = ui.interact(interact_rect, id, sense);
        response.set_intrinsic_size(grid_size);
        {
            let title = terminal.title();
            response.widget_info(|| WidgetInfo::labeled(Role::Terminal, true, title));
        }

        let prev_size = terminal.size();
        let resized_this_frame = prev_size != (cols, rows);
        if resized_this_frame {
            terminal.resize(cols, rows);
        }

        let pixel_width = ((f32::from(cols) * cell_size.x) * ppp)
            .round()
            .clamp(0.0, f32::from(u16::MAX)) as u16;
        let pixel_height = ((f32::from(rows) * cell_size.y) * ppp)
            .round()
            .clamp(0.0, f32::from(u16::MAX)) as u16;
        if terminal.pixel_size() != (pixel_width, pixel_height) {
            terminal.set_pixel_size(pixel_width, pixel_height);
        }
        let cell_px_w = (cell_size.x * ppp).round().clamp(1.0, f32::from(u16::MAX)) as u16;
        let cell_px_h = (cell_size.y * ppp).round().clamp(1.0, f32::from(u16::MAX)) as u16;
        if terminal.cell_pixel_size() != (cell_px_w, cell_px_h) {
            terminal.set_cell_pixel_size(cell_px_w, cell_px_h);
        }
        let term_size = TerminalSize::with_pixels(cols, rows, pixel_width, pixel_height);

        let io_error = self.propagate_backend_resize(
            ui.ctx(),
            input,
            term_size,
            config.resize_debounce_secs,
            now_secs,
        );

        FrameLayout {
            response,
            term_rect,
            clip_rect,
            cols,
            rows,
            cell_size,
            raw_cell_w,
            effective_font_size,
            pixels_per_point: ppp,
            resized_this_frame,
            io_error,
        }
    }

    fn propagate_backend_resize<I: TerminalInput>(
        &mut self,
        ctx: &egui::Context,
        input: &mut I,
        term_size: TerminalSize,
        debounce_secs: f64,
        now_secs: f64,
    ) -> Option<TerminalIoError> {
        let debounce = if debounce_secs.is_finite() && debounce_secs > 0.0 {
            debounce_secs.min(MAX_DEBOUNCE_SECS)
        } else {
            0.0
        };

        if self.last_sent_size.is_none() {
            // First frame: send initial terminal size immediately so PTY/backend knows starting dimensions.
            let io_error = input.resize(term_size).err();
            self.last_sent_size = Some(term_size);
            self.pending_resize = None;
            return io_error;
        }

        if debounce == 0.0 {
            let mut io_error = None;
            if self.last_sent_size != Some(term_size) {
                io_error = input.resize(term_size).err();
                self.last_sent_size = Some(term_size);
            }
            self.pending_resize = None;
            return io_error;
        }

        if Some(term_size) == self.last_sent_size {
            self.pending_resize = None;
            return None;
        }

        let safe_now = if now_secs.is_finite() { now_secs } else { 0.0 };
        match self.pending_resize {
            Some((pending_size, since)) if pending_size == term_size => {
                let elapsed = if since.is_finite() {
                    (safe_now - since).max(0.0)
                } else {
                    debounce
                };
                if elapsed >= debounce {
                    let io_error = input.resize(term_size).err();
                    self.last_sent_size = Some(term_size);
                    self.pending_resize = None;
                    io_error
                } else {
                    let remaining = (debounce - elapsed).clamp(0.001, MAX_DEBOUNCE_SECS);
                    if remaining.is_finite() {
                        ctx.request_repaint_after(std::time::Duration::from_secs_f64(remaining));
                    }
                    None
                }
            }
            _ => {
                self.pending_resize = Some((term_size, safe_now));
                let wait = debounce.clamp(0.001, MAX_DEBOUNCE_SECS);
                if wait.is_finite() {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
                }
                None
            }
        }
    }
}

fn solve_auto_scale_font(
    ctx: &egui::Context,
    avail_w: f32,
    avail_h: f32,
    target_cols: u16,
    target_rows: u16,
    ppp: f32,
) -> (f32, f32, Vec2) {
    let cols_f = f32::from(target_cols);
    let rows_f = f32::from(target_rows);

    let (min_raw_w, min_cell) = measure_cell(ctx, MIN_FONT_SIZE, ppp);
    if cols_f * min_cell.x > avail_w + 1e-3 || rows_f * min_cell.y > avail_h + 1e-3 {
        return (MIN_FONT_SIZE, min_raw_w, min_cell);
    }

    let mut lo = MIN_FONT_SIZE;
    let mut hi = MAX_FONT_SIZE;
    let mut best = (MIN_FONT_SIZE, min_raw_w, min_cell);

    for _ in 0..16 {
        let mid = 0.5 * (lo + hi);
        let (raw_w, cell) = measure_cell(ctx, mid, ppp);
        if cols_f * cell.x <= avail_w + 1e-3 && rows_f * cell.y <= avail_h + 1e-3 {
            best = (mid, raw_w, cell);
            lo = mid;
        } else {
            hi = mid;
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_px_aligns_to_physical_pixels() {
        for ppp in [1.0_f32, 1.25, 1.5, 2.0, 4.0] {
            for val in [0.0_f32, 0.3, 1.7, 13.333, 127.9] {
                let snapped = snap_px(val, ppp);
                let scaled = snapped * ppp;
                assert!(
                    (scaled - scaled.round()).abs() <= 1e-4,
                    "val={val} ppp={ppp} snapped={snapped}"
                );
            }
        }
    }

    #[test]
    fn snap_size_never_returns_zero_or_non_finite() {
        for ppp in [0.0, -1.0, f32::NAN, 1.0, 2.0] {
            let s = snap_size_at_least_1px(0.0, ppp);
            assert!(s.is_finite() && s > 0.0);
        }
    }

    #[test]
    fn layout_caches_measure_cell_borrows_title_and_wires_cell_pixel_size() {
        use egui::{CentralPanel, Context, Frame, RawInput, ViewportId};
        use egui_xterm_core::{CoreConfig, RingBufferInput};

        let ctx = Context::default();
        ctx.enable_accesskit();
        let mut layout_state = TerminalLayoutState::default();
        let mut terminal = Terminal::new(80, 24, CoreConfig::default());
        terminal.advance(b"\x1b]0;LayoutTitle\x07");
        let mut input = RingBufferInput::new();
        let mut pending_focus = false;
        let mut cfg = TerminalConfig::default();

        let run_pass = |layout_state: &mut TerminalLayoutState,
                        terminal: &mut Terminal,
                        input: &mut RingBufferInput,
                        pending_focus: &mut bool,
                        cfg: &TerminalConfig,
                        time: f64,
                        ppp: f32| {
            let mut raw = RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 400.0))),
                time: Some(time),
                ..Default::default()
            };
            raw.viewports
                .entry(ViewportId::ROOT)
                .or_default()
                .native_pixels_per_point = Some(ppp);
            let mut measured = Vec2::ZERO;
            let out = ctx.run_ui(raw, |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let frame_layout = layout_state.layout_and_resize(
                        ui,
                        1,
                        cfg,
                        terminal,
                        input,
                        time,
                        pending_focus,
                        false,
                        None,
                    );
                    measured = frame_layout.cell_size;
                });
            });
            let ak_has_title = out
                .platform_output
                .accesskit_update
                .as_ref()
                .is_some_and(|u| {
                    u.nodes
                        .iter()
                        .any(|(_, node)| node.label() == Some("LayoutTitle"))
                });
            out.drop_without_applying_deltas();
            (measured, ak_has_title)
        };

        let (cell_1x, has_title_1) = run_pass(
            &mut layout_state,
            &mut terminal,
            &mut input,
            &mut pending_focus,
            &cfg,
            0.0,
            1.0,
        );
        assert!(has_title_1);
        let expected_1x = (
            (cell_1x.x * 1.0).round().clamp(1.0, f32::from(u16::MAX)) as u16,
            (cell_1x.y * 1.0).round().clamp(1.0, f32::from(u16::MAX)) as u16,
        );
        assert_eq!(terminal.cell_pixel_size(), expected_1x);
        assert!(layout_state.cell_measure_cache.is_some());

        // Same font_size and ppp hit measure_cell cache; title update propagates without stale cache.
        terminal.advance(b"\x1b]2;UpdatedTitle\x07");
        let (cell_1x_repeat, _) = run_pass(
            &mut layout_state,
            &mut terminal,
            &mut input,
            &mut pending_focus,
            &cfg,
            0.1,
            1.0,
        );
        assert_eq!(cell_1x_repeat, cell_1x);
        assert_eq!(terminal.title(), "UpdatedTitle");

        // Changing ppp & font_size updates cached measure_cell and cell_pixel_size.
        cfg.font_size = 16.0;
        let (cell_2x, _) = run_pass(
            &mut layout_state,
            &mut terminal,
            &mut input,
            &mut pending_focus,
            &cfg,
            0.2,
            2.0,
        );
        let expected_2x = (
            (cell_2x.x * 2.0).round().clamp(1.0, f32::from(u16::MAX)) as u16,
            (cell_2x.y * 2.0).round().clamp(1.0, f32::from(u16::MAX)) as u16,
        );
        assert_eq!(terminal.cell_pixel_size(), expected_2x);
        assert!(cell_2x.y > cell_1x.y);
    }
}
