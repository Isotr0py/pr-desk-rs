//! High-performance, auto-layout terminal widget and runtime for [`egui`].
//!
//! Consumers provide an implementation of [`TerminalInput`] (such as [`ChannelInput`],
//! [`RingBufferInput`], [`FnInput`], or a custom non-blocking stream) and call
//! [`TerminalRuntime::show`] inside any `egui` frame or panel. Layout sizing, pixel snapping,
//! resize reflow, VT500 parsing, selection, clipboard, IME, and 4-stage cached rendering happen
//! automatically.
//!
//! # Examples
//!
//! Drive a [`TerminalRuntime`] inside an `egui::Context` frame:
//!
//! ```
//! use egui::{Context, RawInput, Rect, pos2, vec2};
//! use egui_xterm::{RingBufferInput, TerminalConfig, TerminalRuntime};
//!
//! let ctx = Context::default();
//! let mut runtime = TerminalRuntime::with_config(
//!     RingBufferInput::from_bytes(b"hi\r\n"),
//!     TerminalConfig::default(),
//! );
//! let raw = RawInput {
//!     screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 480.0))),
//!     time: Some(0.0),
//!     ..Default::default()
//! };
//! let out = ctx.run_ui(raw, |ui| {
//!     let resp = runtime.show(ui);
//!     assert!(resp.cols >= 1 && resp.rows >= 1);
//! });
//! out.drop_without_applying_deltas();
//! ```
//!
//! Keep a fixed logical grid with [`ScaleMode::AutoScaleToFit`]:
//!
//! ```
//! use egui::{Context, RawInput, Rect, pos2, vec2};
//! use egui_xterm::{RingBufferInput, ScaleMode, TerminalConfig, TerminalRuntime};
//!
//! let ctx = Context::default();
//! let cfg = TerminalConfig {
//!     scale_mode: ScaleMode::AutoScaleToFit { cols: 80, rows: 24 },
//!     ..TerminalConfig::default()
//! };
//! let mut runtime = TerminalRuntime::with_config(RingBufferInput::from_bytes(b"fit\r\n"), cfg);
//! let raw = RawInput {
//!     screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 384.0))),
//!     time: Some(0.0),
//!     ..Default::default()
//! };
//! let mut dims = (0u16, 0u16);
//! let out = ctx.run_ui(raw, |ui| {
//!     let resp = runtime.show(ui);
//!     dims = (resp.cols, resp.rows);
//! });
//! out.drop_without_applying_deltas();
//! assert_eq!(dims, (80, 24));
//! ```
//!
//! Embed [`TerminalRuntime::widget`] inside an [`egui::Frame`]:
//!
//! ```
//! use egui::{Context, Frame, RawInput, Rect, pos2, vec2};
//! use egui_xterm::{RingBufferInput, TerminalRuntime};
//!
//! let ctx = Context::default();
//! let mut runtime = TerminalRuntime::new(RingBufferInput::from_bytes(b"framed\r\n"));
//! let raw = RawInput {
//!     screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 400.0))),
//!     time: Some(0.0),
//!     ..Default::default()
//! };
//! let out = ctx.run_ui(raw, |ui| {
//!     Frame::new().show(ui, |ui| {
//!         let _response = ui.add(runtime.widget());
//!     });
//! });
//! out.drop_without_applying_deltas();
//! ```

#![forbid(unsafe_code)]

pub mod config;
mod input_map;
mod layout;
mod renderer;
pub mod runtime;
pub mod widgets;

pub use crate::config::{
    CopyBehavior, CursorRenderConfig, ProgressIndicator, ScaleMode, TerminalConfig,
    TerminalResponse,
};
pub use crate::runtime::{TerminalRuntime, TerminalWidget};

pub use egui_xterm_core::{
    CellFlags, CellSnapshot, ChannelInput, ClipboardSelection, ColorScheme, CoreConfig,
    CursorShape, FnInput, Osc52ReadPolicy, PaletteOverrides, RingBufferInput, SelectionKind,
    SelectionMode, Terminal, TerminalEvent, TerminalInput, TerminalIoError, TerminalSize, Theme,
    UnderlineStyle,
};

#[cfg(feature = "metrics")]
pub use egui_xterm_core::TerminalMetrics;
#[cfg(feature = "serde")]
pub use egui_xterm_core::TerminalSnapshot;
#[cfg(feature = "hyperlinks")]
pub use egui_xterm_core::{HyperlinkInfo, HyperlinkSpan};
#[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
pub use egui_xterm_core::{ImageFormat, ImagePlacement, StoredImage};
#[cfg(feature = "kitty-keyboard")]
pub use egui_xterm_core::{
    KITTY_STACK_CAP, KittyEventType, KittyKey, KittyKeyEvent, KittyKeyboardFlags, KittyModifiers,
    encode_kitty_key,
};
#[cfg(feature = "search")]
pub use egui_xterm_core::{SearchError, SearchMatch, SearchQuery};
#[cfg(feature = "shell-integration")]
pub use egui_xterm_core::{ShellZone, ZoneKind};

#[cfg(all(feature = "pty", not(target_arch = "wasm32")))]
pub use egui_xterm_pty as pty;
