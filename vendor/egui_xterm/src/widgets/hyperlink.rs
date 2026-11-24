//! Hyperlink hover detection, underline rendering, and click activation (`hyperlinks` feature).

use egui::{Color32, CursorIcon, Mesh, OpenUrl, Pos2, Rect, Ui, Vec2, pos2};
use egui_xterm_core::{HyperlinkSpan, Terminal};

use crate::layout::{FrameLayout, snap_px, snap_rect, snap_size_at_least_1px};

/// Evaluates pointer hover over `OSC 8` and implicit hyperlinks strictly inside the cell grid,
/// returning visible spans and whether the cursor icon was overridden to `PointingHand`.
#[expect(
    clippy::too_many_arguments,
    reason = "hyperlink interaction parameters"
)]
pub(crate) fn update_hyperlink_interaction_with_cursor(
    ui: &Ui,
    terminal: &mut Terminal,
    pointer_pos: Option<Pos2>,
    layout: &FrameLayout,
    allow_click_activation: bool,
    clicked_primary: bool,
    require_ctrl: bool,
    ctrl_or_cmd_held: bool,
) -> (Vec<HyperlinkSpan>, bool) {
    let Some(pos) = pointer_pos else {
        return (Vec::new(), false);
    };
    let origin = layout.term_rect.min;
    let cell_size = layout.cell_size;
    let cols = layout.cols;
    let rows = layout.rows;
    if cols == 0 || rows == 0 || cell_size.x <= 0.0 || cell_size.y <= 0.0 {
        return (Vec::new(), false);
    }

    let grid_rect = Rect::from_min_size(
        origin,
        Vec2::new(f32::from(cols) * cell_size.x, f32::from(rows) * cell_size.y),
    );
    if !layout.response.rect.is_positive()
        || !layout.response.rect.contains(pos)
        || !grid_rect.contains(pos)
    {
        return (Vec::new(), false);
    }

    let rel_x = ((pos.x - origin.x) / cell_size.x).floor();
    let rel_y = ((pos.y - origin.y) / cell_size.y).floor();
    if rel_x < 0.0 || rel_y < 0.0 {
        return (Vec::new(), false);
    }
    let col = (rel_x as u16).min(cols - 1);
    let row = (rel_y as u16).min(rows - 1);

    let Some((info, spans)) = terminal.hyperlink_visible_spans_at(col, row) else {
        return (Vec::new(), false);
    };

    let uri = info.uri.clone();
    let activation_allowed = !require_ctrl || ctrl_or_cmd_held;
    if activation_allowed {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    let tooltip_text = if require_ctrl && !ctrl_or_cmd_held {
        if cfg!(target_os = "macos") {
            format!("{uri}\n(Cmd+Click to open)")
        } else {
            format!("{uri}\n(Ctrl+Click to open)")
        }
    } else {
        uri.clone()
    };
    egui::Tooltip::always_open(
        ui.ctx().clone(),
        ui.layer_id(),
        layout.response.id.with("hyperlink_tooltip"),
        egui::PopupAnchor::Pointer,
    )
    .show(|ui| {
        ui.label(tooltip_text);
    });

    if allow_click_activation && activation_allowed && clicked_primary {
        terminal.activate_hyperlink_at(col, row);
        ui.ctx().open_url(OpenUrl::new_tab(uri));
    }

    (spans, activation_allowed)
}

/// Paints pixel-snapped hover underlines for active hyperlink spans into `overlay_mesh`.
pub(crate) fn paint_hyperlink_underlines(
    overlay_mesh: &mut Mesh,
    spans: &[HyperlinkSpan],
    origin: Pos2,
    cell_size: Vec2,
    ppp: f32,
    color: Color32,
) {
    if spans.is_empty() {
        return;
    }
    let thickness = snap_size_at_least_1px((cell_size.y * 0.08).max(1.0), ppp);
    for span in spans {
        if span.end_col <= span.start_col {
            continue;
        }
        let x0 = snap_px(origin.x + f32::from(span.start_col) * cell_size.x, ppp);
        let x1 = snap_px(origin.x + f32::from(span.end_col) * cell_size.x, ppp);
        let bottom = snap_px(origin.y + f32::from(span.row + 1) * cell_size.y, ppp);
        let top = snap_px((bottom - thickness).max(origin.y), ppp);
        let rect = snap_rect(Rect::from_min_max(pos2(x0, top), pos2(x1, bottom)), ppp);
        if rect.is_positive() {
            overlay_mesh.add_colored_rect(rect, color);
        }
    }
}
