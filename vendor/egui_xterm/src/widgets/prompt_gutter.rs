//! `OSC 133` prompt & command exit-code gutter overlay (`shell-integration` feature).

use egui::{Color32, Mesh, Pos2, Rect, Vec2, pos2};
use egui_xterm_core::{RenderSnapshot, ShellZonesView, ZoneKind};

use crate::layout::{snap_px, snap_rect, snap_size_at_least_1px};

/// Paints pixel-snapped left-edge `OSC 133` semantic zone bars and exit-code indicators into
/// `overlay_mesh` without shifting `response.rect.min` or emitting text shapes.
pub(crate) fn paint_prompt_gutter(
    overlay_mesh: &mut Mesh,
    snapshot: &RenderSnapshot<'_>,
    zones: ShellZonesView<'_>,
    origin: Pos2,
    cell_size: Vec2,
    ppp: f32,
) {
    if zones.is_empty() || snapshot.rows == 0 || snapshot.cols == 0 || snapshot.is_alt_screen {
        return;
    }

    let retained_top = snapshot
        .scrollback_len
        .saturating_sub(snapshot.display_offset);
    let visible_rows = usize::from(snapshot.rows);
    let retained_end = retained_top.saturating_add(visible_rows);

    let bar_w = snap_size_at_least_1px(2.0, ppp);
    let badge_w = snap_size_at_least_1px(4.0, ppp);
    let grid_right = snap_px(origin.x + f32::from(snapshot.cols) * cell_size.x, ppp);

    for zone in zones.in_line_range(retained_top, retained_end) {
        if zone.end_line < retained_top || zone.start_line >= retained_end {
            continue;
        }
        let r0 = zone.start_line.saturating_sub(retained_top);
        let r1 = zone
            .end_line
            .min(retained_end.saturating_sub(1))
            .saturating_sub(retained_top);

        let color = match zone.exit_code {
            Some(0) => Color32::from_rgb(76, 175, 80),
            Some(_) => Color32::from_rgb(244, 67, 54),
            None => match zone.kind {
                ZoneKind::Prompt => Color32::from_rgb(100, 149, 237),
                ZoneKind::Input => Color32::from_rgb(144, 164, 174),
                ZoneKind::Output => Color32::from_rgb(120, 144, 156),
            },
        };

        let y0 = snap_px(origin.y + r0 as f32 * cell_size.y, ppp);
        let y1 = snap_px(origin.y + (r1 + 1) as f32 * cell_size.y, ppp);
        let bar_rect = snap_rect(
            Rect::from_min_max(pos2(origin.x, y0), pos2(origin.x + bar_w, y1)),
            ppp,
        );
        if bar_rect.is_positive() {
            overlay_mesh.add_colored_rect(bar_rect, color);
        }

        // For prompt lines with a known command exit status, emit a distinct right-edge badge so
        // exit codes remain visually distinct even at narrow widths.
        if zone.kind == ZoneKind::Prompt
            && zone.exit_code.is_some()
            && zone.start_line >= retained_top
        {
            let badge_top = snap_px(y0 + cell_size.y * 0.25, ppp);
            let badge_bottom = snap_px(y0 + cell_size.y * 0.75, ppp).max(badge_top + 1.0 / ppp);
            let badge_left = snap_px((grid_right - badge_w).max(origin.x), ppp);
            let badge_rect = snap_rect(
                Rect::from_min_max(pos2(badge_left, badge_top), pos2(grid_right, badge_bottom)),
                ppp,
            );
            if badge_rect.is_positive() {
                overlay_mesh.add_colored_rect(badge_rect, color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use egui_xterm_core::{CoreConfig, Terminal};

    use super::*;

    #[test]
    fn prompt_gutter_uses_range_query_across_many_zones() {
        let mut term = Terminal::new(80, 10, CoreConfig::default());
        for i in 0..2500 {
            let line =
                format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd_{i}\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
            term.advance(line.as_bytes());
        }
        term.scroll_display(1200);
        let snap = term.active_snapshot();
        let zones = term.shell_zones();
        assert!(zones.len() >= 100);

        let mut mesh = Mesh::default();
        paint_prompt_gutter(
            &mut mesh,
            &snap,
            zones,
            Pos2::ZERO,
            Vec2::new(8.0, 16.0),
            1.0,
        );
        assert!(!mesh.indices.is_empty());
        assert!(mesh.vertices.len() <= 200);
    }
}
