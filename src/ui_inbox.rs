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

        // 表头
        ui.horizontal(|ui| {
            ui.weak(tr(&lang, "inbox_col_repo"));
            ui.add_space(140.0);
            ui.weak(tr(&lang, "inbox_col_title"));
        });

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

fn inbox_row(app: &mut EguiApp, ui: &mut Ui, n: &serde_json::Value, i: usize, lang: &str) {
    let repo = n["repo"].as_str().unwrap_or("");
    let title = n["title"].as_str().unwrap_or("");
    let kind = n["kind"].as_str().unwrap_or("");
    let unread = n["unread"].as_bool().unwrap_or(false);
    let selected = app.inbox_sel == Some(i)
        || app
            .app
            .lock_recover()
            .inbox_detail
            .as_ref()
            .is_some_and(|(r, num, _, url)| {
                r == repo && Some(*num) == notif_url_number(url)
            });

    let height = 30.0;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
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
    // 未读圆点
    if unread {
        p.circle_filled(rect.left_center() + egui::vec2(8.0, 0.0), 3.0, visuals.hyperlink_color);
    }
    let font = FontId::default();
    p.text(
        rect.left_center() + egui::vec2(18.0, 0.0),
        egui::Align2::LEFT_CENTER,
        repo,
        font.clone(),
        weak,
    );
    p.text(
        rect.left_center() + egui::vec2(170.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        font.clone(),
        if unread { text_color } else { weak },
    );
    // 右侧：kind 标签 / reason / 相对时间 / 已加入标记
    let mut rx = rect.right() - 8.0;
    let put_right = |p: &egui::Painter, rx: &mut f32, s: String, c: egui::Color32| {
        p.text(egui::pos2(*rx, rect.center().y), egui::Align2::RIGHT_CENTER, s, FontId::proportional(11.0), c);
        *rx -= 70.0;
    };
    put_right(p, &mut rx, rel_time(n["updated_at"].as_str().unwrap_or("")), weak);
    put_right(p, &mut rx, notif_reason_label(lang, n["reason"].as_str().unwrap_or("")), weak);
    put_right(p, &mut rx, notif_kind_label(lang, kind), weak);
    let state = n["state"].as_str().unwrap_or("");
    if !state.is_empty() {
        let (label, color) = match state {
            "merged" => (tr(lang, "inbox_state_merged"), ui_detail::state_color("merged", visuals.dark_mode)),
            "closed" => (tr(lang, "inbox_state_closed"), ui_detail::state_color("closed", visuals.dark_mode)),
            _ => (tr(lang, "inbox_state_open"), ui_detail::state_color("open", visuals.dark_mode)),
        };
        put_right(p, &mut rx, label.to_string(), color);
    }
    // 「已加入」标记：PullRequest 且已在库
    if kind == "PullRequest"
        && let Some(num) = notif_url_number(n["url"].as_str().unwrap_or(""))
    {
        let added = app
            .app
            .lock_recover()
            .db
            .pr_exists(repo, num)
            .unwrap_or(true);
        if added {
            p.text(
                egui::pos2(rx + 30.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                tr(lang, "inbox_added"),
                FontId::proportional(11.0),
                visuals.weak_text_color(),
            );
        }
    }

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
            egui::ScrollArea::vertical()
                .id_salt("nd-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // ── 头部 ──
                    ui.horizontal(|ui| {
                        let title = if app.nd_title.is_empty() {
                            tr(&lang, "inbox_not_selected")
                        } else {
                            &app.nd_title
                        };
                        ui.heading(RichText::new(title).size(15.0));
                        if ui.small_button("✕").clicked() {
                            app.nd_open = false;
                            app.app.lock_recover().inbox_detail = None;
                        }
                    });
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
                    if app.nd_loading {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new());
                            ui.weak(tr(&lang, "inbox_loading"));
                        });
                        return;
                    }
                    // ── 会话 ──
                    if app.nd_convo.is_empty() {
                        ui.weak(tr(&lang, "inbox_convo_empty_hint"));
                    } else {
                        for e in app.nd_convo.clone() {
                            ui.horizontal(|ui| {
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
                    // ── Files ──
                    if !app.nd_files.is_empty() {
                        ui.add_space(8.0);
                        ui.label(tr(&lang, "tab_files"));
                        for (path, adds, dels) in app.nd_files.clone() {
                            ui.horizontal(|ui| {
                                ui.monospace(path);
                                ui.weak(format!("+{adds}/-{dels}"));
                            });
                        }
                    }
                    // ── Diff ──
                    if !app.nd_diff.is_empty() {
                        ui.add_space(8.0);
                        ui.label(tr(&lang, "tab_diff"));
                        let mut files = std::mem::take(&mut app.nd_diff);
                        ui_detail::diff_view(ui, &mut files, &lang, "nd");
                        app.nd_diff = files;
                    }
                });
        });
}
