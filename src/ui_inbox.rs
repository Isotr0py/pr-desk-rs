//! Inbox：GitHub 通知列表 + 右侧预览面板（标题/kind/状态/会话 markdown/Files/Diff）。

use egui::{FontId, RichText, Ui};

use pr_desk_rs::i18n::{tr, trf};

use crate::state::*;
use crate::{ui_detail, EguiApp};

/// Inbox 主视图（中央面板）。
pub fn inbox_view(app: &mut EguiApp, ui: &mut Ui) {
    let lang = app.lang();
    egui::CentralPanel::default().show(ui, |ui| {
        // ── 工具行：仓库过滤 / 全部已读 / 只看未读 ──
        let (repos, inbox_filter, inbox_all, has_more, loading_more) = {
            let a = app.app.lock_recover();
            let mut seen = std::collections::HashSet::new();
            let repos: Vec<String> = a
                .inbox
                .iter()
                .filter_map(|n| {
                    let s = n["repo"].as_str().unwrap_or("").to_string();
                    if !s.is_empty() && seen.insert(s.clone()) { Some(s) } else { None }
                })
                .collect();
            (repos, a.inbox_filter.clone(), a.inbox_all, a.inbox_has_more, a.inbox_loading_more)
        };
        ui.horizontal(|ui| {
            // 仓库过滤（保序去重列表；"" = 全部）
            let cur_label = if inbox_filter.is_empty() {
                tr(&lang, "inbox_filter_all").to_string()
            } else {
                inbox_filter.clone()
            };
            egui::ComboBox::from_id_salt("inbox-repo-filter")
                .selected_text(cur_label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(inbox_filter.is_empty(), tr(&lang, "inbox_filter_all"))
                        .clicked()
                    {
                        set_repo_filter(app, "");
                    }
                    for r in &repos {
                        if ui.selectable_label(inbox_filter == *r, r).clicked() {
                            set_repo_filter(app, r);
                        }
                    }
                });
            // 「全部已读」：两步确认（3s 超时还原）
            let mar_label = if app.mar_confirm.is_some() {
                tr(&lang, "inbox_mark_all_read_confirm")
            } else {
                tr(&lang, "inbox_mark_all_read")
            };
            if ui.button(mar_label).clicked() {
                let now = std::time::Instant::now();
                match app.mar_confirm {
                    Some(t) if t.elapsed().as_secs() < 3 => {
                        app.mar_confirm = None;
                        let _ = app.cmd_tx.send(Cmd::MarkAllNotifsRead);
                    }
                    _ => app.mar_confirm = Some(now),
                }
            }
            // 3s 超时还原（渲染期检查）
            if let Some(t) = app.mar_confirm {
                if t.elapsed().as_secs() >= 3 {
                    app.mar_confirm = None;
                } else {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                }
            }
            // 「只看未读」开关：切完立即重拉；分页重置回第一页
            let mut unread_only = !inbox_all;
            if ui.checkbox(&mut unread_only, tr(&lang, "inbox_unread_only")).changed() {
                {
                    let mut a = app.app.lock_recover();
                    a.inbox_all = !unread_only;
                    a.inbox_page = 1;
                    a.inbox_has_more = false;
                }
                app.inbox_loading = true;
                let _ = app.cmd_tx.send(Cmd::FetchInbox);
            }
            if app.inbox_loading {
                ui.add(egui::Spinner::new());
            }
        });
        ui.separator();

        // ── 空态 / 加载态 / 列表 ──
        let needs_token = app.app.lock_recover().cfg.token().is_none();
        if needs_token {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.strong(tr(&lang, "inbox_needs_token_title"));
                ui.weak(tr(&lang, "inbox_needs_token_hint"));
            });
            return;
        }
        if app.inbox_loading {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.add(egui::Spinner::new());
                ui.weak(tr(&lang, "inbox_loading"));
            });
            return;
        }
        let rows = app.app.lock_recover().inbox_view.clone();
        if rows.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.strong(tr(&lang, "inbox_empty_title"));
                ui.weak(tr(&lang, "inbox_empty_hint"));
            });
            return;
        }

        // 表头（与数据行同一列布局：未读点/仓库/标题/状态/类型/原因/时间）
        {
            let total_w = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, 20.0), egui::Sense::hover());
            let cols = inbox_col_layout(total_w);
            let weak = ui.visuals().weak_text_color();
            let hf = FontId::proportional(12.0);
            let labels = [
                "",
                tr(&lang, "inbox_col_repo"),
                tr(&lang, "inbox_col_title"),
                tr(&lang, "inbox_col_state"),
                tr(&lang, "inbox_col_type"),
                tr(&lang, "inbox_col_reason"),
                tr(&lang, "inbox_col_time"),
            ];
            for (i, lab) in labels.iter().enumerate() {
                if lab.is_empty() {
                    continue;
                }
                let (x, w) = cols[i];
                let cell = egui::Rect::from_min_size(rect.min + egui::vec2(x, 0.0), egui::vec2(w, rect.height()));
                crate::ui_list::paint_cell(ui, cell, lab, hf.clone(), weak);
            }
        }

        egui::ScrollArea::vertical()
            .id_salt("inbox-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, n) in rows.iter().enumerate() {
                    inbox_row(app, ui, n, i, &lang);
                }
                // 「加载更多」：进行中或无下一页时隐藏
                if has_more {
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| {
                        let label = if loading_more {
                            tr(&lang, "inbox_loading")
                        } else {
                            tr(&lang, "inbox_load_more")
                        };
                        if ui.add_enabled(!loading_more, egui::Button::new(label)).clicked() {
                            let _ = app.cmd_tx.send(Cmd::FetchInboxMore);
                        }
                    });
                }
            });
    });
}

fn set_repo_filter(app: &EguiApp, f: &str) {
    let mut a = app.app.lock_recover();
    a.inbox_filter = f.to_string();
    a.inbox_view = inbox_filtered(&a);
}

/// Inbox 行/表头共用列布局：[未读点][仓库][标题(弹性)][状态][类型][原因][时间]。
/// 返回相对行左缘的 (x, w)；标题列吃剩余宽（最小 120），其余列固定宽、互不侵占。
fn inbox_col_layout(total_w: f32) -> [(f32, f32); 7] {
    const DOT: f32 = 16.0;
    const REPO: f32 = 150.0;
    const STATE: f32 = 52.0;
    const KIND: f32 = 92.0;
    const REASON: f32 = 96.0;
    const TIME: f32 = 48.0;
    let fixed = DOT + REPO + STATE + KIND + REASON + TIME;
    let title_w = (total_w - fixed - 4.0).max(120.0);
    let mut out = [(0.0, 0.0); 7];
    let mut x = 0.0;
    for (i, w) in [DOT, REPO, title_w, STATE, KIND, REASON, TIME].into_iter().enumerate() {
        out[i] = (x, w);
        x += w;
    }
    out
}

fn inbox_row(app: &mut EguiApp, ui: &mut Ui, n: &serde_json::Value, i: usize, lang: &str) {
    let repo = n["repo"].as_str().unwrap_or("");
    let title = n["title"].as_str().unwrap_or("");
    let kind = n["kind"].as_str().unwrap_or("");
    let url = n["url"].as_str().unwrap_or("");
    let unread = n["unread"].as_bool().unwrap_or(false);
    // 选中判定：详情面板开着时按稳定身份（repo + 行自身 url）严格单行匹配——旧实现
    // 比较的是详情自身的 (num, url)（恒等），条件退化成 r == repo，同仓库的行全亮；
    // 面板关着时（未打开过详情）退到键盘导航下标
    let detail = app.app.lock_recover().inbox_detail.clone();
    let selected = if let Some((r, _, _, durl)) = &detail {
        r == repo && !url.is_empty() && *durl == url
    } else {
        app.inbox_sel == Some(i)
    };

    let height = 30.0;
    let total_w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(total_w, height), egui::Sense::click());
    if app.inbox_scroll_to == Some(i) {
        resp.scroll_to_me(Some(egui::Align::Center));
        app.inbox_scroll_to = None;
    }
    let visuals = ui.visuals();
    let bg = if selected {
        visuals.selection.bg_fill
    } else if resp.hovered() {
        visuals.widgets.hovered.bg_fill
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 3.0, bg);

    let p = ui.painter();
    let text_color = visuals.text_color();
    let weak = visuals.weak_text_color();
    let small = FontId::proportional(12.0);
    let cols = inbox_col_layout(total_w);
    let cell = |idx: usize| -> egui::Rect {
        let (x, w) = cols[idx];
        egui::Rect::from_min_size(rect.min + egui::vec2(x, 0.0), egui::vec2(w, rect.height()))
    };
    // 未读圆点：独立窄列（列 0），不与仓库文字挤
    if unread {
        p.circle_filled(cell(0).center(), 3.0, visuals.hyperlink_color);
    }
    crate::ui_list::paint_cell(ui, cell(1), repo, small.clone(), weak);
    crate::ui_list::paint_cell(ui, cell(2), title, small.clone(), if unread { text_color } else { weak });
    let state = n["state"].as_str().unwrap_or("");
    if !state.is_empty() {
        let (label, color) = match state {
            "merged" => (tr(lang, "inbox_state_merged"), ui_detail::state_color("merged", visuals.dark_mode)),
            "closed" => (tr(lang, "inbox_state_closed"), ui_detail::state_color("closed", visuals.dark_mode)),
            _ => (tr(lang, "inbox_state_open"), ui_detail::state_color("open", visuals.dark_mode)),
        };
        crate::ui_list::paint_cell(ui, cell(3), label, small.clone(), color);
    }
    // 类型列；「已加入」的 PR 在类型文字后跟一个绿色 ✓（裁在列内）
    let kind_label = notif_kind_label(lang, kind);
    let added = kind == "PullRequest"
        && notif_url_number(url).is_some_and(|num| {
            app.app.lock_recover().db.pr_exists(repo, num).unwrap_or(true)
        });
    crate::ui_list::paint_cell(ui, cell(4), &kind_label, small.clone(), weak);
    if added {
        let kw = ui.fonts_mut(|f| f.layout_no_wrap(kind_label.clone(), small.clone(), weak).rect.width());
        let kc = cell(4);
        p.with_clip_rect(kc).text(
            egui::pos2((kc.left() + 2.0 + kw + 4.0).min(kc.right() - 10.0), kc.center().y),
            egui::Align2::LEFT_CENTER,
            "✓",
            small.clone(),
            ui_detail::state_color("open", visuals.dark_mode),
        );
    }
    crate::ui_list::paint_cell(ui, cell(5), &notif_reason_label(lang, n["reason"].as_str().unwrap_or("")), small.clone(), weak);
    crate::ui_list::paint_cell(ui, cell(6), &rel_time(n["updated_at"].as_str().unwrap_or("")), small.clone(), weak);

    if resp.clicked() {
        app.inbox_activated(i);
    }
    // 行内快捷：「+ 预评审」（右键菜单承载，避免行内挤按钮）
    resp.context_menu(|ui| {
        if kind == "PullRequest" && ui.button(tr(&lang, "inbox_add_review")).clicked() {
            app.inbox_add_review(i);
            ui.close();
        }
        if ui.button(tr(&lang, "inbox_open_browser")).clicked() {
            let url = n["url"].as_str().unwrap_or("").to_string();
            if !url.is_empty() {
                if let Err(e) = open::that(&url) {
                    crate::worker::send_msg(Msg::Status(trf(lang, "open_browser_failed", &[&e.to_string()])));
                }
            }
            ui.close();
        }
        if ui.button(tr(&lang, "inbox_mark_all_read")).clicked() {
            if let Some(id) = n["id"].as_str().filter(|s| !s.is_empty()) {
                let _ = app.cmd_tx.send(Cmd::MarkNotifRead(id.to_string()));
            }
            ui.close();
        }
    });
}

/// 通知预览面板（右侧）。
pub fn notif_preview(app: &mut EguiApp, ui: &mut Ui) {
    if !app.nd_open {
        return;
    }
    let lang = app.lang();
    egui::Panel::right("notif-detail")
        .resizable(true)
        .default_size(420.0)
        .size_range(300.0..=600.0)
        .show(ui, |ui| {
            // ── 头部：常驻不滚动（旧版整个面板一个滚动区，重开时停在上次滚动位、
            // 标题被裁半——用户截图实证）──
            // ✕ 单独一行钉右上；标题独占一行按面板宽度换行（同 detail_panel）。
            // 不能放回 horizontal：横向布局里 Label 走 TextWrapMode::Extend 不换行，
            // 超长标题单行溢出会把内容 ui 的 max_rect 撑大（egui 的 Region::
            // expand_to_include_rect 连 max_rect 一起扩），其后的 ScrollArea/markdown
            // 全按虚胖宽度排版；Panel 还会把超宽内容重锚到内容右缘，导致面板左缘
            // 被中央列表盖住、右缘跑出窗口（用户截图实证）。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                if ui.small_button("✕").clicked() {
                    app.nd_open = false;
                    app.app.lock_recover().inbox_detail = None;
                }
            });
            let title = if app.nd_title.is_empty() {
                tr(&lang, "inbox_not_selected")
            } else {
                &app.nd_title
            };
            ui.add(egui::Label::new(RichText::new(title).size(15.0).strong()).wrap());
            ui.horizontal(|ui| {
                ui.weak(&app.nd_kind_label);
                if !app.nd_state.is_empty() {
                    let (label, color) = match app.nd_state.as_str() {
                        "merged" => (tr(&lang, "inbox_state_merged"), ui_detail::state_color("merged", ui.visuals().dark_mode)),
                        "closed" => (tr(&lang, "inbox_state_closed"), ui_detail::state_color("closed", ui.visuals().dark_mode)),
                        _ => (tr(&lang, "inbox_state_open"), ui_detail::state_color("open", ui.visuals().dark_mode)),
                    };
                    ui.colored_label(color, label);
                }
            });
            ui.horizontal(|ui| {
                if ui.button(tr(&lang, "inbox_open_browser")).clicked() {
                    app.inbox_open_browser();
                }
                let added = app.app.lock_recover().inbox_added;
                if app.nd_kind == "PullRequest" {
                    if added {
                        ui.weak(tr(&lang, "inbox_added"));
                    } else if ui.button(tr(&lang, "inbox_add_review")).clicked() {
                        if let Some(i) = app.inbox_sel {
                            app.inbox_add_review(i);
                        }
                    }
                }
            });
            ui.separator();
            // ── tab 条：会话 / Files（对齐主面板的合并语义：Files tab 内嵌可折叠 diff）──
            let tabs = [tr(&lang, "tab_conversation"), tr(&lang, "tab_files")];
            ui.horizontal(|ui| {
                for (i, t) in tabs.iter().enumerate() {
                    if ui.selectable_label(app.nd_tab == i, *t).clicked() && app.nd_tab != i {
                        app.nd_tab = i;
                    }
                }
            });
            ui.separator();
            if app.nd_loading {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.weak(tr(&lang, "inbox_loading"));
                });
                return;
            }
            // 每个 tab 独立滚动区；salt 带 nd_epoch——打开新通知时滚动回到顶部
            egui::ScrollArea::vertical()
                .id_salt(("nd-tab", app.nd_epoch, app.nd_tab))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if app.nd_tab == 0 {
                        // ── 会话 ──
                        if app.nd_convo.is_empty() {
                            ui.weak(tr(&lang, "inbox_convo_empty_hint"));
                        } else {
                            for e in app.nd_convo.clone() {
                                // horizontal_wrapped：超长作者名/时间戳在行内换行而不是溢出
                                // （单行溢出会撑大内容 ui 的 max_rect，带歪后续所有换行宽度）
                                ui.horizontal_wrapped(|ui| {
                                    ui_detail::avatar_chip(ui, &e.author);
                                    ui.strong(&e.author);
                                    ui.weak(&e.time);
                                });
                                if !e.text.is_empty() {
                                    egui_commonmark::CommonMarkViewer::new()
                                        .show(ui, &mut app.md_cache, &e.text);
                                }
                                ui.separator();
                            }
                        }
                    } else {
                        // ── Files（主面板同款：有解析后 diff 用折叠 diff 视图，否则纯清单）──
                        if !app.nd_diff.is_empty() {
                            let mut files = std::mem::take(&mut app.nd_diff);
                            ui_detail::diff_view(ui, &mut files, &lang, "nd");
                            app.nd_diff = files;
                        } else {
                            ui_detail::files_tab(ui, &app.nd_files, &lang);
                        }
                    }
                });
        });
}
