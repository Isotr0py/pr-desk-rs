//! Optional UI chrome widgets (`search_bar`, `prompt_gutter`, `hyperlink`, `scrollbar`,
//! `context_menu`, `progress_bar`) layered cleanly onto the terminal grid without shifting
//! `response.rect.min`.

pub(crate) mod context_menu;
#[cfg(feature = "hyperlinks")]
pub(crate) mod hyperlink;
pub mod progress_bar;
#[cfg(feature = "shell-integration")]
pub(crate) mod prompt_gutter;
pub(crate) mod scrollbar;
#[cfg(feature = "search")]
pub mod search_bar;

/// User actions emitted by optional terminal overlays and context menus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetAction {
    /// Copy the active selection to the clipboard.
    CopySelection,
    /// Request paste from the clipboard.
    PasteClipboard,
    /// Clear scrollback history / terminal viewport.
    ClearScrollback,
    /// Open or focus the search bar overlay.
    #[cfg(feature = "search")]
    OpenSearchBar,
    /// Jump to a relative `OSC 133` prompt (`-1` previous, `+1` next).
    #[cfg(feature = "shell-integration")]
    JumpPrompt(i32),
}
