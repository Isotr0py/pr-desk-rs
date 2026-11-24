//! Floating search bar overlay (`search` feature).

use egui::{Align, Frame, Key, Layout, Rect, Ui, UiBuilder, pos2, vec2};
use egui_xterm_core::{SearchQuery, Terminal};

/// Persistent UI state for the optional floating search bar overlay.
#[derive(Debug, Clone, Default)]
pub struct SearchBarState {
    /// Whether the search bar overlay is currently visible.
    pub open: bool,
    /// Current query text in the search input box.
    pub query_text: String,
    /// Interpret [`Self::query_text`] as a regular expression when `true`.
    pub is_regex: bool,
    /// Match case-sensitively when `true`.
    pub case_sensitive: bool,
    /// Match whole words only when `true`.
    pub whole_word: bool,
    /// Last regex compilation error, if any.
    pub last_error: Option<String>,
    focus_requested: bool,
    opened_in_frame: bool,
    pub(crate) has_focus: bool,
    pub(crate) last_rect: Option<Rect>,
}

const MIN_EDIT_W: f32 = 16.0;
const SAFETY_PAD: f32 = 1.0;

impl SearchBarState {
    /// Opens the search bar overlay and requests focus on its text input next frame.
    pub fn open(&mut self) {
        self.open = true;
        self.focus_requested = true;
        self.opened_in_frame = false;
    }

    pub(crate) fn open_from_action(&mut self) {
        self.open = true;
        self.focus_requested = true;
        self.opened_in_frame = true;
    }

    /// Closes the search bar overlay.
    pub fn close(&mut self) {
        self.open = false;
        self.clear_hidden_focus();
    }

    pub(crate) fn clear_hidden_focus(&mut self) {
        self.focus_requested = false;
        self.has_focus = false;
        self.opened_in_frame = false;
        self.last_rect = None;
    }

    pub(crate) fn active_rect(&self, term_rect: Rect, clip_rect: Rect) -> Option<Rect> {
        if !self.open {
            return None;
        }
        let bar_rect = compute_bar_rect(term_rect, clip_rect, self.last_error.is_some())?;
        Some(match self.last_rect {
            Some(prev)
                if prev.intersects(bar_rect)
                    && prev.min.x >= bar_rect.min.x - 1.0
                    && prev.max.x <= bar_rect.max.x + 1.0
                    && prev.min.y >= bar_rect.min.y - 1.0
                    && prev.min.y <= bar_rect.min.y + 1.0 =>
            {
                prev.union(bar_rect).intersect(clip_rect)
            }
            _ => bar_rect,
        })
    }

    pub(crate) const fn blocks_terminal_focus(&self) -> bool {
        self.open && (self.has_focus || self.focus_requested)
    }
}

pub(crate) fn compute_bar_rect(term_rect: Rect, clip_rect: Rect, has_error: bool) -> Option<Rect> {
    if term_rect.width() < 120.0 || term_rect.height() < 36.0 {
        return None;
    }
    let bar_w = (term_rect.width() - 16.0)
        .clamp(104.0, 410.0)
        .min(term_rect.width());
    let bar_h = if has_error && term_rect.height() >= 56.0 {
        48.0
    } else {
        30.0
    };
    let bar_rect = Rect::from_min_size(
        pos2(
            (term_rect.max.x - bar_w - 8.0).max(term_rect.min.x),
            term_rect.min.y + 8.0,
        ),
        vec2(bar_w, bar_h),
    )
    .intersect(clip_rect);
    (bar_rect.width() >= 104.0 && bar_rect.height() >= 24.0).then_some(bar_rect)
}

fn apply_search_query(state: &mut SearchBarState, terminal: &mut Terminal) {
    if state.query_text.is_empty() {
        terminal.clear_search();
        state.last_error = None;
    } else {
        let query = SearchQuery::literal(&state.query_text)
            .with_regex(state.is_regex)
            .with_case_sensitive(state.case_sensitive)
            .with_whole_word(state.whole_word);
        match terminal.search(&query) {
            Ok(_) => state.last_error = None,
            Err(err) => state.last_error = Some(err.to_string()),
        }
    }
}

fn apply_search_step(terminal: &mut Terminal, step_dir: i32) {
    if step_dir > 0 {
        terminal.next_search_match();
    } else if step_dir < 0 {
        terminal.prev_search_match();
    }
}

fn measure_body_text_w(ui: &Ui, font: &egui::FontId, text: &str) -> f32 {
    ui.fonts_mut(|f| {
        f.layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE)
            .size()
            .x
    })
}

fn consume_bar_click(
    ui: &Ui,
    resp: &egui::Response,
    bar_rect: Rect,
    pointer_pressed_outside_bar: bool,
) -> bool {
    if pointer_pressed_outside_bar && ui.ctx().is_being_dragged(resp.id) {
        ui.ctx().stop_dragging();
    }
    if !resp.clicked() {
        return false;
    }
    match resp.interact_pointer_pos() {
        Some(pos) => {
            !pointer_pressed_outside_bar && bar_rect.contains(pos) && resp.rect.contains(pos)
        }
        None => !pointer_pressed_outside_bar,
    }
}

/// Renders the floating search bar at the top-right of `term_rect` when `state.open` is `true`.
///
/// Returns `true` if the terminal itself should reclaim focus (e.g. when closing with `Escape`).
pub(crate) fn show_search_bar(
    ui: &mut Ui,
    id_salt: u64,
    term_rect: Rect,
    clip_rect: Rect,
    state: &mut SearchBarState,
    terminal: &mut Terminal,
) -> bool {
    if !state.open {
        state.clear_hidden_focus();
        return false;
    }
    let Some(bar_rect) = compute_bar_rect(term_rect, clip_rect, state.last_error.is_some()) else {
        state.clear_hidden_focus();
        return false;
    };

    let opened_in_frame = std::mem::take(&mut state.opened_in_frame);
    let was_bar_focused = state.has_focus;
    let visible_term_rect = term_rect.intersect(clip_rect);
    let (clicked_in_bar, pointer_pressed_outside_bar, pointer_pressed_in_term) = ui.input(|i| {
        let mut in_bar = false;
        let mut outside = false;
        let mut in_term = false;
        for ev in &i.events {
            if let egui::Event::PointerButton {
                pos,
                button,
                pressed: true,
                ..
            } = ev
            {
                if bar_rect.contains(*pos) {
                    if *button == egui::PointerButton::Primary {
                        in_bar = true;
                    }
                } else {
                    outside = true;
                    if visible_term_rect.is_positive() && visible_term_rect.contains(*pos) {
                        in_term = true;
                    }
                }
            }
        }
        (in_bar && !outside, outside, in_term)
    });
    if !opened_in_frame && pointer_pressed_in_term {
        state.focus_requested = false;
    }
    let mut return_focus = false;
    let mut query_dirty = false;
    let mut step_dir: i32 = 0;
    let mut edit_focused = false;
    let mut bar_interacted = false;
    let mut needs_repaint = false;

    let bar_w = bar_rect.width();
    let compact = bar_w < 196.0;
    let candidate_toggles = bar_w >= 224.0;
    let candidate_count = bar_w >= 130.0;

    let scope_resp = ui.scope_builder(
        UiBuilder::new()
            .id_salt((id_salt, "search_bar"))
            .max_rect(bar_rect),
        |ui| {
            ui.set_clip_rect(clip_rect);
            let mut frame = Frame::popup(ui.style());
            frame = frame.inner_margin(if compact {
                egui::Margin::symmetric(4, 3)
            } else {
                egui::Margin::symmetric(6, 4)
            });
            frame.show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = if compact { 2.0 } else { 3.0 };
                ui.spacing_mut().button_padding = if compact {
                    vec2(3.0, 1.0)
                } else {
                    vec2(4.0, 1.0)
                };

                let mut focus_edit = false;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let close_resp = ui.small_button("x");
                    if consume_bar_click(ui, &close_resp, bar_rect, pointer_pressed_outside_bar) {
                        state.close();
                        terminal.clear_search();
                        return_focus = true;
                    }
                    let next_resp = ui.small_button(">");
                    if consume_bar_click(ui, &next_resp, bar_rect, pointer_pressed_outside_bar) {
                        step_dir = 1;
                        bar_interacted = true;
                        focus_edit = true;
                    }
                    let prev_resp = ui.small_button("<");
                    if consume_bar_click(ui, &prev_resp, bar_rect, pointer_pressed_outside_bar) {
                        step_dir = -1;
                        bar_interacted = true;
                        focus_edit = true;
                    }

                    let left_rect = ui.available_rect_before_wrap();
                    if left_rect.width() >= 24.0 && left_rect.height() > 0.0 {
                        ui.allocate_ui_with_layout(
                            left_rect.size(),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                let horiz_clip = Rect::from_min_max(
                                    pos2(left_rect.min.x.max(clip_rect.min.x), clip_rect.min.y),
                                    pos2(left_rect.max.x.min(clip_rect.max.x), clip_rect.max.y),
                                );
                                if horiz_clip.is_positive() {
                                    ui.set_clip_rect(horiz_clip);
                                }

                                let avail_w = ui.available_width().max(0.0);
                                let item_spacing = ui.spacing().item_spacing.x;
                                let button_pad_x = ui.spacing().button_padding.x;
                                let body_font = egui::TextStyle::Body.resolve(ui.style());

                                let toggles_w = if candidate_toggles {
                                    let aa_w = measure_body_text_w(ui, &body_font, "Aa")
                                        + 2.0 * button_pad_x;
                                    let re_w = measure_body_text_w(ui, &body_font, ".*")
                                        + 2.0 * button_pad_x;
                                    let ww_w = measure_body_text_w(ui, &body_font, "\\b")
                                        + 2.0 * button_pad_x;
                                    aa_w + re_w + ww_w + 3.0 * item_spacing
                                } else {
                                    0.0
                                };

                                let prev_total = terminal.search_matches().len();
                                let count_reserve = if candidate_count {
                                    let digits = prev_total.max(100).ilog10() + 1;
                                    let max_num = 10usize.saturating_pow(digits).saturating_sub(1);
                                    let sample = format!("{max_num}/{max_num}");
                                    measure_body_text_w(ui, &body_font, &sample)
                                        + item_spacing
                                        + SAFETY_PAD
                                } else {
                                    0.0
                                };

                                let show_toggles = candidate_toggles
                                    && (avail_w - count_reserve - toggles_w) >= MIN_EDIT_W;
                                let effective_toggles_w =
                                    if show_toggles { toggles_w } else { 0.0 };

                                let show_count = candidate_count
                                    && (avail_w - effective_toggles_w - count_reserve)
                                        >= MIN_EDIT_W;
                                let effective_count_w =
                                    if show_count { count_reserve } else { 0.0 };

                                let edit_w =
                                    (avail_w - effective_toggles_w - effective_count_w).max(0.0);

                                let edit_id = ui.make_persistent_id((id_salt, "search_query_edit"));
                                if pointer_pressed_outside_bar
                                    && !state.focus_requested
                                    && !opened_in_frame
                                {
                                    if ui.memory(|m| m.has_focus(edit_id)) {
                                        ui.memory_mut(|m| m.surrender_focus(edit_id));
                                    }
                                    if ui.ctx().is_being_dragged(edit_id) {
                                        ui.ctx().stop_dragging();
                                    }
                                }
                                let prev_focused = ui.memory(|m| m.focused());

                                let edit_resp = ui.add(
                                    egui::TextEdit::singleline(&mut state.query_text)
                                        .id(edit_id)
                                        .desired_width(edit_w)
                                        .hint_text("Search..."),
                                );
                                let focus_requested_for_edit = state.focus_requested
                                    && (opened_in_frame || !pointer_pressed_in_term);
                                let focus_from_bar_interaction =
                                    !pointer_pressed_outside_bar && (focus_edit || clicked_in_bar);
                                let focused_edit_this_frame = !return_focus
                                    && (focus_requested_for_edit || focus_from_bar_interaction);
                                if focused_edit_this_frame {
                                    edit_resp.request_focus();
                                    state.focus_requested = false;
                                }
                                let press_origin_outside_bar = ui.input(|i| {
                                    i.pointer
                                        .press_origin()
                                        .is_some_and(|p| !bar_rect.contains(p))
                                });
                                let stale_pointer_hit = press_origin_outside_bar
                                    || edit_resp
                                        .interact_pointer_pos()
                                        .is_some_and(|p| !bar_rect.contains(p));
                                if !opened_in_frame
                                    && !focused_edit_this_frame
                                    && !state.focus_requested
                                    && (pointer_pressed_outside_bar
                                        || (!was_bar_focused && stale_pointer_hit))
                                {
                                    if ui.memory(|m| m.has_focus(edit_id)) {
                                        ui.memory_mut(|m| m.surrender_focus(edit_id));
                                        if let Some(prev_id) = prev_focused
                                            && prev_id != edit_id
                                        {
                                            ui.memory_mut(|m| m.request_focus(prev_id));
                                        }
                                    }
                                    if ui.ctx().is_being_dragged(edit_id) {
                                        ui.ctx().stop_dragging();
                                    }
                                }
                                if edit_resp.changed() {
                                    query_dirty = true;
                                }
                                let edit_has_focus_before_toggles =
                                    ui.memory(|m| m.has_focus(edit_id));

                                let mut aa_id = None;
                                let mut re_id = None;
                                let mut ww_id = None;
                                let mut aa_has_focus = false;
                                let mut re_has_focus = false;
                                let mut ww_has_focus = false;
                                if show_toggles {
                                    let aa_resp = ui
                                        .selectable_label(state.case_sensitive, "Aa")
                                        .on_hover_text("Match case");
                                    aa_id = Some(aa_resp.id);
                                    if consume_bar_click(
                                        ui,
                                        &aa_resp,
                                        bar_rect,
                                        pointer_pressed_outside_bar,
                                    ) {
                                        state.case_sensitive = !state.case_sensitive;
                                        query_dirty = true;
                                        bar_interacted = true;
                                        edit_resp.request_focus();
                                    }
                                    aa_has_focus = ui.memory(|m| m.has_focus(aa_resp.id));

                                    let ww_resp = ui
                                        .selectable_label(state.whole_word, "\\b")
                                        .on_hover_text("Match whole word");
                                    ww_id = Some(ww_resp.id);
                                    if consume_bar_click(
                                        ui,
                                        &ww_resp,
                                        bar_rect,
                                        pointer_pressed_outside_bar,
                                    ) {
                                        state.whole_word = !state.whole_word;
                                        query_dirty = true;
                                        bar_interacted = true;
                                        edit_resp.request_focus();
                                    }
                                    ww_has_focus = ui.memory(|m| m.has_focus(ww_resp.id));

                                    let re_resp = ui
                                        .selectable_label(state.is_regex, ".*")
                                        .on_hover_text("Use regular expression");
                                    re_id = Some(re_resp.id);
                                    if consume_bar_click(
                                        ui,
                                        &re_resp,
                                        bar_rect,
                                        pointer_pressed_outside_bar,
                                    ) {
                                        state.is_regex = !state.is_regex;
                                        query_dirty = true;
                                        bar_interacted = true;
                                        edit_resp.request_focus();
                                    }
                                    re_has_focus = ui.memory(|m| m.has_focus(re_resp.id));
                                }

                                let esc_pressed = ui.input(|i| i.key_pressed(Key::Escape));
                                let enter_pressed = ui.input(|i| i.key_pressed(Key::Enter));
                                let no_other_focus = ui.memory(|m| m.focused().is_none());
                                let had_edit_focus = ui.memory(|m| m.had_focus_last_frame(edit_id));
                                let had_bar_btn_focus = ui.memory(|m| {
                                    m.had_focus_last_frame(close_resp.id)
                                        || m.had_focus_last_frame(next_resp.id)
                                        || m.had_focus_last_frame(prev_resp.id)
                                        || aa_id.is_some_and(|id| m.had_focus_last_frame(id))
                                        || ww_id.is_some_and(|id| m.had_focus_last_frame(id))
                                        || re_id.is_some_and(|id| m.had_focus_last_frame(id))
                                });
                                let bar_btn_has_focus = ui.memory(|m| {
                                    m.has_focus(close_resp.id)
                                        || m.has_focus(next_resp.id)
                                        || m.has_focus(prev_resp.id)
                                }) || aa_has_focus
                                    || ww_has_focus
                                    || re_has_focus;
                                let bar_owns_escape = !pointer_pressed_outside_bar
                                    && (edit_has_focus_before_toggles
                                        || bar_btn_has_focus
                                        || (was_bar_focused
                                            && (had_edit_focus || had_bar_btn_focus)
                                            && no_other_focus));
                                let bar_owns_enter = !pointer_pressed_outside_bar
                                    && !bar_interacted
                                    && step_dir == 0
                                    && (edit_has_focus_before_toggles
                                        || (was_bar_focused && had_edit_focus && no_other_focus));

                                if state.open && bar_owns_escape && esc_pressed {
                                    state.close();
                                    terminal.clear_search();
                                    return_focus = true;
                                } else if state.open && bar_owns_enter && enter_pressed {
                                    let shift = ui.input(|i| i.modifiers.shift);
                                    step_dir = if shift { -1 } else { 1 };
                                    edit_resp.request_focus();
                                }

                                if state.open {
                                    if query_dirty {
                                        apply_search_query(state, terminal);
                                        query_dirty = false;
                                        needs_repaint = true;
                                    }
                                    if step_dir != 0 {
                                        apply_search_step(terminal, step_dir);
                                        step_dir = 0;
                                        needs_repaint = true;
                                    }
                                }

                                if show_count {
                                    let total = terminal.search_matches().len();
                                    let active_1based = terminal
                                        .active_search_match_index()
                                        .map_or(0, |idx| idx + 1);
                                    let label = format!("{active_1based}/{total}");
                                    let label_w = measure_body_text_w(ui, &body_font, &label);
                                    if label_w <= ui.available_width() + 0.01 {
                                        ui.add(
                                            egui::Label::new(label)
                                                .selectable(false)
                                                .sense(egui::Sense::empty()),
                                        );
                                    } else {
                                        needs_repaint = true;
                                    }
                                }

                                edit_focused =
                                    ui.memory(|m| m.has_focus(edit_id)) || bar_btn_has_focus;
                                if consume_bar_click(
                                    ui,
                                    &edit_resp,
                                    bar_rect,
                                    pointer_pressed_outside_bar,
                                ) {
                                    bar_interacted = true;
                                }
                            },
                        );
                    }
                });
                if state.open
                    && let Some(err) = state.last_error.as_deref()
                {
                    let err_color = ui.visuals().error_fg_color;
                    let err_resp = ui.add(
                        egui::Label::new(egui::RichText::new(err).small().color(err_color))
                            .truncate()
                            .selectable(false),
                    );
                    err_resp.on_hover_text(err);
                }
                if state.open && step_dir != 0 {
                    apply_search_step(terminal, step_dir);
                    needs_repaint = true;
                }
                if state.open && clicked_in_bar && !return_focus {
                    bar_interacted = true;
                }
            })
        },
    );

    if state.open {
        let actual_rect = scope_resp
            .response
            .rect
            .union(bar_rect)
            .intersect(clip_rect);
        state.last_rect = actual_rect.is_positive().then_some(actual_rect);
        state.has_focus = edit_focused || bar_interacted;
    }

    if needs_repaint {
        ui.ctx().request_repaint();
    }

    return_focus
}
