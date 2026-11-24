//! Translation of `egui` keyboard, text, IME, clipboard, focus, and pointer events into
//! terminal actions and VT byte sequences.

use std::borrow::Cow;

use egui::{
    Event, EventFilter, ImeEvent, Key, Modifiers, MouseWheelUnit, OutputCommand, PointerButton,
    Pos2, Rect, Ui, Vec2,
};
use egui_xterm_core::{
    MouseButton, MouseEncoding, MouseEventKind, MouseInputEvent, MouseModifiers, MouseProtocol,
    SelectionKind, Terminal,
};

use crate::config::{CopyBehavior, TerminalConfig};
use crate::renderer::ImePreeditState;

pub(crate) const TERMINAL_FOCUS_FILTER: EventFilter = EventFilter {
    tab: true,
    horizontal_arrows: true,
    vertical_arrows: true,
    escape: true,
};

/// Persistent per-runtime input, focus, IME, and pointer state.
#[derive(Debug, Default)]
pub(crate) struct InputMapperState {
    was_focused: bool,
    last_reported_focus: Option<bool>,
    pub pending_focus_lock_settle: bool,
    pub ime_preedit: ImePreeditState,
    outgoing_scratch: Vec<u8>,
    caps_lock: bool,
    num_lock: bool,
    scrollbar_press_active: bool,
    left_down_in_grid: bool,
    press_pos: Option<Pos2>,
    drag_started: bool,
    click_count: u8,
    last_click_time: f64,
    last_click_pos: Option<Pos2>,
    active_mouse_button: Option<MouseButton>,
    pressed_mouse_buttons: u8,
    last_reported_cell: Option<(u16, u16)>,
    last_reported_pixel: Option<(u32, u32)>,
    last_pointer_pos: Option<Pos2>,
}

/// Output produced by [`InputMapperState::process_frame_events`].
pub(crate) struct InputFrameOutcome<'a> {
    pub bytes_to_write: Cow<'a, [u8]>,
    #[cfg(feature = "hyperlinks")]
    pub clean_primary_click: bool,
    pub any_user_activity: bool,
    #[cfg(feature = "search")]
    pub open_search_requested: bool,
}

struct InputFrameFlags {
    #[cfg(feature = "hyperlinks")]
    clean_primary_click: bool,
    any_user_activity: bool,
    #[cfg(feature = "search")]
    open_search_requested: bool,
}

impl InputMapperState {
    /// Explicitly updates tracked `CapsLock` and `NumLock` states (e.g. from host OS state).
    pub(crate) const fn set_lock_modifiers(&mut self, caps_lock: bool, num_lock: bool) {
        self.caps_lock = caps_lock;
        self.num_lock = num_lock;
    }

    /// Returns the currently tracked `CapsLock` state.
    pub(crate) const fn caps_lock(&self) -> bool {
        self.caps_lock
    }

    /// Returns the currently tracked `NumLock` state.
    pub(crate) const fn num_lock(&self) -> bool {
        self.num_lock
    }

    /// Marks that focus was requested on this frame so the next frame guards immediate `Escape`/`Tab`.
    #[cfg(feature = "search")]
    pub(crate) fn note_focus_requested(&mut self) {
        self.pending_focus_lock_settle = true;
    }

    /// Emits `DEC ?1004` focus reporting strictly on focus transitions and clears IME preedit on blur.
    pub(crate) fn sync_focus_transition(
        &mut self,
        terminal: &mut Terminal,
        has_focus: bool,
        out_bytes: &mut Vec<u8>,
    ) {
        if has_focus != self.was_focused {
            self.was_focused = has_focus;
            if self.last_reported_focus != Some(has_focus) {
                if let Some(bytes) = terminal.focus_report(has_focus) {
                    out_bytes.extend_from_slice(bytes);
                }
                self.last_reported_focus = Some(has_focus);
            }
            if !has_focus {
                self.ime_preedit.text.clear();
                self.ime_preedit.active_range_chars = None;
            }
        }
    }

    /// Applies focus lock, processes pointer/mouse/keyboard/IME/clipboard events, and updates
    /// selection and IME preedit state, borrowing the reusable `outgoing_scratch` buffer.
    #[expect(clippy::too_many_arguments, reason = "per-frame input coordinator")]
    pub(crate) fn process_frame_events(
        &mut self,
        ui: &Ui,
        id: egui::Id,
        response: &egui::Response,
        term_rect: Rect,
        cell_size: Vec2,
        cols: u16,
        rows: u16,
        now_secs: f64,
        config: &TerminalConfig,
        terminal: &mut Terminal,
        search_bar_focused: bool,
        search_bar_actually_focused: bool,
        search_bar_rect: Option<Rect>,
        scrollbar_rect: Option<Rect>,
        scrollbar_dragging: bool,
        scrollbar_pointer_active: bool,
        context_menu_open: bool,
    ) -> InputFrameOutcome<'_> {
        let mut scratch = std::mem::take(&mut self.outgoing_scratch);
        scratch.clear();
        let flags = self.process_frame_events_into(
            ui,
            id,
            response,
            term_rect,
            cell_size,
            cols,
            rows,
            now_secs,
            config,
            terminal,
            search_bar_focused,
            search_bar_actually_focused,
            search_bar_rect,
            scrollbar_rect,
            scrollbar_dragging,
            scrollbar_pointer_active,
            context_menu_open,
            &mut scratch,
        );
        self.outgoing_scratch = scratch;
        InputFrameOutcome {
            bytes_to_write: Cow::Borrowed(&self.outgoing_scratch),
            #[cfg(feature = "hyperlinks")]
            clean_primary_click: flags.clean_primary_click,
            any_user_activity: flags.any_user_activity,
            #[cfg(feature = "search")]
            open_search_requested: flags.open_search_requested,
        }
    }

    /// Processes frame input events and appends outgoing VT bytes directly into `out_bytes`
    /// without allocating intermediate `Vec<u8>` buffers.
    #[expect(clippy::too_many_arguments, reason = "per-frame input coordinator")]
    fn process_frame_events_into(
        &mut self,
        ui: &Ui,
        id: egui::Id,
        response: &egui::Response,
        term_rect: Rect,
        cell_size: Vec2,
        cols: u16,
        rows: u16,
        now_secs: f64,
        config: &TerminalConfig,
        terminal: &mut Terminal,
        search_bar_focused: bool,
        search_bar_actually_focused: bool,
        search_bar_rect: Option<Rect>,
        scrollbar_rect: Option<Rect>,
        scrollbar_dragging: bool,
        scrollbar_pointer_active: bool,
        context_menu_open: bool,
        out_bytes: &mut Vec<u8>,
    ) -> InputFrameFlags {
        let mut out = InputFrameFlags {
            #[cfg(feature = "hyperlinks")]
            clean_primary_click: false,
            any_user_activity: false,
            #[cfg(feature = "search")]
            open_search_requested: false,
        };

        let raw_events = ui.input(|i| i.events.clone());
        let interact_rect = response.rect;
        let in_interact_grid =
            |pos: Pos2| interact_rect.is_positive() && interact_rect.contains(pos);
        let pointer_in_overlay = |pos: Pos2| {
            search_bar_rect.is_some_and(|r| r.contains(pos))
                || scrollbar_rect.is_some_and(|r| r.contains(pos))
        };

        let (mouse_protocol, mouse_encoding) = {
            let modes = terminal.active_modes();
            (modes.mouse_protocol, modes.mouse_encoding)
        };
        let ppp = crate::layout::sanitize_ppp(ui.ctx().pixels_per_point());

        // Acquire focus on click or initial pointer press/drag inside the terminal grid
        // (excluding overlay widgets like the search bar, scrollbar, and open context menu).
        let primary_pressed_in_grid = !context_menu_open
            && raw_events.iter().any(|ev| {
                matches!(
                    ev,
                    Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed: true,
                        ..
                    } if in_interact_grid(*pos) && !pointer_in_overlay(*pos)
                )
            });
        let mouse_protocol_press_in_grid = !context_menu_open
            && mouse_protocol != MouseProtocol::None
            && raw_events.iter().any(|ev| {
                matches!(
                    ev,
                    Event::PointerButton {
                        pos,
                        button: PointerButton::Secondary | PointerButton::Middle,
                        pressed: true,
                        modifiers,
                    } if !modifiers.shift && in_interact_grid(*pos) && !pointer_in_overlay(*pos)
                )
            });
        let any_pressed_in_grid = !context_menu_open
            && raw_events.iter().any(|ev| {
                matches!(
                    ev,
                    Event::PointerButton {
                        pos,
                        pressed: true,
                        ..
                    } if in_interact_grid(*pos) && !pointer_in_overlay(*pos)
                )
            });
        let interact_in_grid = !context_menu_open
            && response
                .interact_pointer_pos()
                .is_some_and(|p| in_interact_grid(p) && !pointer_in_overlay(p));
        let clicked_in_grid = response.clicked() && interact_in_grid;
        let drag_started_in_grid =
            response.drag_started() && !scrollbar_dragging && interact_in_grid;

        let non_primary_in_search_bar = !context_menu_open
            && !search_bar_focused
            && ui.memory(|m| m.focused().is_none() && m.had_focus_last_frame(id))
            && search_bar_rect.is_some_and(|bar_rect| {
                let mut non_primary_in_bar = false;
                let mut primary_or_outside = false;
                for ev in &raw_events {
                    if let Event::PointerButton { pos, button, .. } = ev {
                        if *button != PointerButton::Primary && bar_rect.contains(*pos) {
                            non_primary_in_bar = true;
                        } else {
                            primary_or_outside = true;
                        }
                    }
                }
                let held_non_primary_in_bar = ui.input(|i| {
                    !i.pointer.primary_down()
                        && i.pointer.any_down()
                        && i.pointer.latest_pos().is_some_and(|p| bar_rect.contains(p))
                });
                (non_primary_in_bar || held_non_primary_in_bar) && !primary_or_outside
            });

        let in_own_scrollbar = if !context_menu_open
            && !search_bar_focused
            && ui.memory(|m| m.focused().is_none() && m.had_focus_last_frame(id))
        {
            if let Some(sb_rect) = scrollbar_rect {
                let in_sb = |p: Pos2| {
                    sb_rect.contains(p) && !search_bar_rect.is_some_and(|r| r.contains(p))
                };
                let mut origin_in_sb = self.scrollbar_press_active
                    || ui.input(|i| i.pointer.press_origin()).is_some_and(in_sb);
                let mut btn_in_sb = false;
                let mut pressed_outside_sb = false;
                for ev in &raw_events {
                    if let Event::PointerButton { pos, pressed, .. } = ev {
                        if *pressed {
                            origin_in_sb = in_sb(*pos);
                            if origin_in_sb {
                                btn_in_sb = true;
                            } else {
                                pressed_outside_sb = true;
                            }
                        } else {
                            if in_sb(*pos) {
                                btn_in_sb = true;
                            } else if !origin_in_sb {
                                pressed_outside_sb = true;
                            }
                            origin_in_sb = false;
                        }
                    }
                }
                let held_in_sb = scrollbar_dragging
                    || self.scrollbar_press_active
                    || (scrollbar_pointer_active
                        && ui.input(|i| i.pointer.press_origin()).is_some_and(in_sb));
                (btn_in_sb || held_in_sb) && !pressed_outside_sb
            } else if scrollbar_pointer_active {
                !raw_events.iter().any(|ev| {
                    matches!(
                        ev,
                        Event::PointerButton {
                            pos,
                            pressed: true,
                            ..
                        } if !in_interact_grid(*pos)
                    )
                })
            } else {
                false
            }
        } else {
            false
        };

        if let Some(sb_rect) = scrollbar_rect {
            let in_sb =
                |p: Pos2| sb_rect.contains(p) && !search_bar_rect.is_some_and(|r| r.contains(p));
            let mut saw_button_ev = false;
            for ev in &raw_events {
                if let Event::PointerButton { pos, pressed, .. } = ev {
                    saw_button_ev = true;
                    self.scrollbar_press_active = *pressed && in_sb(*pos);
                }
            }
            if !saw_button_ev && !ui.input(|i| i.pointer.any_down()) {
                self.scrollbar_press_active = false;
            }
        } else if !scrollbar_pointer_active || !ui.input(|i| i.pointer.any_down()) {
            self.scrollbar_press_active = false;
        }

        if primary_pressed_in_grid
            || mouse_protocol_press_in_grid
            || non_primary_in_search_bar
            || in_own_scrollbar
            || (!search_bar_focused && (clicked_in_grid || drag_started_in_grid))
        {
            response.request_focus();
        }

        let effective_search_bar_focused = search_bar_focused
            && !primary_pressed_in_grid
            && !mouse_protocol_press_in_grid
            && (search_bar_actually_focused || !any_pressed_in_grid);
        let has_focus = ui.memory(|m| m.has_focus(id)) && !effective_search_bar_focused;
        let os_has_focus = response.has_focus() && !effective_search_bar_focused;

        // Lock Tab, arrow keys, and Escape while the terminal has focus.
        let filter = TERMINAL_FOCUS_FILTER;
        if has_focus {
            ui.memory_mut(|m| m.set_focus_lock_filter(id, filter));
            self.pending_focus_lock_settle = !ui.memory(|m| m.had_focus_last_frame(id));
        } else {
            self.pending_focus_lock_settle = false;
        }

        // Emit DEC ?1004 focus reporting strictly on focus transitions.
        // When a visible search bar overlay is open, defer focus-out transitions until after
        // Stage 7 so closing the bar via its "x" button keeps terminal focus without a spurious
        // `\x1b[O` -> `\x1b[I` bounce.
        if os_has_focus || search_bar_rect.is_none() {
            self.sync_focus_transition(terminal, os_has_focus, out_bytes);
        }

        // Process pointer / mouse / wheel events first.
        if mouse_protocol == MouseProtocol::None {
            self.active_mouse_button = None;
            self.pressed_mouse_buttons = 0;
            self.last_reported_cell = None;
            self.last_reported_pixel = None;
        }

        // Reconcile latched pointer-button state if a release event was missed (e.g. released
        // outside the window or while this widget was hidden).
        let mut saw_primary_release = false;
        let mut saw_middle_release = false;
        let mut saw_secondary_release = false;
        for ev in &raw_events {
            if let Event::PointerButton {
                button,
                pressed: false,
                ..
            } = ev
            {
                match button {
                    PointerButton::Primary | PointerButton::Extra1 | PointerButton::Extra2 => {
                        saw_primary_release = true;
                    }
                    PointerButton::Middle => saw_middle_release = true,
                    PointerButton::Secondary => saw_secondary_release = true,
                }
            }
        }
        let (primary_down, middle_down, secondary_down) = ui.input(|i| {
            (
                i.pointer.button_down(PointerButton::Primary),
                i.pointer.button_down(PointerButton::Middle),
                i.pointer.button_down(PointerButton::Secondary),
            )
        });
        if self.left_down_in_grid && !saw_primary_release && !primary_down {
            self.left_down_in_grid = false;
            self.pressed_mouse_buttons &= !mouse_button_bit(MouseButton::Left);
            if self.drag_started {
                terminal.selection_finish();
            } else {
                terminal.selection_clear();
            }
            self.drag_started = false;
            self.press_pos = None;
        }
        if !saw_primary_release && !primary_down {
            self.pressed_mouse_buttons &= !mouse_button_bit(MouseButton::Left);
        }
        if !saw_middle_release && !middle_down {
            self.pressed_mouse_buttons &= !mouse_button_bit(MouseButton::Middle);
        }
        if !saw_secondary_release && !secondary_down {
            self.pressed_mouse_buttons &= !mouse_button_bit(MouseButton::Right);
        }
        if let Some(active_btn) = self.active_mouse_button
            && (self.pressed_mouse_buttons & mouse_button_bit(active_btn)) == 0
        {
            self.active_mouse_button = first_pressed_button(self.pressed_mouse_buttons);
        }
        if !raw_events.is_empty() {
            let frame_shift_held = ui.input(|i| i.modifiers.shift)
                || raw_events.iter().any(|ev| {
                    matches!(
                        ev,
                        Event::PointerButton { modifiers, .. } | Event::MouseWheel { modifiers, .. }
                            if modifiers.shift
                    )
                });
            for ev in &raw_events {
                match ev {
                    Event::PointerMoved(pos) => {
                        self.last_pointer_pos = Some(*pos);
                        if self.left_down_in_grid {
                            if self.press_pos.is_some_and(|p| p.distance(*pos) > 2.0) {
                                self.drag_started = true;
                            }
                            if let Some((col, row)) =
                                clamp_pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                            {
                                terminal.selection_update(usize::from(row), usize::from(col));
                            }
                        } else if !context_menu_open
                            && !scrollbar_dragging
                            && !pointer_in_overlay(*pos)
                            && mouse_protocol != MouseProtocol::None
                            && !frame_shift_held
                            && (in_interact_grid(*pos) || self.active_mouse_button.is_some())
                        {
                            let mods = ui.input(|i| i.modifiers);
                            let cell = if in_interact_grid(*pos) {
                                pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                            } else {
                                clamp_pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                            };
                            if let Some((col, row)) = cell {
                                let (px, py) = pos_to_pixels(*pos, term_rect, ppp);
                                let changed = if mouse_encoding == MouseEncoding::SgrPixels {
                                    self.last_reported_pixel != Some((px, py))
                                } else {
                                    self.last_reported_cell != Some((col, row))
                                };
                                if changed {
                                    self.last_reported_cell = Some((col, row));
                                    self.last_reported_pixel = Some((px, py));
                                    let mev = MouseInputEvent {
                                        kind: MouseEventKind::Motion,
                                        button: self.active_mouse_button,
                                        col,
                                        row,
                                        modifiers: to_mouse_mods(mods),
                                    };
                                    terminal.encode_mouse_event_pixels_into(mev, px, py, out_bytes);
                                }
                            }
                        }
                    }
                    Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    } => {
                        self.last_pointer_pos = Some(*pos);
                        let in_bounds = in_interact_grid(*pos);
                        let over_overlay = pointer_in_overlay(*pos);
                        let bypass_mouse_tracking = modifiers.shift;

                        if !*pressed {
                            let m_btn = to_mouse_button(*button);
                            let m_bit = mouse_button_bit(m_btn);
                            if *button == PointerButton::Primary && self.left_down_in_grid {
                                self.left_down_in_grid = false;
                                self.pressed_mouse_buttons &= !m_bit;
                                if self.drag_started {
                                    if let Some((col, row)) =
                                        clamp_pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                                    {
                                        terminal
                                            .selection_update(usize::from(row), usize::from(col));
                                    }
                                    terminal.selection_finish();
                                    if terminal.has_non_empty_selection()
                                        && let Some(sel) =
                                            terminal.selection_text().filter(|s| !s.is_empty())
                                    {
                                        ui.ctx().send_cmd(OutputCommand::TextSelectionSettled(sel));
                                    }
                                } else if self.click_count == 1 && !context_menu_open {
                                    terminal.selection_clear();
                                    #[cfg(feature = "hyperlinks")]
                                    if in_bounds && !over_overlay && !scrollbar_pointer_active {
                                        out.clean_primary_click = true;
                                    }
                                    #[cfg(not(feature = "hyperlinks"))]
                                    let (_in_bounds, _over_overlay, _scrollbar_pointer_active) =
                                        (in_bounds, over_overlay, scrollbar_pointer_active);
                                }
                                continue;
                            }
                            if self.active_mouse_button == Some(m_btn)
                                || (self.pressed_mouse_buttons & m_bit) != 0
                            {
                                self.pressed_mouse_buttons &= !m_bit;
                                if self.active_mouse_button == Some(m_btn) {
                                    self.active_mouse_button =
                                        first_pressed_button(self.pressed_mouse_buttons);
                                }
                                if mouse_protocol != MouseProtocol::None {
                                    let cell = if in_bounds {
                                        pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                                    } else {
                                        clamp_pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                                    };
                                    if let Some((col, row)) = cell {
                                        let (px, py) = pos_to_pixels(*pos, term_rect, ppp);
                                        let mev = MouseInputEvent {
                                            kind: MouseEventKind::Release,
                                            button: Some(m_btn),
                                            col,
                                            row,
                                            modifiers: to_mouse_mods(*modifiers),
                                        };
                                        terminal
                                            .encode_mouse_event_pixels_into(mev, px, py, out_bytes);
                                    }
                                }
                            }
                            continue;
                        }

                        if !in_bounds
                            || over_overlay
                            || scrollbar_dragging
                            || scrollbar_pointer_active
                            || context_menu_open
                        {
                            continue;
                        }

                        if mouse_protocol != MouseProtocol::None && !bypass_mouse_tracking {
                            if let Some((col, row)) =
                                pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                            {
                                let m_btn = to_mouse_button(*button);
                                self.active_mouse_button = Some(m_btn);
                                self.pressed_mouse_buttons |= mouse_button_bit(m_btn);
                                let (px, py) = pos_to_pixels(*pos, term_rect, ppp);
                                self.last_reported_cell = Some((col, row));
                                self.last_reported_pixel = Some((px, py));
                                let mev = MouseInputEvent {
                                    kind: MouseEventKind::Press,
                                    button: Some(m_btn),
                                    col,
                                    row,
                                    modifiers: to_mouse_mods(*modifiers),
                                };
                                terminal.encode_mouse_event_pixels_into(mev, px, py, out_bytes);
                            }
                        } else if *button == PointerButton::Primary
                            && let Some((col, row)) =
                                pos_to_cell(*pos, term_rect, cell_size, cols, rows)
                        {
                            self.left_down_in_grid = true;
                            self.press_pos = Some(*pos);
                            self.drag_started = false;

                            let rapid = (now_secs - self.last_click_time) <= 0.4
                                && self.last_click_pos.is_some_and(|p| p.distance(*pos) <= 4.0);
                            self.click_count = if rapid { (self.click_count % 3) + 1 } else { 1 };
                            self.last_click_time = now_secs;
                            self.last_click_pos = Some(*pos);

                            match self.click_count {
                                2 => terminal.selection_word(usize::from(row), usize::from(col)),
                                3 => terminal.selection_line(usize::from(row), usize::from(col)),
                                _ if modifiers.alt => {
                                    terminal.selection_block(usize::from(row), usize::from(col));
                                }
                                _ => terminal.selection_start(
                                    usize::from(row),
                                    usize::from(col),
                                    SelectionKind::Stream,
                                ),
                            }
                        }
                    }
                    Event::MouseWheel {
                        unit,
                        delta,
                        modifiers,
                        ..
                    } => {
                        if context_menu_open {
                            continue;
                        }
                        let hover_pos = ui
                            .input(|i| i.pointer.hover_pos())
                            .or(self.last_pointer_pos);
                        if hover_pos.is_some_and(|p| search_bar_rect.is_some_and(|r| r.contains(p)))
                        {
                            continue;
                        }
                        let pointer_inside = hover_pos.is_some_and(in_interact_grid);
                        if !pointer_inside && (!has_focus || hover_pos.is_some()) {
                            continue;
                        }
                        let (col, row) = hover_pos
                            .and_then(|p| pos_to_cell(p, term_rect, cell_size, cols, rows))
                            .unwrap_or((0, 0));
                        let (px, py) =
                            hover_pos.map_or((0, 0), |p| pos_to_pixels(p, term_rect, ppp));

                        let steps_y = match unit {
                            MouseWheelUnit::Line => delta.y.abs().round().max(1.0) as usize,
                            MouseWheelUnit::Point => {
                                (delta.y.abs() / cell_size.y.max(1.0)).round().max(1.0) as usize
                            }
                            MouseWheelUnit::Page => usize::from(rows.max(1)),
                        };

                        if (mouse_protocol != MouseProtocol::None && !modifiers.shift)
                            || (terminal.is_alt_screen() && terminal.active_modes().alt_scroll)
                        {
                            if delta.y.abs() > f32::EPSILON {
                                let btn = if delta.y > 0.0 {
                                    MouseButton::WheelUp
                                } else {
                                    MouseButton::WheelDown
                                };
                                for _ in 0..steps_y.min(16) {
                                    let mev = MouseInputEvent {
                                        kind: MouseEventKind::Press,
                                        button: Some(btn),
                                        col,
                                        row,
                                        modifiers: to_mouse_mods(*modifiers),
                                    };
                                    terminal.encode_mouse_event_pixels_into(mev, px, py, out_bytes);
                                }
                            } else if delta.x.abs() > f32::EPSILON
                                && mouse_protocol != MouseProtocol::None
                            {
                                let btn = if delta.x > 0.0 {
                                    MouseButton::WheelLeft
                                } else {
                                    MouseButton::WheelRight
                                };
                                let mev = MouseInputEvent {
                                    kind: MouseEventKind::Press,
                                    button: Some(btn),
                                    col,
                                    row,
                                    modifiers: to_mouse_mods(*modifiers),
                                };
                                terminal.encode_mouse_event_pixels_into(mev, px, py, out_bytes);
                            }
                        } else if delta.y.abs() > f32::EPSILON {
                            let signed_lines = if delta.y > 0.0 {
                                steps_y as i32
                            } else {
                                -(steps_y as i32)
                            };
                            terminal.scroll_display(signed_lines);
                        }
                    }
                    Event::WindowFocused(focused)
                        if has_focus && self.last_reported_focus != Some(*focused) =>
                    {
                        if let Some(bytes) = terminal.focus_report(*focused) {
                            out_bytes.extend_from_slice(bytes);
                        }
                        self.last_reported_focus = Some(*focused);
                    }
                    _ => {}
                }
            }
        }

        if !has_focus || raw_events.is_empty() || context_menu_open {
            return out;
        }

        let is_mac = ui.ctx().os().is_mac();
        let filtered_events = ui.input(|i| i.filtered_events(&filter));
        let mouse_protocol = terminal.active_modes().mouse_protocol;
        let shift_held = ui.input(|i| i.modifiers.shift);
        let mut skip_next_text = false;

        for (idx, ev) in filtered_events.iter().enumerate() {
            match ev {
                Event::Copy => {
                    out.any_user_activity = true;
                    let protocol_handled = if !is_mac {
                        #[cfg(feature = "kitty-keyboard")]
                        let kitty_active = !terminal.kitty_keyboard_flags().is_empty();
                        #[cfg(not(feature = "kitty-keyboard"))]
                        let kitty_active = false;

                        let mok_always = !kitty_active && terminal.modify_other_keys_level() >= 2;
                        let should_send_key = (kitty_active || mok_always)
                            && match config.effective_copy_behavior() {
                                CopyBehavior::AlwaysSendEtx => true,
                                CopyBehavior::CopyIfSelectionElseSendEtx => {
                                    !terminal.has_non_empty_selection()
                                }
                                CopyBehavior::AlwaysCopy => false,
                            };
                        if should_send_key {
                            #[cfg(feature = "kitty-keyboard")]
                            if kitty_active {
                                let flags = terminal.kitty_keyboard_flags();
                                let modes = *terminal.active_modes();
                                encode_egui_key_kitty_into(
                                    flags,
                                    &modes,
                                    Key::C,
                                    true,
                                    false,
                                    Modifiers::CTRL,
                                    None,
                                    self.caps_lock,
                                    self.num_lock,
                                    out_bytes,
                                )
                            } else {
                                encode_egui_modify_other_keys_into(
                                    terminal,
                                    Key::C,
                                    Modifiers::CTRL,
                                    out_bytes,
                                )
                            }
                            #[cfg(not(feature = "kitty-keyboard"))]
                            if mok_always {
                                encode_egui_modify_other_keys_into(
                                    terminal,
                                    Key::C,
                                    Modifiers::CTRL,
                                    out_bytes,
                                )
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !protocol_handled {
                        handle_copy_shortcut(ui, config, terminal, is_mac, true, out_bytes);
                    }
                }
                Event::Cut => {
                    out.any_user_activity = true;
                    if !is_mac {
                        out_bytes.push(0x18);
                    }
                }
                Event::Paste(text) => {
                    out.any_user_activity = true;
                    terminal.format_paste_into(text, out_bytes);
                }
                Event::MiddleClickPaste { text, pos } => {
                    let allow_middle_paste = mouse_protocol == MouseProtocol::None || shift_held;
                    if allow_middle_paste && in_interact_grid(*pos) && !pointer_in_overlay(*pos) {
                        out.any_user_activity = true;
                        terminal.format_paste_into(text, out_bytes);
                    }
                }
                Event::Ime(ime_ev) => {
                    out.any_user_activity = true;
                    match ime_ev {
                        ImeEvent::Preedit {
                            text,
                            active_range_chars,
                        } => {
                            if text.is_empty() {
                                self.ime_preedit.text.clear();
                                self.ime_preedit.active_range_chars = None;
                            } else {
                                self.ime_preedit.text.clone_from(text);
                                self.ime_preedit.active_range_chars = active_range_chars.clone();
                            }
                        }
                        ImeEvent::Commit(text) => {
                            self.ime_preedit.text.clear();
                            self.ime_preedit.active_range_chars = None;
                            if !text.is_empty() {
                                out_bytes.extend_from_slice(text.as_bytes());
                            }
                        }
                        ImeEvent::DeleteSurrounding { .. } => {}
                        _ => {}
                    }
                }
                Event::Key {
                    key,
                    pressed,
                    repeat,
                    modifiers,
                    ..
                } => {
                    #[cfg(not(feature = "kitty-keyboard"))]
                    let _unused_repeat = repeat;
                    // Check for an immediately following Event::Text so Kitty Flag 16 and legacy
                    // shortcut suppression stay synchronized without double-sending.
                    let paired_text = if *pressed {
                        match filtered_events.get(idx + 1) {
                            Some(Event::Text(txt)) => Some(txt.as_str()),
                            _ => None,
                        }
                    } else {
                        None
                    };

                    if *pressed && !*repeat {
                        match key {
                            Key::CapsLock => self.caps_lock = !self.caps_lock,
                            Key::NumLock => self.num_lock = !self.num_lock,
                            _ => {}
                        }
                    }
                    // Heuristic sync for CapsLock when OS lock state changed outside focus.
                    if *pressed
                        && is_alpha_key(*key)
                        && !modifiers.shift
                        && !modifiers.ctrl
                        && !modifiers.alt
                        && !modifiers.mac_cmd
                        && !modifiers.command
                        && let Some(txt) = paired_text
                    {
                        let mut chars = txt.chars();
                        if let (Some(ch), None) = (chars.next(), chars.next()) {
                            if ch.is_ascii_uppercase() {
                                self.caps_lock = true;
                            } else if ch.is_ascii_lowercase() {
                                self.caps_lock = false;
                            }
                        }
                    }

                    if *pressed && *key == Key::F && modifiers.ctrl && modifiers.shift {
                        #[cfg(feature = "search")]
                        {
                            out.open_search_requested = true;
                            skip_next_text = paired_text.is_some();
                            continue;
                        }
                    }

                    #[cfg(feature = "kitty-keyboard")]
                    let kitty_active = !terminal.kitty_keyboard_flags().is_empty();
                    #[cfg(not(feature = "kitty-keyboard"))]
                    let kitty_active = false;
                    let mok_level = terminal.modify_other_keys_level();

                    // Handle direct Key::C + CTRL when not already translated into Event::Copy.
                    if *pressed
                        && *key == Key::C
                        && modifiers.ctrl
                        && !modifiers.shift
                        && !modifiers.alt
                        && !modifiers.mac_cmd
                    {
                        let protocol_captures_etx = kitty_active || mok_level >= 2;
                        let allow_copy_shortcut = !protocol_captures_etx
                            || (config.effective_copy_behavior() != CopyBehavior::AlwaysSendEtx
                                && terminal.has_non_empty_selection());
                        if allow_copy_shortcut {
                            out.any_user_activity = true;
                            handle_copy_shortcut(ui, config, terminal, false, false, out_bytes);
                            skip_next_text = paired_text.is_some();
                            continue;
                        }
                    }

                    // Handle Ctrl+Shift+C / Ctrl+Shift+V when Kitty keyboard isn't intercepting them.
                    if *pressed
                        && !kitty_active
                        && modifiers.ctrl
                        && modifiers.shift
                        && !modifiers.alt
                    {
                        if *key == Key::C {
                            if terminal.has_non_empty_selection()
                                && let Some(sel) =
                                    terminal.selection_text().filter(|s| !s.is_empty())
                            {
                                copy_text_everywhere(ui, sel);
                            }
                            skip_next_text = paired_text.is_some();
                            continue;
                        }
                        if *key == Key::V {
                            if let Some(text) = try_read_native_clipboard() {
                                terminal.format_paste_into(&text, out_bytes);
                            } else {
                                ui.ctx()
                                    .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                            }
                            skip_next_text = paired_text.is_some();
                            continue;
                        }
                    }

                    #[cfg(feature = "kitty-keyboard")]
                    if kitty_active {
                        let flags = terminal.kitty_keyboard_flags();
                        let modes = *terminal.active_modes();
                        if encode_egui_key_kitty_into(
                            flags,
                            &modes,
                            *key,
                            *pressed,
                            *repeat,
                            *modifiers,
                            paired_text,
                            self.caps_lock,
                            self.num_lock,
                            out_bytes,
                        ) {
                            out.any_user_activity = true;
                            skip_next_text = paired_text.is_some();
                            continue;
                        } else if !*pressed
                            || (paired_text.is_some()
                                && !modifiers.ctrl
                                && !modifiers.alt
                                && !modifiers.mac_cmd)
                        {
                            // Suppressed key release, or let Event::Text handle printable char.
                            continue;
                        }
                        // Fall through to legacy encoding when Kitty returns `false` on press
                        // (e.g., Flag 2 only with Ctrl+- / Ctrl+/ / Ctrl+2..7). Kitty retains
                        // strict precedence over `modifyOtherKeys`.
                    }

                    if !*pressed {
                        continue;
                    }

                    if !kitty_active
                        && mok_level > 0
                        && encode_egui_modify_other_keys_into(terminal, *key, *modifiers, out_bytes)
                    {
                        out.any_user_activity = true;
                        skip_next_text = paired_text.is_some();
                        continue;
                    }

                    let modes = *terminal.active_modes();
                    if encode_egui_key_legacy_into(&modes, *key, *modifiers, out_bytes) {
                        out.any_user_activity = true;
                        skip_next_text = paired_text.is_some();
                    }
                }
                Event::Text(text) => {
                    if skip_next_text {
                        skip_next_text = false;
                        continue;
                    }
                    if !text.is_empty() {
                        out.any_user_activity = true;
                        out_bytes.extend_from_slice(text.as_bytes());
                    }
                }
                _ => {}
            }
        }

        out
    }
}

fn handle_copy_shortcut(
    ui: &Ui,
    config: &TerminalConfig,
    terminal: &Terminal,
    is_mac: bool,
    from_copy_event: bool,
    out_bytes: &mut Vec<u8>,
) {
    match config.effective_copy_behavior() {
        CopyBehavior::AlwaysCopy => {
            if terminal.has_non_empty_selection()
                && let Some(sel) = terminal.selection_text().filter(|s| !s.is_empty())
            {
                copy_text_everywhere(ui, sel);
            }
        }
        CopyBehavior::AlwaysSendEtx => {
            if !(from_copy_event && is_mac) {
                out_bytes.push(0x03);
            }
        }
        CopyBehavior::CopyIfSelectionElseSendEtx => {
            if terminal.has_non_empty_selection()
                && let Some(sel) = terminal.selection_text().filter(|s| !s.is_empty())
            {
                copy_text_everywhere(ui, sel);
            } else if !(from_copy_event && is_mac) {
                out_bytes.push(0x03);
            }
        }
    }
}

pub(crate) fn copy_text_everywhere(ui: &Ui, text: String) {
    #[cfg(all(feature = "native-clipboard", not(target_arch = "wasm32")))]
    {
        if let Ok(mut cb) = arboard::Clipboard::new()
            && cb.set_text(text.clone()).is_err()
        {
            // Best-effort native clipboard write; `ui.ctx().copy_text` below is authoritative.
        }
    }
    ui.ctx().copy_text(text);
}

pub(crate) fn try_read_native_clipboard() -> Option<String> {
    #[cfg(all(feature = "native-clipboard", not(target_arch = "wasm32")))]
    {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }
    #[cfg(not(all(feature = "native-clipboard", not(target_arch = "wasm32"))))]
    {
        None
    }
}

fn pos_to_cell(
    pos: Pos2,
    term_rect: Rect,
    cell_size: Vec2,
    cols: u16,
    rows: u16,
) -> Option<(u16, u16)> {
    if cols == 0
        || rows == 0
        || cell_size.x <= 0.0
        || cell_size.y <= 0.0
        || !term_rect.contains(pos)
    {
        return None;
    }
    let col = (((pos.x - term_rect.min.x) / cell_size.x).floor() as u16).min(cols - 1);
    let row = (((pos.y - term_rect.min.y) / cell_size.y).floor() as u16).min(rows - 1);
    Some((col, row))
}

fn pos_to_pixels(pos: Pos2, term_rect: Rect, ppp: f32) -> (u32, u32) {
    let clamped_x = pos.x.clamp(term_rect.min.x, term_rect.max.x);
    let clamped_y = pos.y.clamp(term_rect.min.y, term_rect.max.y);
    let rel_x = ((clamped_x - term_rect.min.x).max(0.0) * ppp).round();
    let rel_y = ((clamped_y - term_rect.min.y).max(0.0) * ppp).round();
    let px = if rel_x.is_finite() {
        rel_x.clamp(0.0, u32::MAX as f32) as u32
    } else {
        0
    };
    let py = if rel_y.is_finite() {
        rel_y.clamp(0.0, u32::MAX as f32) as u32
    } else {
        0
    };
    (px, py)
}

fn clamp_pos_to_cell(
    pos: Pos2,
    term_rect: Rect,
    cell_size: Vec2,
    cols: u16,
    rows: u16,
) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 || cell_size.x <= 0.0 || cell_size.y <= 0.0 {
        return None;
    }
    let rx = ((pos.x - term_rect.min.x) / cell_size.x).floor();
    let ry = ((pos.y - term_rect.min.y) / cell_size.y).floor();
    let col = rx.clamp(0.0, f32::from(cols - 1)) as u16;
    let row = ry.clamp(0.0, f32::from(rows - 1)) as u16;
    Some((col, row))
}

const fn to_mouse_mods(mods: Modifiers) -> MouseModifiers {
    MouseModifiers {
        shift: mods.shift,
        alt: mods.alt,
        ctrl: mods.ctrl,
    }
}

const fn to_mouse_button(btn: PointerButton) -> MouseButton {
    match btn {
        PointerButton::Primary | PointerButton::Extra1 | PointerButton::Extra2 => MouseButton::Left,
        PointerButton::Middle => MouseButton::Middle,
        PointerButton::Secondary => MouseButton::Right,
    }
}

const fn mouse_button_bit(btn: MouseButton) -> u8 {
    match btn {
        MouseButton::Left => 1 << 0,
        MouseButton::Middle => 1 << 1,
        MouseButton::Right => 1 << 2,
        MouseButton::WheelUp => 1 << 3,
        MouseButton::WheelDown => 1 << 4,
        MouseButton::WheelLeft => 1 << 5,
        MouseButton::WheelRight => 1 << 6,
    }
}

const fn first_pressed_button(mask: u8) -> Option<MouseButton> {
    if (mask & mouse_button_bit(MouseButton::Left)) != 0 {
        Some(MouseButton::Left)
    } else if (mask & mouse_button_bit(MouseButton::Middle)) != 0 {
        Some(MouseButton::Middle)
    } else if (mask & mouse_button_bit(MouseButton::Right)) != 0 {
        Some(MouseButton::Right)
    } else {
        None
    }
}

const fn is_egui_modifier_key(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
            | Key::CapsLock
            | Key::NumLock
            | Key::ScrollLock
    )
}

const fn is_alpha_key(key: Key) -> bool {
    matches!(
        key,
        Key::A
            | Key::B
            | Key::C
            | Key::D
            | Key::E
            | Key::F
            | Key::G
            | Key::H
            | Key::I
            | Key::J
            | Key::K
            | Key::L
            | Key::M
            | Key::N
            | Key::O
            | Key::P
            | Key::Q
            | Key::R
            | Key::S
            | Key::T
            | Key::U
            | Key::V
            | Key::W
            | Key::X
            | Key::Y
            | Key::Z
    )
}

#[cfg(feature = "kitty-keyboard")]
#[expect(clippy::too_many_arguments, reason = "kitty keyboard input encoder")]
fn encode_egui_key_kitty_into(
    flags: egui_xterm_core::KittyKeyboardFlags,
    modes: &egui_xterm_core::Modes,
    key: Key,
    pressed: bool,
    repeat: bool,
    modifiers: Modifiers,
    associated_text: Option<&str>,
    caps_lock: bool,
    num_lock: bool,
    out_bytes: &mut Vec<u8>,
) -> bool {
    use egui_xterm_core::{
        KittyEventType, KittyKeyEvent, KittyKeyboardFlags, KittyModifiers, encode_kitty_key_into,
    };

    let event_type = if !pressed {
        KittyEventType::Release
    } else if repeat {
        KittyEventType::Repeat
    } else {
        KittyEventType::Press
    };

    let k_mods = KittyModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        ctrl: modifiers.ctrl,
        super_key: modifiers.mac_cmd || modifiers.command && !modifiers.ctrl,
        hyper: false,
        meta: false,
        caps_lock,
        num_lock,
    };

    // Preserve DECCKM application cursor keys when no modifiers/event-types/report-all apply.
    if modes.app_cursor_keys
        && event_type == KittyEventType::Press
        && !k_mods.has_active_modifiers()
        && !flags.contains(KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES)
    {
        let seq: Option<&[u8]> = match key {
            Key::ArrowUp => Some(b"\x1bOA"),
            Key::ArrowDown => Some(b"\x1bOB"),
            Key::ArrowRight => Some(b"\x1bOC"),
            Key::ArrowLeft => Some(b"\x1bOD"),
            Key::Home => Some(b"\x1bOH"),
            Key::End => Some(b"\x1bOF"),
            _ => None,
        };
        if let Some(bytes) = seq {
            out_bytes.extend_from_slice(bytes);
            return true;
        }
    }

    let Some((kitty_key, shifted_key, base_layout_key)) = egui_key_to_kitty(key) else {
        return false;
    };
    let ev = KittyKeyEvent {
        key: kitty_key,
        event_type,
        modifiers: k_mods,
        shifted_key,
        base_layout_key,
        associated_text,
    };
    encode_kitty_key_into(flags, &ev, out_bytes)
}

#[cfg(feature = "kitty-keyboard")]
fn encode_egui_modify_other_keys_into(
    terminal: &Terminal,
    key: Key,
    modifiers: Modifiers,
    out_bytes: &mut Vec<u8>,
) -> bool {
    use egui_xterm_core::{KittyEventType, KittyKeyEvent, KittyModifiers};

    if modifiers.mac_cmd || is_egui_modifier_key(key) {
        return false;
    }
    let Some((kitty_key, shifted_key, base_layout_key)) = egui_key_to_kitty(key) else {
        return false;
    };
    let ev = KittyKeyEvent {
        key: kitty_key,
        event_type: KittyEventType::Press,
        modifiers: KittyModifiers {
            shift: modifiers.shift,
            alt: modifiers.alt,
            ctrl: modifiers.ctrl,
            ..KittyModifiers::default()
        },
        shifted_key,
        base_layout_key,
        associated_text: None,
    };
    terminal.encode_modify_other_keys_into(&ev, out_bytes)
}

#[cfg(not(feature = "kitty-keyboard"))]
fn encode_egui_modify_other_keys_into(
    terminal: &Terminal,
    key: Key,
    modifiers: Modifiers,
    out_bytes: &mut Vec<u8>,
) -> bool {
    let level = terminal.modify_other_keys_level();
    if level == 0 || modifiers.mac_cmd || is_egui_modifier_key(key) {
        return false;
    }
    let mod_mask = u8::from(modifiers.shift)
        | (u8::from(modifiers.alt) << 1)
        | (u8::from(modifiers.ctrl) << 2);
    if mod_mask == 0 {
        return false;
    }
    let codepoint: u32 = match key {
        Key::Enter => {
            if level == 1 && !modifiers.shift && !modifiers.ctrl {
                return false;
            }
            13
        }
        Key::Tab => {
            if level == 1 && !modifiers.ctrl {
                return false;
            }
            9
        }
        Key::Backspace => {
            if level == 1 && !modifiers.shift {
                return false;
            }
            127
        }
        Key::Escape => {
            if level == 1 && !modifiers.shift && !modifiers.ctrl {
                return false;
            }
            27
        }
        Key::Space => {
            if level == 1 && !(modifiers.shift && modifiers.ctrl) {
                return false;
            }
            32
        }
        _ => {
            let Some((base, shifted)) = key_to_ascii_pair(key).or_else(|| {
                if key == Key::IntlBackslash {
                    Some(('\\', '|'))
                } else {
                    None
                }
            }) else {
                return false;
            };
            if !modifiers.ctrl && !modifiers.alt {
                return false;
            }
            if level == 1 {
                let has_legacy_ctrl = matches!(
                    base.to_ascii_lowercase(),
                    '@' | 'a'..='z' | '[' | '\\' | ']' | '^' | '_'
                );
                if !modifiers.ctrl
                    || (!(modifiers.shift && base.is_ascii_alphabetic()) && has_legacy_ctrl)
                {
                    return false;
                }
            }
            let eff = if modifiers.shift { shifted } else { base };
            u32::from(eff)
        }
    };
    out_bytes.extend_from_slice(b"\x1b[27;");
    push_decimal_u32(out_bytes, 1 + u32::from(mod_mask));
    out_bytes.push(b';');
    push_decimal_u32(out_bytes, codepoint);
    out_bytes.push(b'~');
    true
}

#[cfg(feature = "kitty-keyboard")]
fn egui_key_to_kitty(key: Key) -> Option<(egui_xterm_core::KittyKey, Option<char>, Option<char>)> {
    use egui_xterm_core::KittyKey;

    // Note: `egui::Key` collapses numpad digits/operators into `Num0..=Num9`, `Enter`, `Plus`,
    // `Minus`, `Period`, `Slash`, `Equals`, and `Comma`, which are handled by `key_to_ascii_pair`
    // and the command keys below. Only `Key::Clear` maps directly to `KittyKey::KpBegin` (`\x1b[E`).
    if let Some((base, shifted)) = key_to_ascii_pair(key) {
        return Some((KittyKey::Char(base), Some(shifted), Some(base)));
    }
    let kk = match key {
        Key::Enter => KittyKey::Enter,
        Key::Tab => KittyKey::Tab,
        Key::Backspace => KittyKey::Backspace,
        Key::Escape => KittyKey::Escape,
        Key::Space => KittyKey::Space,
        Key::ArrowUp => KittyKey::Up,
        Key::ArrowDown => KittyKey::Down,
        Key::ArrowRight => KittyKey::Right,
        Key::ArrowLeft => KittyKey::Left,
        Key::Home => KittyKey::Home,
        Key::End => KittyKey::End,
        Key::Insert => KittyKey::Insert,
        Key::Delete => KittyKey::Delete,
        Key::PageUp => KittyKey::PageUp,
        Key::PageDown => KittyKey::PageDown,
        Key::CapsLock => KittyKey::CapsLock,
        Key::ScrollLock => KittyKey::ScrollLock,
        Key::NumLock => KittyKey::NumLock,
        Key::PrintScreen => KittyKey::PrintScreen,
        Key::Pause => KittyKey::Pause,
        Key::Menu => KittyKey::Menu,
        Key::Clear => KittyKey::KpBegin,
        Key::IntlBackslash => return Some((KittyKey::Char('\\'), Some('|'), Some('\\'))),
        Key::ShiftLeft => KittyKey::LeftShift,
        Key::ShiftRight => KittyKey::RightShift,
        Key::ControlLeft => KittyKey::LeftControl,
        Key::ControlRight => KittyKey::RightControl,
        Key::AltLeft => KittyKey::LeftAlt,
        Key::AltRight => KittyKey::RightAlt,
        Key::SuperLeft => KittyKey::LeftSuper,
        Key::SuperRight => KittyKey::RightSuper,
        Key::F1 => KittyKey::F(1),
        Key::F2 => KittyKey::F(2),
        Key::F3 => KittyKey::F(3),
        Key::F4 => KittyKey::F(4),
        Key::F5 => KittyKey::F(5),
        Key::F6 => KittyKey::F(6),
        Key::F7 => KittyKey::F(7),
        Key::F8 => KittyKey::F(8),
        Key::F9 => KittyKey::F(9),
        Key::F10 => KittyKey::F(10),
        Key::F11 => KittyKey::F(11),
        Key::F12 => KittyKey::F(12),
        Key::F13 => KittyKey::F(13),
        Key::F14 => KittyKey::F(14),
        Key::F15 => KittyKey::F(15),
        Key::F16 => KittyKey::F(16),
        Key::F17 => KittyKey::F(17),
        Key::F18 => KittyKey::F(18),
        Key::F19 => KittyKey::F(19),
        Key::F20 => KittyKey::F(20),
        Key::F21 => KittyKey::F(21),
        Key::F22 => KittyKey::F(22),
        Key::F23 => KittyKey::F(23),
        Key::F24 => KittyKey::F(24),
        Key::F25 => KittyKey::F(25),
        Key::F26 => KittyKey::F(26),
        Key::F27 => KittyKey::F(27),
        Key::F28 => KittyKey::F(28),
        Key::F29 => KittyKey::F(29),
        Key::F30 => KittyKey::F(30),
        Key::F31 => KittyKey::F(31),
        Key::F32 => KittyKey::F(32),
        Key::F33 => KittyKey::F(33),
        Key::F34 => KittyKey::F(34),
        Key::F35 => KittyKey::F(35),
        _ => return None,
    };
    Some((kk, None, None))
}

fn encode_egui_key_legacy_into(
    modes: &egui_xterm_core::Modes,
    key: Key,
    modifiers: Modifiers,
    out_bytes: &mut Vec<u8>,
) -> bool {
    let mod_param = legacy_modifier_param(modifiers);

    match key {
        Key::Enter => {
            if modifiers.alt {
                out_bytes.push(0x1B);
            }
            if modes.linefeed_newline {
                out_bytes.extend_from_slice(b"\r\n");
            } else {
                out_bytes.push(b'\r');
            }
            return true;
        }
        Key::Tab => {
            if modifiers.shift {
                out_bytes.extend_from_slice(b"\x1b[Z");
            } else {
                out_bytes.push(b'\t');
            }
            return true;
        }
        Key::Backspace => {
            if modifiers.alt {
                out_bytes.extend_from_slice(b"\x1b\x7f");
            } else if modifiers.ctrl {
                out_bytes.push(0x08);
            } else {
                out_bytes.push(0x7F);
            }
            return true;
        }
        Key::Escape => {
            out_bytes.push(0x1B);
            return true;
        }
        Key::ArrowUp => {
            encode_csi_cursor_into(b'A', mod_param, modes.app_cursor_keys, out_bytes);
            return true;
        }
        Key::ArrowDown => {
            encode_csi_cursor_into(b'B', mod_param, modes.app_cursor_keys, out_bytes);
            return true;
        }
        Key::ArrowRight => {
            encode_csi_cursor_into(b'C', mod_param, modes.app_cursor_keys, out_bytes);
            return true;
        }
        Key::ArrowLeft => {
            encode_csi_cursor_into(b'D', mod_param, modes.app_cursor_keys, out_bytes);
            return true;
        }
        Key::Clear => {
            encode_csi_cursor_into(b'E', mod_param, false, out_bytes);
            return true;
        }
        Key::Home => {
            encode_csi_cursor_into(b'H', mod_param, modes.app_cursor_keys, out_bytes);
            return true;
        }
        Key::End => {
            encode_csi_cursor_into(b'F', mod_param, modes.app_cursor_keys, out_bytes);
            return true;
        }
        Key::Insert => {
            encode_tilde_key_into(2, mod_param, out_bytes);
            return true;
        }
        Key::Delete => {
            encode_tilde_key_into(3, mod_param, out_bytes);
            return true;
        }
        Key::PageUp => {
            encode_tilde_key_into(5, mod_param, out_bytes);
            return true;
        }
        Key::PageDown => {
            encode_tilde_key_into(6, mod_param, out_bytes);
            return true;
        }
        Key::F1 => {
            encode_ss3_or_csi_into(b'P', mod_param, out_bytes);
            return true;
        }
        Key::F2 => {
            encode_ss3_or_csi_into(b'Q', mod_param, out_bytes);
            return true;
        }
        Key::F3 => {
            encode_ss3_or_csi_into(b'R', mod_param, out_bytes);
            return true;
        }
        Key::F4 => {
            encode_ss3_or_csi_into(b'S', mod_param, out_bytes);
            return true;
        }
        Key::F5 => {
            encode_tilde_key_into(15, mod_param, out_bytes);
            return true;
        }
        Key::F6 => {
            encode_tilde_key_into(17, mod_param, out_bytes);
            return true;
        }
        Key::F7 => {
            encode_tilde_key_into(18, mod_param, out_bytes);
            return true;
        }
        Key::F8 => {
            encode_tilde_key_into(19, mod_param, out_bytes);
            return true;
        }
        Key::F9 => {
            encode_tilde_key_into(20, mod_param, out_bytes);
            return true;
        }
        Key::F10 => {
            encode_tilde_key_into(21, mod_param, out_bytes);
            return true;
        }
        Key::F11 => {
            encode_tilde_key_into(23, mod_param, out_bytes);
            return true;
        }
        Key::F12 => {
            encode_tilde_key_into(24, mod_param, out_bytes);
            return true;
        }
        _ => {}
    }

    // Ctrl + key -> C0 control codes.
    if modifiers.ctrl && !modifiers.mac_cmd {
        let ctrl_byte = match key {
            Key::Space | Key::Num2 => Some(0x00),
            Key::A => Some(0x01),
            Key::B => Some(0x02),
            Key::C => Some(0x03),
            Key::D => Some(0x04),
            Key::E => Some(0x05),
            Key::F => Some(0x06),
            Key::G => Some(0x07),
            Key::H => Some(0x08),
            Key::I => Some(0x09),
            Key::J => Some(0x0A),
            Key::K => Some(0x0B),
            Key::L => Some(0x0C),
            Key::M => Some(0x0D),
            Key::N => Some(0x0E),
            Key::O => Some(0x0F),
            Key::P => Some(0x10),
            Key::Q => Some(0x11),
            Key::R => Some(0x12),
            Key::S => Some(0x13),
            Key::T => Some(0x14),
            Key::U => Some(0x15),
            Key::V => Some(0x16),
            Key::W => Some(0x17),
            Key::X => Some(0x18),
            Key::Y => Some(0x19),
            Key::Z => Some(0x1A),
            Key::OpenBracket | Key::Num3 => Some(0x1B),
            Key::Backslash | Key::Num4 => Some(0x1C),
            Key::CloseBracket | Key::Num5 => Some(0x1D),
            Key::Num6 => Some(0x1E),
            Key::Minus | Key::Num7 | Key::Slash => Some(0x1F),
            _ => None,
        };
        if let Some(b) = ctrl_byte {
            if modifiers.alt {
                out_bytes.push(0x1B);
            }
            out_bytes.push(b);
            return true;
        }
    }

    // Alt + printable key -> ESC prefix.
    if modifiers.alt && !modifiers.ctrl && !modifiers.mac_cmd {
        if let Some((base, shifted)) = key_to_ascii_pair(key) {
            let ch = if modifiers.shift { shifted } else { base };
            out_bytes.push(0x1B);
            out_bytes.push(ch as u8);
            return true;
        }
        if key == Key::Space {
            out_bytes.extend_from_slice(b"\x1b ");
            return true;
        }
    }

    false
}

const fn legacy_modifier_param(mods: Modifiers) -> u8 {
    let mut mask = 0u8;
    if mods.shift {
        mask |= 1;
    }
    if mods.alt {
        mask |= 2;
    }
    if mods.ctrl {
        mask |= 4;
    }
    if mask == 0 { 1 } else { 1 + mask }
}

#[inline]
fn push_decimal_u32(out: &mut Vec<u8>, mut val: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        if val == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

fn encode_csi_cursor_into(final_ch: u8, mod_param: u8, app_cursor: bool, out: &mut Vec<u8>) {
    if mod_param > 1 {
        out.extend_from_slice(b"\x1b[1;");
        push_decimal_u32(out, u32::from(mod_param));
        out.push(final_ch);
    } else if app_cursor {
        out.extend_from_slice(&[0x1B, b'O', final_ch]);
    } else {
        out.extend_from_slice(&[0x1B, b'[', final_ch]);
    }
}

fn encode_tilde_key_into(num: u16, mod_param: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[");
    push_decimal_u32(out, u32::from(num));
    if mod_param > 1 {
        out.push(b';');
        push_decimal_u32(out, u32::from(mod_param));
    }
    out.push(b'~');
}

fn encode_ss3_or_csi_into(final_ch: u8, mod_param: u8, out: &mut Vec<u8>) {
    if mod_param > 1 {
        out.extend_from_slice(b"\x1b[1;");
        push_decimal_u32(out, u32::from(mod_param));
        out.push(final_ch);
    } else {
        out.extend_from_slice(&[0x1B, b'O', final_ch]);
    }
}

const fn key_to_ascii_pair(key: Key) -> Option<(char, char)> {
    Some(match key {
        Key::A => ('a', 'A'),
        Key::B => ('b', 'B'),
        Key::C => ('c', 'C'),
        Key::D => ('d', 'D'),
        Key::E => ('e', 'E'),
        Key::F => ('f', 'F'),
        Key::G => ('g', 'G'),
        Key::H => ('h', 'H'),
        Key::I => ('i', 'I'),
        Key::J => ('j', 'J'),
        Key::K => ('k', 'K'),
        Key::L => ('l', 'L'),
        Key::M => ('m', 'M'),
        Key::N => ('n', 'N'),
        Key::O => ('o', 'O'),
        Key::P => ('p', 'P'),
        Key::Q => ('q', 'Q'),
        Key::R => ('r', 'R'),
        Key::S => ('s', 'S'),
        Key::T => ('t', 'T'),
        Key::U => ('u', 'U'),
        Key::V => ('v', 'V'),
        Key::W => ('w', 'W'),
        Key::X => ('x', 'X'),
        Key::Y => ('y', 'Y'),
        Key::Z => ('z', 'Z'),
        Key::Num0 => ('0', ')'),
        Key::Num1 => ('1', '!'),
        Key::Num2 => ('2', '@'),
        Key::Num3 => ('3', '#'),
        Key::Num4 => ('4', '$'),
        Key::Num5 => ('5', '%'),
        Key::Num6 => ('6', '^'),
        Key::Num7 => ('7', '&'),
        Key::Num8 => ('8', '*'),
        Key::Num9 => ('9', '('),
        Key::Minus => ('-', '_'),
        Key::Equals => ('=', '+'),
        Key::OpenBracket => ('[', '{'),
        Key::CloseBracket => (']', '}'),
        Key::Backslash => ('\\', '|'),
        Key::Semicolon => (';', ':'),
        Key::Quote => ('\'', '"'),
        Key::Comma => (',', '<'),
        Key::Period => ('.', '>'),
        Key::Slash => ('/', '?'),
        Key::Backtick => ('`', '~'),
        Key::Plus => ('+', '+'),
        Key::Colon => (':', ':'),
        Key::Pipe => ('|', '|'),
        Key::Questionmark => ('?', '?'),
        Key::Exclamationmark => ('!', '!'),
        Key::OpenCurlyBracket => ('{', '{'),
        Key::CloseCurlyBracket => ('}', '}'),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use egui::{CentralPanel, Context, Frame, RawInput, pos2, vec2};
    use egui_xterm_core::CoreConfig;

    use super::*;

    fn run_mapper_frame(
        ctx: &Context,
        mapper: &mut InputMapperState,
        terminal: &mut Terminal,
        config: &TerminalConfig,
        events: Vec<Event>,
    ) -> Vec<u8> {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));
        let raw = RawInput {
            screen_rect: Some(screen),
            time: Some(0.1),
            focused: true,
            events,
            ..Default::default()
        };
        let mut written = Vec::new();
        let out = ctx.run_ui(raw, |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let resp = ui.allocate_response(vec2(640.0, 400.0), egui::Sense::click_and_drag());
                let id = resp.id;
                ui.memory_mut(|m| m.request_focus(id));
                let outcome = mapper.process_frame_events(
                    ui,
                    id,
                    &resp,
                    screen,
                    vec2(8.0, 16.0),
                    80,
                    25,
                    0.1,
                    config,
                    terminal,
                    false,
                    false,
                    None,
                    None,
                    false,
                    false,
                    false,
                );
                written.extend_from_slice(&outcome.bytes_to_write);
            });
        });
        out.drop_without_applying_deltas();
        written
    }

    #[test]
    fn key_clear_maps_to_legacy_kp_begin_and_kitty() {
        let term = Terminal::new(80, 24, CoreConfig::default());
        let mut out = Vec::new();
        let modes = *term.active_snapshot().modes;

        assert!(encode_egui_key_legacy_into(
            &modes,
            Key::Clear,
            Modifiers::NONE,
            &mut out
        ));
        assert_eq!(out, b"\x1b[E");

        out.clear();
        assert!(encode_egui_key_legacy_into(
            &modes,
            Key::Clear,
            Modifiers::SHIFT,
            &mut out
        ));
        assert_eq!(out, b"\x1b[1;2E");

        #[cfg(feature = "kitty-keyboard")]
        {
            let mut term = term;
            term.advance(b"\x1b[>1u");
            out.clear();
            let flags = term.kitty_keyboard_flags();
            assert!(encode_egui_key_kitty_into(
                flags,
                &modes,
                Key::Clear,
                true,
                false,
                Modifiers::NONE,
                None,
                false,
                false,
                &mut out,
            ));
            assert_eq!(out, b"\x1b[57427u");
        }
    }

    #[test]
    fn modify_other_keys_mode_1_mode_2_and_kitty_precedence() {
        let ctx = Context::default();
        let cfg = TerminalConfig::default();
        let mut mapper = InputMapperState::default();
        let mut term = Terminal::new(80, 24, CoreConfig::default());

        // Warm frame to acquire focus.
        let _ = run_mapper_frame(&ctx, &mut mapper, &mut term, &cfg, Vec::new());

        // Mode 1: Shift+Enter -> CSI 27;2;13~, Ctrl+1 -> CSI 27;5;49~, Ctrl+a -> 0x01.
        term.advance(b"\x1b[>4;1m");
        let b1 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::SHIFT,
            }],
        );
        assert_eq!(b1, b"\x1b[27;2;13~");

        let b2 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::Num1,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            }],
        );
        assert_eq!(b2, b"\x1b[27;5;49~");

        let b3 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            }],
        );
        assert_eq!(b3, b"\x01");

        // Mode 2: Ctrl+a -> CSI 27;5;97~, Shift+Space -> CSI 27;2;32~ (suppresses Text), Shift+A -> "A".
        term.advance(b"\x1b[>4;2m");
        let b4 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            }],
        );
        assert_eq!(b4, b"\x1b[27;5;97~");

        let b5 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![
                Event::Key {
                    key: Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::SHIFT,
                },
                Event::Text(" ".to_owned()),
            ],
        );
        assert_eq!(b5, b"\x1b[27;2;32~");

        let b6 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![
                Event::Key {
                    key: Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::SHIFT,
                },
                Event::Text("A".to_owned()),
            ],
        );
        assert_eq!(b6, b"A");

        // Ctrl+C without selection under Mode 2 -> CSI 27;5;99~.
        let b7 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::C,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            }],
        );
        assert_eq!(b7, b"\x1b[27;5;99~");

        // Kitty keyboard takes strict precedence over modifyOtherKeys=2.
        #[cfg(feature = "kitty-keyboard")]
        {
            term.advance(b"\x1b[>1u");
            let b8 = run_mapper_frame(
                &ctx,
                &mut mapper,
                &mut term,
                &cfg,
                vec![Event::Key {
                    key: Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::CTRL,
                }],
            );
            assert_eq!(b8, b"\x1b[97;5u");
        }
    }

    #[test]
    fn lock_modifiers_toggle_case_sync_and_explicit_setter() {
        let ctx = Context::default();
        let cfg = TerminalConfig::default();
        let mut mapper = InputMapperState::default();
        let mut term = Terminal::new(80, 24, CoreConfig::default());
        #[cfg(feature = "kitty-keyboard")]
        term.advance(b"\x1b[>9u"); // DISAMBIGUATE | REPORT_ALL_KEYS_AS_ESCAPE_CODES

        let _ = run_mapper_frame(&ctx, &mut mapper, &mut term, &cfg, Vec::new());

        // Toggle NumLock and CapsLock via key presses.
        let _ = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![
                Event::Key {
                    key: Key::CapsLock,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                Event::Key {
                    key: Key::NumLock,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
            ],
        );
        assert!(mapper.caps_lock());
        assert!(mapper.num_lock());

        // Enter with CapsLock (64) + NumLock (128) -> 1 + 192 = 193 under Kitty.
        let bytes = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        #[cfg(feature = "kitty-keyboard")]
        assert_eq!(bytes, b"\x1b[13;193u");
        #[cfg(not(feature = "kitty-keyboard"))]
        assert_eq!(bytes, b"\r");

        // Explicit setter clears both; Key/Text case-sync detects CapsLock from unshifted uppercase.
        mapper.set_lock_modifiers(false, false);
        assert!(!mapper.caps_lock());
        assert!(!mapper.num_lock());

        let synced = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![
                Event::Key {
                    key: Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                Event::Text("A".to_owned()),
            ],
        );
        assert!(mapper.caps_lock());
        #[cfg(feature = "kitty-keyboard")]
        assert_eq!(synced, b"\x1b[97;65u");
        #[cfg(not(feature = "kitty-keyboard"))]
        assert_eq!(synced, b"A");

        let synced_off = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![
                Event::Key {
                    key: Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                Event::Text("a".to_owned()),
            ],
        );
        assert!(!mapper.caps_lock());
        #[cfg(feature = "kitty-keyboard")]
        assert_eq!(synced_off, b"\x1b[97u");
        #[cfg(not(feature = "kitty-keyboard"))]
        assert_eq!(synced_off, b"a");
    }

    #[test]
    fn outgoing_scratch_reuses_capacity_across_frames() {
        let ctx = Context::default();
        let cfg = TerminalConfig::default();
        let mut mapper = InputMapperState::default();
        let mut term = Terminal::new(80, 24, CoreConfig::default());

        let _ = run_mapper_frame(&ctx, &mut mapper, &mut term, &cfg, Vec::new());
        let b1 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::ArrowUp,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert_eq!(b1, b"\x1b[A");
        let cap1 = mapper.outgoing_scratch.capacity();
        let ptr1 = mapper.outgoing_scratch.as_ptr();
        assert!(cap1 > 0);

        let b2 = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![Event::Key {
                key: Key::ArrowDown,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert_eq!(b2, b"\x1b[B");
        assert_eq!(mapper.outgoing_scratch.capacity(), cap1);
        assert_eq!(mapper.outgoing_scratch.as_ptr(), ptr1);
    }

    #[test]
    fn event_copy_and_sgr_pixels_and_punctuation_keys_zero_alloc() {
        let ctx = Context::default();
        ctx.set_os(egui::os::OperatingSystem::Windows);
        let cfg = TerminalConfig::default();
        let mut mapper = InputMapperState::default();
        let mut term = Terminal::new(80, 24, CoreConfig::default());
        term.advance(b"hello world\x1b[>4;2m\x1b[?1003;1016h");

        let _ = run_mapper_frame(&ctx, &mut mapper, &mut term, &cfg, Vec::new());

        // Event::Copy without selection in modifyOtherKeys=2 -> CSI 27;5;99~.
        let copy_no_sel = run_mapper_frame(&ctx, &mut mapper, &mut term, &cfg, vec![Event::Copy]);
        assert_eq!(copy_no_sel, b"\x1b[27;5;99~");

        // Punctuation keys under modifyOtherKeys=2 + Ctrl.
        for (key, code) in [
            (Key::Plus, 43),
            (Key::Equals, 61),
            (Key::Comma, 44),
            (Key::Period, 46),
            (Key::Minus, 45),
            (Key::Slash, 47),
        ] {
            let encoded = run_mapper_frame(
                &ctx,
                &mut mapper,
                &mut term,
                &cfg,
                vec![Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::CTRL,
                }],
            );
            assert_eq!(
                String::from_utf8_lossy(&encoded),
                format!("\x1b[27;5;{code}~"),
                "failed for key {key:?}"
            );
        }

        // SGR-Pixels (?1016) mouse press and sub-cell motion.
        let mouse_bytes = run_mapper_frame(
            &ctx,
            &mut mapper,
            &mut term,
            &cfg,
            vec![
                Event::PointerButton {
                    pos: pos2(12.0, 20.0),
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerMoved(pos2(14.0, 21.0)),
                Event::PointerButton {
                    pos: pos2(14.0, 21.0),
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
        );
        let mouse_str = String::from_utf8_lossy(&mouse_bytes);
        assert!(mouse_str.contains("\x1b[<0;12;20M"), "got {mouse_str:?}");
        assert!(mouse_str.contains("\x1b[<32;14;21M"), "got {mouse_str:?}");
        assert!(mouse_str.contains("\x1b[<0;14;21m"), "got {mouse_str:?}");
    }
}
