//! Non-intrusive scrollback scrollbar overlay.

use egui::{Color32, Pos2, Rect, Sense, Ui, pos2};
use egui_xterm_core::Terminal;

use crate::layout::{FrameLayout, snap_px, snap_rect, snap_size_at_least_1px};

/// Per-frame scrollbar hit-test and interaction state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarFrame {
    pub track_rect: Rect,
    pub thumb_rect: Rect,
    pub visible: bool,
    pub hovered: bool,
    pub dragged: bool,
    pub pointer_active: bool,
}

/// Computes scrollbar geometry and handles pointer click/drag interaction BEFORE grid event mapping
/// so dragging or clicking the scrollbar never triggers a terminal text selection.
///
/// Returns `None` when `scrollback_len() == 0` or on the alternate screen.
pub(crate) fn interact_scrollbar(
    ui: &mut Ui,
    id_salt: u64,
    terminal: &mut Terminal,
    layout: &FrameLayout,
    search_bar_rect: Option<Rect>,
    suppress_interaction: bool,
) -> Option<ScrollbarFrame> {
    let term_rect = layout.response.rect;
    let ppp = layout.pixels_per_point;
    let terminal_hovered = layout.response.hovered();
    let scrollback = terminal.scrollback_len();
    if scrollback == 0
        || terminal.is_alt_screen()
        || term_rect.width() < 24.0
        || term_rect.height() < 24.0
    {
        return None;
    }

    let display_offset = terminal.display_offset();
    let id = ui.make_persistent_id((id_salt, "scrollbar"));
    let bar_w = snap_size_at_least_1px(6.0, ppp);
    let track_rect = snap_rect(
        Rect::from_min_max(
            pos2(snap_px(term_rect.max.x - bar_w, ppp), term_rect.min.y),
            term_rect.max,
        ),
        ppp,
    );
    if !track_rect.is_positive() {
        return None;
    }

    // If the floating search bar currently covers a pointer inside track_rect, yield to search bar.
    let pointer_in_search = ui
        .input(|i| i.pointer.latest_pos())
        .is_some_and(|p| search_bar_rect.is_some_and(|r| r.contains(p)));

    let resp = ui.interact(track_rect, id, Sense::CLICK | Sense::DRAG);
    let visible = terminal_hovered || display_offset > 0 || resp.hovered() || resp.dragged();

    let visible_rows = usize::from(terminal.size().1.max(1));
    let total_rows = scrollback + visible_rows;
    let track_h = track_rect.height();
    let thumb_h = snap_size_at_least_1px(
        ((visible_rows as f32 / total_rows as f32) * track_h)
            .max(16.0)
            .min(track_h),
        ppp,
    );
    let max_offset = scrollback as f32;
    let ratio = if max_offset > 0.0 {
        1.0 - (display_offset as f32 / max_offset).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let travel = (track_h - thumb_h).max(0.0);
    let thumb_top = snap_px(track_rect.min.y + ratio * travel, ppp);
    let thumb_bottom = snap_px((thumb_top + thumb_h).min(track_rect.max.y), ppp);
    let thumb_rect = snap_rect(
        Rect::from_min_max(
            Pos2::new(track_rect.min.x, thumb_top),
            Pos2::new(track_rect.max.x, thumb_bottom),
        ),
        ppp,
    );

    if !suppress_interaction
        && !pointer_in_search
        && (resp.dragged() || resp.clicked())
        && let Some(pos) = resp.interact_pointer_pos()
        && travel > 0.0
    {
        let target_ratio = ((pos.y - track_rect.min.y - thumb_h * 0.5) / travel).clamp(0.0, 1.0);
        let target_offset = ((1.0 - target_ratio) * max_offset).round() as usize;
        let delta = i64::try_from(target_offset)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(terminal.display_offset()).unwrap_or(i64::MAX))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        if delta != 0 {
            terminal.scroll_display(delta);
        }
    }

    let secondary_or_middle_event_in_track = ui.input(|i| {
        i.events.iter().any(|ev| {
            matches!(
                ev,
                egui::Event::PointerButton {
                    button: egui::PointerButton::Secondary | egui::PointerButton::Middle,
                    pos,
                    ..
                } if track_rect.contains(*pos)
            )
        })
    });
    let interact_in_track = resp
        .interact_pointer_pos()
        .is_some_and(|p| track_rect.contains(p));
    let press_origin_in_track = ui
        .input(|i| i.pointer.press_origin())
        .is_some_and(|p| track_rect.contains(p));
    let pointer_active = !suppress_interaction
        && (resp.hovered()
            || resp.dragged()
            || resp.drag_stopped()
            || resp.clicked()
            || resp.secondary_clicked()
            || resp.middle_clicked()
            || resp.is_pointer_button_down_on()
            || interact_in_track
            || press_origin_in_track
            || secondary_or_middle_event_in_track);

    Some(ScrollbarFrame {
        track_rect,
        thumb_rect,
        visible,
        hovered: !suppress_interaction && resp.hovered(),
        dragged: !suppress_interaction && resp.dragged(),
        pointer_active,
    })
}

/// Paints the overlaid scrollback scrollbar using the frame computed by [`interact_scrollbar`].
pub(crate) fn paint_scrollbar(painter: &egui::Painter, frame: ScrollbarFrame, fg_color: Color32) {
    if !frame.visible {
        return;
    }
    let track_color = Color32::from_rgba_unmultiplied(fg_color.r(), fg_color.g(), fg_color.b(), 24);
    let thumb_alpha = if frame.hovered || frame.dragged {
        160
    } else {
        96
    };
    let thumb_color =
        Color32::from_rgba_unmultiplied(fg_color.r(), fg_color.g(), fg_color.b(), thumb_alpha);

    painter.rect_filled(frame.track_rect, 0.0, track_color);
    if frame.thumb_rect.is_positive() {
        painter.rect_filled(frame.thumb_rect, 0.0, thumb_color);
    }
}
