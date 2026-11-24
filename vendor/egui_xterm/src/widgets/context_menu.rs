//! Context menu attachment for terminal actions.

use egui::Response;
use egui_xterm_core::Terminal;

use crate::widgets::WidgetAction;

/// Attaches a standard terminal right-click context menu to `response` when `allow_menu` is `true`,
/// returning any selected [`WidgetAction`]. Emits zero shapes when closed.
pub(crate) fn attach_context_menu(
    response: &Response,
    terminal: &Terminal,
    allow_menu: bool,
) -> Option<WidgetAction> {
    if !allow_menu {
        return None;
    }

    let mut action = None;

    response.context_menu(|ui| {
        let has_selection = terminal.has_non_empty_selection();
        if ui
            .add_enabled(has_selection, egui::Button::new("Copy"))
            .clicked()
        {
            action = Some(WidgetAction::CopySelection);
            ui.close();
        }
        if ui.button("Paste").clicked() {
            action = Some(WidgetAction::PasteClipboard);
            ui.close();
        }
        ui.separator();
        #[cfg(feature = "search")]
        if ui.button("Find...").clicked() {
            action = Some(WidgetAction::OpenSearchBar);
            ui.close();
        }
        #[cfg(feature = "shell-integration")]
        {
            if ui.button("Previous Prompt").clicked() {
                action = Some(WidgetAction::JumpPrompt(-1));
                ui.close();
            }
            if ui.button("Next Prompt").clicked() {
                action = Some(WidgetAction::JumpPrompt(1));
                ui.close();
            }
        }
        if ui.button("Clear Scrollback").clicked() {
            action = Some(WidgetAction::ClearScrollback);
            ui.close();
        }
    });

    action
}
