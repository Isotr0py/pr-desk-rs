//! Frame-driven [`TerminalRuntime`] and [`TerminalWidget`] integration for `egui`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use egui::{Ui, Widget};
use egui_xterm_core::{
    ClipboardSelection, CoreConfig, MouseProtocol, Osc52ReadPolicy, Terminal, TerminalEvent,
    TerminalInput, TerminalIoError,
};

use crate::config::{ProgressIndicator, TerminalConfig, TerminalResponse};
use crate::input_map::{InputMapperState, copy_text_everywhere, try_read_native_clipboard};
use crate::layout::TerminalLayoutState;
use crate::renderer::TerminalRenderer;
use crate::widgets::WidgetAction;

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);
const POLL_CHUNK_BYTES: usize = 65_536;

/// Stateful `egui` terminal runtime wrapping a [`Terminal`] and a user-provided [`TerminalInput`].
///
/// Calling [`Self::show`] or [`Self::show_with_config`] inside any `egui` container automatically
/// detects the allocated area, snaps cell geometry to physical pixels, updates `(cols, rows)` or
/// font scaling, drains input up to `read_budget_bytes_per_frame`, routes keyboard/IME/mouse/
/// clipboard events, and renders via the 4-stage cached pipeline.
pub struct TerminalRuntime<I: TerminalInput> {
    config: TerminalConfig,
    inner: RuntimeInner<I>,
}

struct RuntimeInner<I: TerminalInput> {
    instance_id: u64,
    input: I,
    terminal: Terminal,
    layout_state: TerminalLayoutState,
    renderer: TerminalRenderer,
    input_mapper: InputMapperState,
    poll_scratch: Vec<u8>,
    poll_staging: Vec<u8>,
    outgoing_scratch: Vec<u8>,
    secondary_press_in_search_bar: bool,
    scrollbar_secondary_press_in_track: bool,
    mouse_cursor_icon: Option<egui::CursorIcon>,
    last_io_error: Option<TerminalIoError>,
    progress: Option<ProgressIndicator>,
    pending_osc52_read: Option<ClipboardSelection>,
    last_cfg_osc52_read_policy: Osc52ReadPolicy,
    #[cfg(feature = "search")]
    search_bar: crate::widgets::search_bar::SearchBarState,
}

impl<I: TerminalInput> TerminalRuntime<I> {
    /// Creates a new runtime with [`TerminalConfig::default()`].
    #[must_use]
    pub fn new(input: I) -> Self {
        Self::with_config(input, TerminalConfig::default())
    }

    /// Creates a new runtime initialized from `config`.
    #[must_use]
    pub fn with_config(input: I, config: TerminalConfig) -> Self {
        let core_cfg = CoreConfig {
            scrollback_limit: config.scrollback_limit,
            theme: config.theme.clone(),
            default_cursor_shape: config.cursor_shape,
            default_cursor_blink: config.cursor_blink,
            osc52_read_policy: config.osc52_read_policy,
            allow_osc52_read: matches!(config.osc52_read_policy, Osc52ReadPolicy::Allow),
            ..CoreConfig::default()
        };
        let last_cfg_osc52_read_policy = config.osc52_read_policy;
        let terminal = Terminal::new(80, 24, core_cfg);
        Self {
            config,
            inner: RuntimeInner {
                instance_id: NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed),
                input,
                terminal,
                layout_state: TerminalLayoutState::default(),
                renderer: TerminalRenderer::default(),
                input_mapper: InputMapperState::default(),
                poll_scratch: vec![0u8; POLL_CHUNK_BYTES],
                poll_staging: Vec::new(),
                outgoing_scratch: Vec::new(),
                secondary_press_in_search_bar: false,
                scrollbar_secondary_press_in_track: false,
                mouse_cursor_icon: None,
                last_io_error: None,
                progress: None,
                pending_osc52_read: None,
                last_cfg_osc52_read_policy,
                #[cfg(feature = "search")]
                search_bar: crate::widgets::search_bar::SearchBarState::default(),
            },
        }
    }

    /// Renders and updates the terminal using its internal [`TerminalConfig`].
    pub fn show(&mut self, ui: &mut Ui) -> TerminalResponse {
        self.inner.show_with_config(ui, &self.config)
    }

    /// Renders and updates the terminal using an externally owned mutable [`TerminalConfig`].
    pub fn show_with_config(
        &mut self,
        ui: &mut Ui,
        config: &mut TerminalConfig,
    ) -> TerminalResponse {
        self.inner.show_with_config(ui, config)
    }

    /// Returns a [`TerminalWidget`] adapter implementing [`egui::Widget`].
    pub fn widget(&mut self) -> TerminalWidget<'_, I> {
        TerminalWidget::new(self)
    }

    /// Returns a shared reference to the underlying [`Terminal`] state machine.
    #[must_use]
    pub const fn terminal(&self) -> &Terminal {
        &self.inner.terminal
    }

    /// Returns a mutable reference to the underlying [`Terminal`] state machine.
    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.inner.terminal
    }

    /// Returns a shared reference to the backing [`TerminalInput`].
    #[must_use]
    pub const fn input(&self) -> &I {
        &self.inner.input
    }

    /// Returns a mutable reference to the backing [`TerminalInput`].
    pub fn input_mut(&mut self) -> &mut I {
        &mut self.inner.input
    }

    /// Returns a shared reference to the internal [`TerminalConfig`].
    #[must_use]
    pub const fn config(&self) -> &TerminalConfig {
        &self.config
    }

    /// Returns a mutable reference to the internal [`TerminalConfig`].
    pub fn config_mut(&mut self) -> &mut TerminalConfig {
        &mut self.config
    }

    /// Opens the floating search bar overlay (`search` feature).
    #[cfg(feature = "search")]
    pub fn open_search(&mut self) {
        self.inner.search_bar.open();
    }

    /// Closes the floating search bar overlay (`search` feature).
    #[cfg(feature = "search")]
    pub fn close_search(&mut self) {
        self.inner.search_bar.close();
    }

    /// Returns mutable access to [`crate::widgets::search_bar::SearchBarState`] (`search` feature).
    #[cfg(feature = "search")]
    pub fn search_bar_mut(&mut self) -> &mut crate::widgets::search_bar::SearchBarState {
        &mut self.inner.search_bar
    }

    /// Returns the most recent non-fatal backend `poll_read`, `write_bytes`, or `resize` I/O
    /// error, if any.
    #[must_use]
    pub const fn last_io_error(&self) -> Option<&TerminalIoError> {
        self.inner.last_io_error.as_ref()
    }

    /// Takes and clears the most recent non-fatal backend `poll_read`, `write_bytes`, or `resize`
    /// I/O error.
    pub fn take_io_error(&mut self) -> Option<TerminalIoError> {
        self.inner.last_io_error.take()
    }

    /// Returns the active `OSC 9;4` progress indicator state, if any.
    #[must_use]
    pub const fn progress(&self) -> Option<ProgressIndicator> {
        self.inner.progress
    }

    /// Returns the last reported current document URI (`OSC 6`), if known.
    #[must_use]
    pub fn document_uri(&self) -> Option<&str> {
        self.inner.terminal.document_uri()
    }

    /// Returns the pending `OSC 52` clipboard read selection awaiting host approval under
    /// [`Osc52ReadPolicy::Prompt`], if any.
    #[must_use]
    pub const fn pending_osc52_read(&self) -> Option<ClipboardSelection> {
        self.inner.pending_osc52_read
    }

    /// Approves a pending `OSC 52` clipboard read prompt, sending `text` to the backend and
    /// returning `true` if a prompt was pending.
    pub fn approve_osc52_read(&mut self, text: &str) -> bool {
        let Some(selection) = self.inner.pending_osc52_read.take() else {
            return false;
        };
        self.inner.terminal.provide_clipboard_read(selection, text);
        let outgoing = self.inner.terminal.drain_outgoing();
        if !outgoing.is_empty()
            && let Err(err) = self.inner.input.write_bytes(&outgoing)
            && !err.is_closed()
        {
            self.inner.last_io_error = Some(err);
        }
        true
    }

    /// Dismisses a pending `OSC 52` clipboard read prompt without transmitting clipboard data,
    /// returning `true` if a prompt was pending.
    pub fn deny_osc52_read(&mut self) -> bool {
        self.inner.pending_osc52_read.take().is_some()
    }

    /// Explicitly updates the tracked `CapsLock` and `NumLock` modifier states used for Kitty
    /// keyboard modifier bit encoding.
    pub const fn set_lock_modifiers(&mut self, caps_lock: bool, num_lock: bool) {
        self.inner
            .input_mapper
            .set_lock_modifiers(caps_lock, num_lock);
    }

    /// Returns the currently tracked `CapsLock` lock modifier state.
    #[must_use]
    pub const fn caps_lock(&self) -> bool {
        self.inner.input_mapper.caps_lock()
    }

    /// Returns the currently tracked `NumLock` lock modifier state.
    #[must_use]
    pub const fn num_lock(&self) -> bool {
        self.inner.input_mapper.num_lock()
    }
}

impl<I: TerminalInput> RuntimeInner<I> {
    fn record_io_error(
        &mut self,
        err: TerminalIoError,
        frame_io_errors: &mut Vec<TerminalIoError>,
    ) {
        if !err.is_closed() {
            self.last_io_error = Some(err.clone());
            frame_io_errors.push(err);
        }
    }

    fn show_with_config(&mut self, ui: &mut Ui, config: &TerminalConfig) -> TerminalResponse {
        let now_secs = ui.input(|i| i.time);
        let safe_now = if now_secs.is_finite() { now_secs } else { 0.0 };
        if self.terminal.theme() != &config.theme {
            self.terminal.set_theme(config.theme.clone());
        }
        if config.osc52_read_policy != self.last_cfg_osc52_read_policy {
            self.last_cfg_osc52_read_policy = config.osc52_read_policy;
            self.terminal
                .set_osc52_read_policy(config.osc52_read_policy);
            if config.osc52_read_policy == Osc52ReadPolicy::Deny {
                self.pending_osc52_read = None;
            }
        }

        self.outgoing_scratch.clear();
        let mut frame_io_errors: Vec<TerminalIoError> = Vec::new();

        // 1. Auto-layout, pixel snapping, core reflow, and debounced backend resize.
        #[cfg(feature = "search")]
        let (search_blocks_focus, prev_search_rect) = (
            self.search_bar.blocks_terminal_focus(),
            self.search_bar.last_rect,
        );
        #[cfg(not(feature = "search"))]
        let (search_blocks_focus, prev_search_rect) = (false, None);

        let mut layout = self.layout_state.layout_and_resize(
            ui,
            self.instance_id,
            config,
            &mut self.terminal,
            &mut self.input,
            now_secs,
            &mut self.input_mapper.pending_focus_lock_settle,
            search_blocks_focus,
            prev_search_rect,
        );
        if let Some(err) = layout.io_error.take() {
            self.record_io_error(err, &mut frame_io_errors);
        }

        // 2. Bounded non-blocking poll_read coalesced into one VT advance -> tick animations ->
        //    collect outgoing VT replies.
        let budget = config.read_budget_bytes_per_frame;
        let mut total_read = 0usize;
        if budget > 0 && !self.input.is_closed() {
            let mut stream_ended = false;
            let mut used_staging = false;
            self.poll_staging.clear();

            while total_read < budget {
                if !used_staging && total_read < self.poll_scratch.len() {
                    let want = (budget - total_read).min(self.poll_scratch.len() - total_read);
                    match self
                        .input
                        .poll_read(&mut self.poll_scratch[total_read..total_read + want])
                    {
                        Ok(0) => break,
                        Ok(n) => {
                            let n = n.min(want);
                            total_read += n;
                        }
                        Err(error) => {
                            if error.is_closed() {
                                stream_ended = true;
                            } else {
                                self.record_io_error(error, &mut frame_io_errors);
                            }
                            break;
                        }
                    }
                } else {
                    if !used_staging {
                        self.poll_staging
                            .extend_from_slice(&self.poll_scratch[..total_read]);
                        used_staging = true;
                    }
                    let want = (budget - total_read).min(self.poll_scratch.len());
                    match self.input.poll_read(&mut self.poll_scratch[..want]) {
                        Ok(0) => break,
                        Ok(n) => {
                            let n = n.min(want);
                            total_read += n;
                            self.poll_staging.extend_from_slice(&self.poll_scratch[..n]);
                        }
                        Err(error) => {
                            if error.is_closed() {
                                stream_ended = true;
                            } else {
                                self.record_io_error(error, &mut frame_io_errors);
                            }
                            break;
                        }
                    }
                }
            }

            if total_read > 0 {
                let chunk = if used_staging {
                    &self.poll_staging[..total_read]
                } else {
                    &self.poll_scratch[..total_read]
                };
                self.terminal.advance_with_time(chunk, safe_now);
            }

            if total_read >= budget {
                ui.ctx().request_repaint();
            } else if !stream_ended && config.idle_poll_interval_ms > 0 {
                ui.ctx()
                    .request_repaint_after(Duration::from_millis(u64::from(
                        config.idle_poll_interval_ms,
                    )));
            }
        }
        if total_read == 0 {
            self.terminal.check_sync_timeout(safe_now);
        }
        if let Some(deadline) = self.terminal.sync_output_deadline()
            && let Ok(delay) = Duration::try_from_secs_f64((deadline - safe_now).max(0.0))
        {
            ui.ctx().request_repaint_after(delay);
        }
        #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
        {
            let _advanced = self.terminal.tick_animations(safe_now); // Core marks dirty rows for Stage 6 render.
            if let Some(deadline) = self.terminal.next_animation_deadline()
                && let Ok(delay) = Duration::try_from_secs_f64((deadline - safe_now).max(0.001))
            {
                ui.ctx().request_repaint_after(delay);
            }
        }

        let outgoing = self.terminal.drain_outgoing();
        if !outgoing.is_empty() {
            self.outgoing_scratch.extend_from_slice(&outgoing);
        }

        // 3. Interact with scrollbar overlay before grid event mapping, and isolate search bar
        //    and any open context menu popup.
        #[cfg(feature = "search")]
        let (search_bar_focused, search_bar_actually_focused, search_bar_rect) = {
            let rect = self
                .search_bar
                .active_rect(layout.term_rect, layout.clip_rect);
            if rect.is_none() {
                self.search_bar.clear_hidden_focus();
            }
            (
                self.search_bar.blocks_terminal_focus(),
                self.search_bar.open && self.search_bar.has_focus,
                rect,
            )
        };
        #[cfg(not(feature = "search"))]
        let (search_bar_focused, search_bar_actually_focused, search_bar_rect) =
            (false, false, None);

        let (mouse_protocol, shift_held, secondary_shift_click) = {
            let mouse_protocol = self.terminal.active_modes().mouse_protocol;
            let (shift_held, secondary_shift_click) = ui.input(|i| {
                let any_btn_shift = i.events.iter().any(|ev| {
                    matches!(
                        ev,
                        egui::Event::PointerButton { modifiers, .. } if modifiers.shift
                    )
                });
                let sec_shift = i.events.iter().any(|ev| {
                    matches!(
                        ev,
                        egui::Event::PointerButton {
                            button: egui::PointerButton::Secondary,
                            modifiers,
                            ..
                        } if modifiers.shift
                    )
                });
                (i.modifiers.shift || any_btn_shift, sec_shift)
            });
            (mouse_protocol, shift_held, secondary_shift_click)
        };
        let mouse_captured = mouse_protocol != MouseProtocol::None && !shift_held;
        let menu_mouse_captured = mouse_captured && !secondary_shift_click;
        let popup_id = egui::Popup::default_response_id(&layout.response);
        let context_menu_open = egui::Popup::is_id_open(ui.ctx(), popup_id);

        let scrollbar_frame = crate::widgets::scrollbar::interact_scrollbar(
            ui,
            self.instance_id,
            &mut self.terminal,
            &layout,
            search_bar_rect,
            context_menu_open,
        );
        let scrollbar_rect = scrollbar_frame.map(|s| s.track_rect);
        let scrollbar_dragging = scrollbar_frame.is_some_and(|s| s.dragged);
        let scrollbar_secondary_started_in_track = track_secondary_press_in_rect(
            ui,
            scrollbar_rect,
            &mut self.scrollbar_secondary_press_in_track,
            context_menu_open,
        );
        let scrollbar_pointer_active = scrollbar_frame.is_some_and(|s| s.pointer_active)
            || scrollbar_secondary_started_in_track;

        let input_out = self.input_mapper.process_frame_events(
            ui,
            layout.response.id,
            &layout.response,
            layout.term_rect,
            layout.cell_size,
            layout.cols,
            layout.rows,
            now_secs,
            config,
            &mut self.terminal,
            search_bar_focused,
            search_bar_actually_focused,
            search_bar_rect,
            scrollbar_rect,
            scrollbar_dragging,
            scrollbar_pointer_active,
            context_menu_open,
        );
        if input_out.any_user_activity {
            self.renderer.note_input_activity(now_secs);
        }
        if !input_out.bytes_to_write.is_empty() {
            self.outgoing_scratch
                .extend_from_slice(&input_out.bytes_to_write);
        }
        #[cfg(feature = "search")]
        if input_out.open_search_requested {
            self.search_bar.open_from_action();
        }

        let secondary_press_started_in_search = track_secondary_press_in_rect(
            ui,
            search_bar_rect,
            &mut self.secondary_press_in_search_bar,
            false,
        );

        let pointer_over_search = secondary_press_started_in_search
            || search_bar_rect.is_some_and(|bar_rect| {
                let latest_in_bar = ui
                    .input(|i| i.pointer.latest_pos())
                    .is_some_and(|p| bar_rect.contains(p));
                let interact_in_bar = layout
                    .response
                    .interact_pointer_pos()
                    .is_some_and(|p| bar_rect.contains(p));
                let press_origin_in_bar = ui
                    .input(|i| i.pointer.press_origin())
                    .is_some_and(|p| bar_rect.contains(p));
                let secondary_event_in_bar = ui.input(|i| {
                    i.events.iter().any(|ev| {
                        matches!(
                            ev,
                            egui::Event::PointerButton {
                                button: egui::PointerButton::Secondary,
                                pos,
                                ..
                            } if bar_rect.contains(*pos)
                        )
                    })
                });
                latest_in_bar || interact_in_bar || press_origin_in_bar || secondary_event_in_bar
            });

        // 4. Optional hyperlink hover & activation.
        #[cfg(feature = "hyperlinks")]
        let (hovered_spans, hyperlink_cursor_overridden) = {
            let pointer_pos = ui.input(|i| i.pointer.hover_pos()).filter(|p| {
                !context_menu_open
                    && !scrollbar_dragging
                    && !scrollbar_pointer_active
                    && !search_bar_rect.is_some_and(|r| r.contains(*p))
                    && !scrollbar_rect.is_some_and(|r| r.contains(*p))
            });
            let ctrl_or_cmd_held = ui.input(|i| {
                i.modifiers.ctrl
                    || i.modifiers.command
                    || i.events.iter().any(|ev| {
                        matches!(
                            ev,
                            egui::Event::PointerButton { modifiers, .. }
                                if modifiers.ctrl || modifiers.command
                        )
                    })
            });
            crate::widgets::hyperlink::update_hyperlink_interaction_with_cursor(
                ui,
                &mut self.terminal,
                pointer_pos,
                &layout,
                !mouse_captured,
                input_out.clean_primary_click && !mouse_captured,
                config.hyperlink_require_ctrl,
                ctrl_or_cmd_held,
            )
        };
        #[cfg(not(feature = "hyperlinks"))]
        let hyperlink_cursor_overridden = false;

        // 5. Optional context menu (keep open once opened via Shift+RightClick even after Shift release).
        let allow_menu = context_menu_open
            || (!menu_mouse_captured && !pointer_over_search && !scrollbar_pointer_active);
        if let Some(action) = crate::widgets::context_menu::attach_context_menu(
            &layout.response,
            &self.terminal,
            allow_menu,
        ) {
            match action {
                WidgetAction::CopySelection => {
                    if let Some(sel) = self.terminal.selection_text().filter(|s| !s.is_empty()) {
                        copy_text_everywhere(ui, sel);
                    }
                }
                WidgetAction::PasteClipboard => {
                    if let Some(text) = try_read_native_clipboard() {
                        self.terminal
                            .format_paste_into(&text, &mut self.outgoing_scratch);
                    } else {
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                    }
                }
                WidgetAction::ClearScrollback => {
                    self.terminal.advance(b"\x1b[3J\x1b[2J\x1b[H");
                }
                #[cfg(feature = "search")]
                WidgetAction::OpenSearchBar => {
                    self.search_bar.open_from_action();
                }
                #[cfg(feature = "shell-integration")]
                WidgetAction::JumpPrompt(delta) => {
                    self.terminal.jump_to_prompt(delta);
                }
            }
        }

        let mut events = self.terminal.drain_events();
        for ev in &events {
            match ev {
                TerminalEvent::Bell => {
                    self.renderer
                        .trigger_visual_bell(now_secs, config.visual_bell_duration_ms);
                }
                TerminalEvent::MouseShape(shape) => {
                    self.mouse_cursor_icon = map_mouse_shape(shape);
                }
                TerminalEvent::Progress { state, progress } => match *state {
                    0 => self.progress = None,
                    1..=4 => {
                        self.progress = Some(ProgressIndicator {
                            state: *state,
                            progress: (*progress).min(100),
                        });
                    }
                    _ => {}
                },
                TerminalEvent::ClipboardWrite(text) if config.osc52_write_enabled => {
                    copy_text_everywhere(ui, text.clone());
                }
                TerminalEvent::ClipboardReadPrompt { selection } => {
                    self.pending_osc52_read = Some(*selection);
                }
                _ => {}
            }
        }

        if !hyperlink_cursor_overridden
            && let Some(icon) = self.mouse_cursor_icon
            && layout.response.hovered()
            && !context_menu_open
            && !scrollbar_dragging
            && !scrollbar_pointer_active
        {
            let pointer_over_overlay = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| {
                search_bar_rect.is_some_and(|r| r.contains(p))
                    || scrollbar_rect.is_some_and(|r| r.contains(p))
            });
            if !pointer_over_overlay {
                ui.ctx().set_cursor_icon(icon);
            }
        }

        // 6. 4-stage cached rendering.
        let has_focus = layout.response.has_focus();
        let ime_cursor_rect = self.renderer.render_with_bell_style(
            ui,
            &layout,
            config,
            &mut self.terminal,
            now_secs,
            has_focus,
            &self.input_mapper.ime_preedit,
            #[cfg(feature = "hyperlinks")]
            &hovered_spans,
            config.visual_bell_color,
            config.visual_bell_opacity,
        );

        // 7. Overlaid scrollbar, progress bar, & optional floating search bar.
        let default_fg = {
            let rgb = config.theme.scheme.foreground;
            egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
        };
        let overlay_painter = ui.painter_at(layout.clip_rect);
        if let Some(sb) = scrollbar_frame {
            crate::widgets::scrollbar::paint_scrollbar(&overlay_painter, sb, default_fg);
        }
        if config.show_progress_bar
            && let Some(indicator) = self.progress
        {
            crate::widgets::progress_bar::paint_progress_bar(
                ui,
                &overlay_painter,
                layout.term_rect,
                layout.clip_rect,
                layout.pixels_per_point,
                indicator,
                default_fg,
                safe_now,
            );
        }

        #[cfg(feature = "search")]
        if self.search_bar.open
            && crate::widgets::search_bar::show_search_bar(
                ui,
                self.instance_id,
                layout.term_rect,
                layout.clip_rect,
                &mut self.search_bar,
                &mut self.terminal,
            )
        {
            layout.response.request_focus();
            self.input_mapper.note_focus_requested();
        }

        // 8. Sync final DEC ?1004 focus state (after search bar close/open), flush coalesced
        //    outgoing bytes once per frame, and report IME area.
        #[cfg(feature = "search")]
        let final_search_bar_focused = self.search_bar.blocks_terminal_focus();
        #[cfg(not(feature = "search"))]
        let final_search_bar_focused = false;
        let final_has_focus = layout.response.has_focus() && !final_search_bar_focused;
        self.input_mapper.sync_focus_transition(
            &mut self.terminal,
            final_has_focus,
            &mut self.outgoing_scratch,
        );
        let post_outgoing = self.terminal.drain_outgoing();
        if !post_outgoing.is_empty() {
            self.outgoing_scratch.extend_from_slice(&post_outgoing);
        }
        if !self.outgoing_scratch.is_empty() {
            if let Err(err) = self.input.write_bytes(&self.outgoing_scratch) {
                self.record_io_error(err, &mut frame_io_errors);
            }
            self.outgoing_scratch.clear();
        }
        for err in frame_io_errors {
            events.push(TerminalEvent::IoError(err.to_string()));
        }

        if !final_search_bar_focused && ui.memory(|m| m.owns_ime_events(layout.response.id)) {
            let to_global = ui
                .ctx()
                .layer_transform_to_global(ui.layer_id())
                .unwrap_or_default();
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect: to_global * layout.term_rect,
                    cursor_rect: to_global * ime_cursor_rect,
                    should_interrupt_composition: false,
                });
            });
        }

        TerminalResponse {
            response: layout.response,
            cols: layout.cols,
            rows: layout.rows,
            cell_size: layout.cell_size,
            effective_font_size: layout.effective_font_size,
            resized_this_frame: layout.resized_this_frame,
            events,
        }
    }
}

/// [`egui::Widget`] adapter wrapping a mutable [`TerminalRuntime`] (and optional external
/// [`TerminalConfig`]) for ergonomic `ui.add(TerminalWidget::new(&mut runtime))` usage.
pub struct TerminalWidget<'a, I: TerminalInput> {
    runtime: &'a mut TerminalRuntime<I>,
    external_config: Option<&'a mut TerminalConfig>,
}

impl<'a, I: TerminalInput> TerminalWidget<'a, I> {
    /// Creates a widget borrowing `runtime` and using `runtime`'s internal configuration.
    pub fn new(runtime: &'a mut TerminalRuntime<I>) -> Self {
        Self {
            runtime,
            external_config: None,
        }
    }

    /// Overrides the configuration with an external mutable [`TerminalConfig`].
    #[must_use]
    pub fn with_config(mut self, config: &'a mut TerminalConfig) -> Self {
        self.external_config = Some(config);
        self
    }

    /// Shows the widget and returns the rich [`TerminalResponse`].
    pub fn show(self, ui: &mut Ui) -> TerminalResponse {
        match self.external_config {
            Some(cfg) => self.runtime.show_with_config(ui, cfg),
            None => self.runtime.show(ui),
        }
    }
}

impl<I: TerminalInput> Widget for TerminalWidget<'_, I> {
    fn ui(self, ui: &mut Ui) -> egui::Response {
        self.show(ui).response
    }
}

fn track_secondary_press_in_rect(
    ui: &Ui,
    rect: Option<egui::Rect>,
    latched: &mut bool,
    suppress_latch: bool,
) -> bool {
    let mut started_in_rect = false;
    let mut saw_secondary_event = false;
    ui.input(|i| {
        for ev in &i.events {
            if let egui::Event::PointerButton {
                button: egui::PointerButton::Secondary,
                pressed,
                pos,
                ..
            } = ev
            {
                saw_secondary_event = true;
                if *pressed {
                    *latched = !suppress_latch && rect.is_some_and(|r| r.contains(*pos));
                    if *latched {
                        started_in_rect = true;
                    }
                } else {
                    if *latched {
                        started_in_rect = true;
                    }
                    *latched = false;
                }
            }
        }
    });
    if !saw_secondary_event && *latched && ui.input(|i| !i.pointer.secondary_down()) {
        *latched = false;
    }
    started_in_rect
}

fn map_mouse_shape(shape: &str) -> Option<egui::CursorIcon> {
    let trimmed = shape.trim().trim_start_matches('=');
    for raw_token in trimmed.split(',') {
        let token = raw_token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some(icon) = map_single_mouse_shape(token) {
            return Some(icon);
        }
    }
    None
}

fn map_single_mouse_shape(token: &str) -> Option<egui::CursorIcon> {
    if token.len() > 32 || !token.is_ascii() {
        return None;
    }
    let mut buf = [0u8; 32];
    for (dst, &src) in buf.iter_mut().zip(token.as_bytes()) {
        *dst = src.to_ascii_lowercase();
    }
    match &buf[..token.len()] {
        b"default" | b"left_ptr" | b"arrow" => Some(egui::CursorIcon::Default),
        b"none" => Some(egui::CursorIcon::None),
        b"context-menu" => Some(egui::CursorIcon::ContextMenu),
        b"help" | b"question_arrow" => Some(egui::CursorIcon::Help),
        b"pointer" | b"hand" | b"hand2" => Some(egui::CursorIcon::PointingHand),
        b"progress" => Some(egui::CursorIcon::Progress),
        b"wait" | b"watch" => Some(egui::CursorIcon::Wait),
        b"cell" => Some(egui::CursorIcon::Cell),
        b"crosshair" | b"cross" | b"tcross" => Some(egui::CursorIcon::Crosshair),
        b"text" | b"xterm" | b"ibeam" => Some(egui::CursorIcon::Text),
        b"vertical-text" => Some(egui::CursorIcon::VerticalText),
        b"alias" => Some(egui::CursorIcon::Alias),
        b"copy" => Some(egui::CursorIcon::Copy),
        b"move" | b"fleur" => Some(egui::CursorIcon::Move),
        b"no-drop" => Some(egui::CursorIcon::NoDrop),
        b"not-allowed" | b"pirate" => Some(egui::CursorIcon::NotAllowed),
        b"grab" | b"openhand" => Some(egui::CursorIcon::Grab),
        b"grabbing" | b"closedhand" => Some(egui::CursorIcon::Grabbing),
        b"all-scroll" => Some(egui::CursorIcon::AllScroll),
        b"col-resize" => Some(egui::CursorIcon::ResizeColumn),
        b"row-resize" => Some(egui::CursorIcon::ResizeRow),
        b"ew-resize" | b"sb_h_double_arrow" => Some(egui::CursorIcon::ResizeHorizontal),
        b"ns-resize" | b"sb_v_double_arrow" => Some(egui::CursorIcon::ResizeVertical),
        b"nesw-resize" => Some(egui::CursorIcon::ResizeNeSw),
        b"nwse-resize" => Some(egui::CursorIcon::ResizeNwSe),
        b"n-resize" => Some(egui::CursorIcon::ResizeNorth),
        b"s-resize" => Some(egui::CursorIcon::ResizeSouth),
        b"e-resize" => Some(egui::CursorIcon::ResizeEast),
        b"w-resize" => Some(egui::CursorIcon::ResizeWest),
        b"ne-resize" => Some(egui::CursorIcon::ResizeNorthEast),
        b"nw-resize" => Some(egui::CursorIcon::ResizeNorthWest),
        b"se-resize" => Some(egui::CursorIcon::ResizeSouthEast),
        b"sw-resize" => Some(egui::CursorIcon::ResizeSouthWest),
        b"zoom-in" => Some(egui::CursorIcon::ZoomIn),
        b"zoom-out" => Some(egui::CursorIcon::ZoomOut),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use egui::{
        CentralPanel, Context, CursorIcon, Event, Frame, Modifiers, RawInput, Rect, pos2, vec2,
    };
    use egui_xterm_core::TerminalSize;

    #[derive(Default)]
    struct FailingBackend {
        fail_resize: bool,
        fail_write: bool,
        pending_read: Vec<u8>,
    }

    impl TerminalInput for FailingBackend {
        fn poll_read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalIoError> {
            if self.pending_read.is_empty() {
                return Ok(0);
            }
            let n = buf.len().min(self.pending_read.len());
            buf[..n].copy_from_slice(&self.pending_read[..n]);
            self.pending_read.drain(..n);
            Ok(n)
        }

        fn write_bytes(&mut self, _bytes: &[u8]) -> Result<(), TerminalIoError> {
            if self.fail_write {
                Err(TerminalIoError::message("write failed"))
            } else {
                Ok(())
            }
        }

        fn resize(&mut self, _size: TerminalSize) -> Result<(), TerminalIoError> {
            if self.fail_resize {
                Err(TerminalIoError::message("resize failed"))
            } else {
                Ok(())
            }
        }

        fn is_closed(&self) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct ChunkedCoalescingBackend {
        chunks: VecDeque<Result<Vec<u8>, TerminalIoError>>,
        poll_calls: usize,
        write_calls: usize,
        written_batches: Vec<Vec<u8>>,
        closed: bool,
    }

    impl TerminalInput for ChunkedCoalescingBackend {
        fn poll_read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalIoError> {
            self.poll_calls += 1;
            let Some(next) = self.chunks.front_mut() else {
                return if self.closed {
                    Err(TerminalIoError::Closed)
                } else {
                    Ok(0)
                };
            };
            match next {
                Ok(data) => {
                    if data.is_empty() {
                        self.chunks.pop_front();
                        return Ok(0);
                    }
                    let n = buf.len().min(data.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    data.drain(..n);
                    if data.is_empty() {
                        self.chunks.pop_front();
                    }
                    Ok(n)
                }
                Err(_) => {
                    let err = match self.chunks.pop_front() {
                        Some(Err(e)) => e,
                        _ => TerminalIoError::Closed,
                    };
                    if err.is_closed() {
                        self.closed = true;
                    }
                    Err(err)
                }
            }
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), TerminalIoError> {
            self.write_calls += 1;
            self.written_batches.push(bytes.to_vec());
            Ok(())
        }

        fn is_closed(&self) -> bool {
            self.closed && self.chunks.is_empty()
        }
    }

    #[test]
    fn runtime_records_and_takes_non_fatal_resize_and_write_io_errors() {
        let ctx = Context::default();
        let backend = FailingBackend {
            fail_resize: true,
            fail_write: false,
            pending_read: b"\x1b[5n".to_vec(),
        };
        let mut rt = TerminalRuntime::new(backend);
        assert!(rt.last_io_error().is_none());

        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 480.0))),
            time: Some(0.1),
            ..Default::default()
        };
        let mut events1 = Vec::new();
        let out = ctx.run_ui(raw, |ui| {
            events1 = rt.show(ui).events;
        });
        out.drop_without_applying_deltas();

        assert!(events1.contains(&TerminalEvent::IoError("resize failed".to_owned())));
        assert_eq!(
            rt.last_io_error().map(ToString::to_string).as_deref(),
            Some("resize failed")
        );
        assert_eq!(
            rt.take_io_error().map(|e| e.to_string()).as_deref(),
            Some("resize failed")
        );
        assert!(rt.last_io_error().is_none());

        rt.input_mut().fail_resize = false;
        rt.input_mut().fail_write = true;
        rt.input_mut().pending_read.extend_from_slice(b"\x1b[5n");

        let raw2 = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 480.0))),
            time: Some(0.2),
            ..Default::default()
        };
        let mut events2 = Vec::new();
        let out2 = ctx.run_ui(raw2, |ui| {
            events2 = rt.show(ui).events;
        });
        out2.drop_without_applying_deltas();

        assert!(events2.contains(&TerminalEvent::IoError("write failed".to_owned())));
        assert_eq!(
            rt.take_io_error().map(|e| e.to_string()).as_deref(),
            Some("write failed")
        );
        assert!(rt.last_io_error().is_none());
    }

    #[test]
    fn runtime_coalesces_poll_read_writes_and_surfaces_poll_read_errors() {
        let ctx = Context::default();
        let mut backend = ChunkedCoalescingBackend::default();
        // Split a 3-byte UTF-8 sequence ("你" = E4 BD A0) and an OSC 0 title across small chunks.
        backend.chunks.push_back(Ok(b"\x1b]0;Co".to_vec()));
        backend.chunks.push_back(Ok(b"al\x07\xE4".to_vec()));
        backend.chunks.push_back(Ok(b"\xBD\xA0".to_vec()));
        backend.chunks.push_back(Ok(b"\x1b[?1004h\x1b[5n".to_vec()));
        let mut rt = TerminalRuntime::new(backend);

        // Frame 1: multi-chunk poll_read coalesced; VT reply + focus-in report coalesced into 1 write_bytes.
        let raw1 = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 480.0))),
            focused: true,
            time: Some(0.1),
            events: vec![
                Event::PointerButton {
                    pos: pos2(40.0, 40.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: pos2(40.0, 40.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
                Event::Text("Z".to_owned()),
            ],
            ..Default::default()
        };
        let out1 = ctx.run_ui(raw1, |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                drop(rt.show(ui));
            });
        });
        out1.drop_without_applying_deltas();
        assert_eq!(rt.terminal().title(), "Coal");
        assert_eq!(rt.terminal().visible_line_text(0).as_deref(), Some("你"));
        assert!(rt.input().poll_calls >= 4);
        assert_eq!(rt.input().write_calls, 1);
        assert_eq!(rt.input().written_batches[0], b"\x1b[0n\x1b[IZ");

        // Non-Closed poll_read error surfaces in last_io_error & TerminalEvent::IoError.
        rt.input_mut()
            .chunks
            .push_back(Err(TerminalIoError::message("EIO read failure")));
        let mut events_err = Vec::new();
        let out2 = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 480.0))),
                time: Some(0.2),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    events_err = rt.show(ui).events;
                });
            },
        );
        out2.drop_without_applying_deltas();
        assert!(events_err.contains(&TerminalEvent::IoError("EIO read failure".to_owned())));
        assert_eq!(
            rt.take_io_error().map(|e| e.to_string()).as_deref(),
            Some("EIO read failure")
        );

        // TerminalIoError::Closed must NOT populate last_io_error or TerminalEvent::IoError.
        rt.input_mut()
            .chunks
            .push_back(Err(TerminalIoError::Closed));
        let mut events_closed = Vec::new();
        let out3 = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 480.0))),
                time: Some(0.3),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    events_closed = rt.show(ui).events;
                });
            },
        );
        out3.drop_without_applying_deltas();
        assert!(rt.last_io_error().is_none());
        assert!(
            !events_closed
                .iter()
                .any(|e| matches!(e, TerminalEvent::IoError(_)))
        );
    }

    #[test]
    fn zero_alloc_mouse_shape_and_hyperlink_precedence() {
        assert_eq!(map_mouse_shape(""), None);
        assert_eq!(map_mouse_shape("none"), Some(CursorIcon::None));
        assert_eq!(map_mouse_shape("copy"), Some(CursorIcon::Copy));
        assert_eq!(
            map_mouse_shape("ew-resize"),
            Some(CursorIcon::ResizeHorizontal)
        );
        assert_eq!(map_mouse_shape("=Zoom-In"), Some(CursorIcon::ZoomIn));
        assert_eq!(
            map_mouse_shape("not-a-cursor, pointer, hand"),
            Some(CursorIcon::PointingHand)
        );

        #[cfg(feature = "hyperlinks")]
        {
            let ctx = Context::default();
            let mut rt = TerminalRuntime::new(egui_xterm_core::RingBufferInput::from_bytes(
                b"\x1b]22;crosshair\x07\x1b]8;;https://example.com\x07link\x1b]8;;\x07\r\n",
            ));
            rt.config_mut().hyperlink_require_ctrl = true;
            let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 400.0));

            // Frame 1: ingest OSC 22 + OSC 8 link.
            let out1 = ctx.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    time: Some(0.0),
                    ..Default::default()
                },
                |ui| {
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        drop(rt.show(ui));
                    });
                },
            );
            out1.drop_without_applying_deltas();

            // Frame 2: hover over cell (1, 0) without Ctrl -> OSC 22 Crosshair must win.
            let out2 = ctx.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    time: Some(0.1),
                    events: vec![Event::PointerMoved(pos2(12.0, 8.0))],
                    ..Default::default()
                },
                |ui| {
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        drop(rt.show(ui));
                    });
                },
            );
            assert_eq!(out2.platform_output.cursor_icon, CursorIcon::Crosshair);
            out2.drop_without_applying_deltas();

            // Frame 3: hover over cell (1, 0) WITH Ctrl -> PointingHand must win.
            let out3 = ctx.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    time: Some(0.2),
                    events: vec![
                        Event::ModifiersChanged(Modifiers::CTRL),
                        Event::PointerMoved(pos2(12.0, 8.0)),
                    ],
                    ..Default::default()
                },
                |ui| {
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        drop(rt.show(ui));
                    });
                },
            );
            assert_eq!(out3.platform_output.cursor_icon, CursorIcon::PointingHand);
            out3.drop_without_applying_deltas();
        }
    }

    #[test]
    fn osc52_policies_progress_and_lock_modifiers_roundtrip() {
        let ctx = Context::default();
        let mut rt = TerminalRuntime::new(egui_xterm_core::RingBufferInput::from_bytes(
            b"\x1b]52;c;aGk=\x07\x1b]9;4;1;45\x07",
        ));
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 400.0));

        // Frame 1: OSC 52 write allowed + OSC 9;4 normal 45%.
        let out1 = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen),
                time: Some(0.0),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    drop(rt.show(ui));
                });
            },
        );
        assert!(
            out1.platform_output
                .commands
                .iter()
                .any(|c| matches!(c, egui::OutputCommand::CopyText(s) if s == "hi"))
        );
        out1.drop_without_applying_deltas();
        assert_eq!(
            rt.progress(),
            Some(ProgressIndicator {
                state: 1,
                progress: 45,
            })
        );

        // Disable osc52_write_enabled and switch to Prompt read policy.
        rt.config_mut().osc52_write_enabled = false;
        rt.config_mut().osc52_read_policy = Osc52ReadPolicy::Prompt;
        rt.input_mut()
            .push(b"\x1b]52;c;bm8=\x07\x1b]52;c;?\x07\x1b]9;4;0\x07");

        let out2 = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen),
                time: Some(0.1),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    drop(rt.show(ui));
                });
            },
        );
        assert!(
            !out2
                .platform_output
                .commands
                .iter()
                .any(|c| matches!(c, egui::OutputCommand::CopyText(_)))
        );
        out2.drop_without_applying_deltas();
        assert_eq!(rt.progress(), None);
        assert_eq!(rt.pending_osc52_read(), Some(ClipboardSelection::Clipboard));
        assert!(rt.approve_osc52_read("secret"));
        assert_eq!(rt.pending_osc52_read(), None);
        assert_eq!(rt.input_mut().drain_written(), b"\x1b]52;c;c2VjcmV0\x1b\\");

        rt.set_lock_modifiers(true, true);
        assert!(rt.caps_lock());
        assert!(rt.num_lock());
    }

    #[test]
    fn runtime_paints_progress_bar_overlay_and_schedules_indeterminate_repaints() {
        use std::sync::mpsc;

        let ctx = Context::default();
        let (tx, rx) = mpsc::channel();
        ctx.set_repaint_observer(move |info| {
            if tx.send(info.delay).is_err() {
                // Receiver dropped after test assertions complete.
            }
        });
        let mut rt = TerminalRuntime::new(egui_xterm_core::RingBufferInput::from_bytes(
            b"\x1b]9;4;3\x07",
        ));
        rt.config_mut().cursor_blink = false;
        rt.config_mut().idle_poll_interval_ms = 0;
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 400.0));

        // Frame 1: state=3 indeterminate schedules ~16ms repaint and emits top-edge Rect shapes.
        let out1 = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen),
                time: Some(0.1),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    drop(rt.show(ui));
                });
            },
        );
        let delays1: Vec<Duration> = rx.try_iter().collect();
        let bar_rects1: usize = out1
            .shapes
            .iter()
            .filter(|cs| {
                matches!(
                    &cs.shape,
                    egui::Shape::Rect(r)
                        if r.rect.min.y == 0.0 && r.rect.height() <= 4.0 && r.rect.width() > 50.0
                )
            })
            .count();
        out1.drop_without_applying_deltas();
        assert_eq!(bar_rects1, 2);
        assert!(
            delays1
                .iter()
                .any(|d| *d > Duration::ZERO && *d <= Duration::from_millis(20))
        );

        // show_progress_bar = false suppresses bar shapes and indeterminate repaint.
        while rx.try_recv().is_ok() {}
        rt.config_mut().show_progress_bar = false;
        let out2 = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen),
                time: Some(0.2),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    drop(rt.show(ui));
                });
            },
        );
        let delays2: Vec<Duration> = rx.try_iter().collect();
        let bar_rects2: usize = out2
            .shapes
            .iter()
            .filter(|cs| {
                matches!(
                    &cs.shape,
                    egui::Shape::Rect(r)
                        if r.rect.min.y == 0.0 && r.rect.height() <= 4.0 && r.rect.width() > 50.0
                )
            })
            .count();
        out2.drop_without_applying_deltas();
        assert_eq!(bar_rects2, 0);
        assert!(!delays2.iter().any(|d| *d > Duration::ZERO));
    }

    #[cfg(feature = "kitty-graphics")]
    #[test]
    fn runtime_ticks_kitty_animations_and_schedules_deadline_repaint() {
        use std::sync::mpsc;

        let ctx = Context::default();
        let (tx, rx) = mpsc::channel();
        ctx.set_repaint_observer(move |info| {
            if tx.send(info.delay).is_err() {
                // Receiver dropped after test assertions complete.
            }
        });
        let payload = concat!(
            "\x1b_Ga=T,f=32,s=1,v=1,i=50,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=f,f=32,s=1,v=1,i=50,z=100,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=a,i=50,r=1,z=100,s=3,v=2,q=2\x1b\\",
        );
        let mut rt = TerminalRuntime::new(egui_xterm_core::RingBufferInput::from_bytes(
            payload.as_bytes(),
        ));
        rt.config_mut().cursor_blink = false;
        rt.config_mut().idle_poll_interval_ms = 0;
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 400.0));

        let out1 = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen),
                time: Some(1.0),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    drop(rt.show(ui));
                });
            },
        );
        out1.drop_without_applying_deltas();
        let delays1: Vec<Duration> = rx.try_iter().collect();
        assert!(
            delays1
                .iter()
                .any(|d| *d >= Duration::from_millis(90) && *d <= Duration::from_millis(110)),
            "expected ~100ms animation deadline repaint, got {delays1:?}"
        );
        let rev1 = rt.terminal().image_data(50).expect("image 50").revision;

        // Frame 2 after 110ms advances animation frame and bumps image revision.
        let out2 = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen),
                time: Some(1.11),
                ..Default::default()
            },
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    drop(rt.show(ui));
                });
            },
        );
        out2.drop_without_applying_deltas();
        let rev2 = rt.terminal().image_data(50).expect("image 50").revision;
        assert!(
            rev2 > rev1,
            "expected revision bump on animation frame advance"
        );
    }
}
