//! Configuration and response types for the [`crate::TerminalRuntime`] widget.

use egui_xterm_core::{CursorShape, Osc52ReadPolicy, TerminalEvent, Theme};

/// Active `OSC 9;4` task progress indicator state reported by the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProgressIndicator {
    /// Progress state (`0` = hidden, `1` = normal, `2` = error, `3` = indeterminate, `4` = paused).
    pub state: u8,
    /// Completion percentage (`0..=100`).
    pub progress: u8,
}

/// How the terminal grid adapts when its allocated [`egui::Ui`] container changes size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ScaleMode {
    /// Preserve the configured font size and zoom factor, recomputing `(cols, rows)` from the
    /// container's available space and triggering primary-screen reflow.
    #[default]
    ReflowGrid,
    /// Keep a fixed `(cols, rows)` logical grid and dynamically scale the rendered font size to fit
    /// inside the available container rectangle.
    AutoScaleToFit {
        /// Target column count (clamped to at least `1`).
        cols: u16,
        /// Target row count (clamped to at least `1`).
        rows: u16,
    },
}

/// Policy controlling how `Copy` shortcuts (`Event::Copy` / `Ctrl+C`) behave when text is
/// selected or unselected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CopyBehavior {
    /// Copy the active selection when present; otherwise send `ETX` (`0x03`, `Ctrl+C`) on
    /// non-macOS platforms (or when `Ctrl+C` is pressed directly).
    #[default]
    CopyIfSelectionElseSendEtx,
    /// Always copy the active selection when present and never emit `ETX`.
    AlwaysCopy,
    /// Always emit `ETX` (`0x03`) without copying the selection.
    AlwaysSendEtx,
}

/// Fine-grained cursor rendering parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, deny_unknown_fields))]
pub struct CursorRenderConfig {
    /// Draw a hollow 1px-snapped rectangle when the terminal does not hold keyboard focus.
    pub hollow_when_unfocused: bool,
    /// Full blink cycle period in seconds (on for half the period, off for half).
    pub blink_period_secs: f64,
    /// Width of [`CursorShape::Bar`] in logical points before physical pixel snapping.
    pub bar_width_px: f32,
    /// Height of [`CursorShape::Underline`] in logical points before physical pixel snapping.
    pub underline_height_px: f32,
}

impl Default for CursorRenderConfig {
    fn default() -> Self {
        Self {
            hollow_when_unfocused: true,
            blink_period_secs: 1.0,
            bar_width_px: 2.0,
            underline_height_px: 2.0,
        }
    }
}

/// Configuration for [`crate::TerminalRuntime`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, deny_unknown_fields))]
pub struct TerminalConfig {
    /// Base monospace font size in logical points.
    pub font_size: f32,
    /// Multiplier applied to [`Self::font_size`] in [`ScaleMode::ReflowGrid`].
    pub zoom_factor: f32,
    /// Layout and scaling strategy for container size changes.
    pub scale_mode: ScaleMode,
    /// Maximum number of retained scrollback lines on the primary screen.
    pub scrollback_limit: usize,
    /// Default cursor shape when not overridden by `DECSCUSR`.
    pub cursor_shape: CursorShape,
    /// Default cursor blink setting when not overridden by `DECSCUSR`.
    pub cursor_blink: bool,
    /// When `true`, `Ctrl+C` / `Event::Copy` copies an active selection instead of sending `ETX`.
    pub copy_on_ctrl_c_when_selected: bool,
    /// Debounce interval in seconds before forwarding grid size changes to [`crate::TerminalInput::resize`].
    pub resize_debounce_secs: f64,
    /// Color scheme and bold-bright behavior.
    pub theme: Theme,
    /// Opacity of the default terminal background in `0.0..=1.0`.
    /// Text, cursor, selection, and explicit ANSI backgrounds are unaffected.
    pub background_opacity: f32,
    /// Maximum number of bytes polled from [`crate::TerminalInput::poll_read`] per frame.
    pub read_budget_bytes_per_frame: usize,
    /// Delay in milliseconds before polling an idle input again, even if `egui` is otherwise idle.
    /// Set to `0` when the producer requests repaints as bytes arrive.
    pub idle_poll_interval_ms: u32,
    /// Additional cursor geometry and blink tuning.
    pub cursor_config: CursorRenderConfig,
    /// Copy vs interrupt behavior override for copy shortcuts.
    pub copy_behavior: CopyBehavior,
    /// Enable SGR blinking text (`SGR 5`/`6`) with a 500ms on/off cycle.
    pub enable_blink: bool,
    /// Duration in milliseconds of the visual bell flash on `BEL` (`0x07`). Set to `0` to disable.
    pub visual_bell_duration_ms: u32,
    /// Optional color override for the visual bell flash on `BEL` (`0x07`). When `None`, uses
    /// the active theme's foreground color.
    pub visual_bell_color: Option<egui::Color32>,
    /// Peak opacity of the visual bell flash in `0.0..=1.0`. Set to `0.0` to disable.
    pub visual_bell_opacity: f32,
    /// When `true`, render a subtle top-edge `OSC 9;4` progress indicator bar when active.
    pub show_progress_bar: bool,
    /// When `true`, allow `OSC 52` clipboard write sequences to update the host clipboard.
    pub osc52_write_enabled: bool,
    /// Policy controlling how `OSC 52 ; Pc ; ?` clipboard read requests are handled.
    pub osc52_read_policy: Osc52ReadPolicy,
    /// When `true`, clicking a hyperlink requires holding `Ctrl` (or `Cmd` on macOS).
    pub hyperlink_require_ctrl: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            zoom_factor: 1.0,
            scale_mode: ScaleMode::ReflowGrid,
            scrollback_limit: 10_000,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            copy_on_ctrl_c_when_selected: true,
            resize_debounce_secs: 0.15,
            theme: Theme::dark(),
            background_opacity: 1.0,
            read_budget_bytes_per_frame: 65_536,
            idle_poll_interval_ms: 50,
            cursor_config: CursorRenderConfig::default(),
            copy_behavior: CopyBehavior::CopyIfSelectionElseSendEtx,
            enable_blink: true,
            visual_bell_duration_ms: 150,
            visual_bell_color: None,
            visual_bell_opacity: 0.18,
            show_progress_bar: true,
            osc52_write_enabled: true,
            osc52_read_policy: Osc52ReadPolicy::Deny,
            hyperlink_require_ctrl: false,
        }
    }
}

impl TerminalConfig {
    pub(crate) fn effective_copy_behavior(&self) -> CopyBehavior {
        match self.copy_behavior {
            CopyBehavior::CopyIfSelectionElseSendEtx => {
                if self.copy_on_ctrl_c_when_selected {
                    CopyBehavior::CopyIfSelectionElseSendEtx
                } else {
                    CopyBehavior::AlwaysSendEtx
                }
            }
            other => other,
        }
    }
}

/// Result returned by [`crate::TerminalRuntime::show`] and [`crate::TerminalRuntime::show_with_config`].
#[derive(Debug, Clone)]
pub struct TerminalResponse {
    /// Underlying [`egui::Response`] covering the snapped terminal grid rectangle.
    pub response: egui::Response,
    /// Computed visible column count (`>= 1`).
    pub cols: u16,
    /// Computed visible row count (`>= 1`).
    pub rows: u16,
    /// Pixel-snapped logical cell size `(cell_width, cell_height)`.
    pub cell_size: egui::Vec2,
    /// Effective font size in logical points used for rendering this frame.
    pub effective_font_size: f32,
    /// `true` if the core terminal grid dimensions changed during this frame.
    pub resized_this_frame: bool,
    /// Semantic terminal events emitted during this frame (e.g. title, CWD, bell, hyperlinks).
    pub events: Vec<TerminalEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_config_and_progress_indicator_defaults() {
        let cfg = TerminalConfig::default();
        assert_eq!(cfg.visual_bell_color, None);
        assert!((cfg.visual_bell_opacity - 0.18).abs() < 1e-6);
        assert!(cfg.show_progress_bar);
        assert!(cfg.osc52_write_enabled);
        assert_eq!(cfg.osc52_read_policy, Osc52ReadPolicy::Deny);

        let ind = ProgressIndicator::default();
        assert_eq!(ind.state, 0);
        assert_eq!(ind.progress, 0);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn terminal_config_and_progress_indicator_implement_serde() {
        fn assert_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
        assert_serde::<ProgressIndicator>();
        assert_serde::<TerminalConfig>();
    }
}
