//! 顶栏 / 状态栏 / 多选操作条 / PR 列表视图（分组 + 排序 + 多选 + hover 快捷 + 右键菜单）。

use std::time::Instant;

use egui::{Align2, Color32, FontId, RichText, Ui};

use pr_desk_rs::i18n::{tr, trf};

use crate::state::*;
use crate::{ui_detail, worker, EguiApp};

pub fn search_id() -> egui::Id {
    egui::Id::unique("search-box")
}

// ── 顶栏 ──

pub fn top_bar(app: &mut EguiApp, ui: &mut Ui) {
    egui::Panel::top("top_bar").show(ui, |ui| {
        ui.horizontal(|ui| {
            let (lang, refreshing, syncing) = {
                let a = app.app.lock_recover();
                (a.cfg.language.clone(), a.gh_refreshing, a.sync_inflight)
            };
            // 刷新：连点去重——一轮刷新在途时忽略后续点击（避免排队跑 N 轮全量拉取）
            if ui
                .button(tr(&lang, "tb_refresh"))
                .on_hover_text(tr(&lang, "refresh_tooltip"))
                .clicked()
            {
                let send = {
                    let mut a = app.app.lock_recover();
                    if a.gh_refreshing {
                        false
                    } else {
                        a.gh_refreshing = true;
                        true
                    }
                };
                if send {
                    app.last_poll = Instant::now();
                    let _ = app.cmd_tx.send(Cmd::RefreshGithub);
                }
            }
            if refreshing {
                ui.add(egui::Spinner::new());
            }
            // 同步：同刷新（连点去重）
            if ui.button("⇅").on_hover_text(tr(&lang, "sync_tooltip")).clicked() {
                let send = {
                    let mut a = app.app.lock_recover();
                    if a.sync_inflight {
                        false
                    } else {
                        a.sync_inflight = true;
                        true
                    }
                };
                if send {
                    let _ = app.cmd_tx.send(Cmd::Sync);
                }
            }
            if syncing {
                ui.add(egui::Spinner::new());
            }
            ui.separator();
            // 搜索（编辑即重算过滤视图）
            let filter_resp = ui.add(
                egui::TextEdit::singleline(&mut app.filter_buf)
                    .id(search_id())
                    .hint_text(tr(&lang, "search_placeholder"))
                    .desired_width(220.0),
            );
            if filter_resp.changed() {
                let mut a = app.app.lock_recover();
                a.filter = app.filter_buf.clone();
                recompute_flat(&mut a);
            }
            ui.separator();
            // 视图切换：列表 / Inbox
            for (i, t) in [tr(&lang, "tab_list"), tr(&lang, "tab_inbox")].iter().enumerate() {
                if ui.selectable_label(app.view_mode == i, *t).clicked() && app.view_mode != i {
                    app.view_mode = i;
                    if i == 1 {
                        // 首次进 Inbox：拉取通知（加载态，不显示误导空态）
                        app.inbox_loading = true;
                        let _ = app.cmd_tx.send(Cmd::FetchInbox);
                    }
                }
            }
            ui.separator();
            // 数据源档案切换（worker 校验档案存在 → 换库 + 重建同步客户端 + 刷新）
            let active = app.app.lock_recover().cfg.active_profile.clone();
            let active_label = if active == "local" {
                tr(&lang, "source_local").to_string()
            } else {
                active.clone()
            };
            egui::ComboBox::from_id_salt("profile-switcher")
                .selected_text(active_label)
                .show_ui(ui, |ui| {
                    for p in &app.profile_names {
                        let text = if p == "local" { tr(&lang, "source_local").to_string() } else { p.clone() };
                        if ui.selectable_label(active == *p, text).clicked() && active != *p {
                            let _ = app.cmd_tx.send(Cmd::SwitchProfile(p.clone()));
                        }
                    }
                })
                .response
                .on_hover_text(tr(&lang, "source_switcher_tooltip"));
            ui.separator();
            // 高分快筛开关（只看评分 ≥8.0 的 PR）
            let mut hs = app.app.lock_recover().min_score > 0.0;
            if ui
                .toggle_value(&mut hs, RichText::new("★≥8").size(11.0))
                .on_hover_text(tr(&lang, "high_score_tooltip"))
                .changed()
            {
                {
                    let mut a = app.app.lock_recover();
                    a.min_score = if hs { 8.0 } else { 0.0 };
                    recompute_flat(&mut a);
                }
            }
            ui.separator();
            // 主题切换
            let dark = app.app.lock_recover().cfg.theme == "dark";
            let label = if dark { tr(&lang, "tb_theme_light") } else { tr(&lang, "tb_theme_dark") };
            if ui.button(label).clicked() {
                app.theme_toggled(ui.ctx());
            }
            // 设置
            if ui
                .button("⚙")
                .on_hover_text(tr(&lang, "settings_tooltip"))
                .clicked()
            {
                crate::ui_settings::open_settings(app);
            }
        });
    });
}

// ── 状态栏 ──

pub fn status_bar(app: &EguiApp, ui: &mut Ui) {
    egui::Panel::bottom("status_bar").show(ui, |ui| {
        ui.horizontal(|ui| {
            let lang = app.lang();
            ui.label(app.status_bar_sync_text());
            ui.weak(tr(&lang, if app.view_mode == 0 { "statusbar_hints" } else { "statusbar_hints_inbox" }));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let a = app.app.lock_recover();
                ui.weak(trf(&lang, "sb_pr_count", &[&a.total.to_string()]));
                if let Some(q) = *a.rate_limit.lock_recover() {
                    ui.weak(trf(&lang, "sb_api_quota", &[&q.to_string()]));
                }
            });
        });
    });
}

// ── 多选操作条 ──

pub fn selection_bar(app: &mut EguiApp, ui: &mut Ui) {
    let (n, all_pinned, all_on_board, all_rejected) = {
        let a = app.app.lock_recover();
        selection_summary(&a)
    };
    if n == 0 {
        return;
    }
    let lang = app.lang();
    egui::Panel::bottom("selection_bar").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.strong(format!(
                "{}{}{}",
                tr(&lang, "selbar_count_pre"),
                n,
                tr(&lang, "selbar_count_post")
            ));
            ui.separator();
            // 优先级下拉
            egui::ComboBox::from_id_salt("sel-prio")
                .selected_text(tr(&lang, "ctx_priority"))
                .show_ui(ui, |ui| {
                    for (v, key) in [
                        (1, "ctx_priority_high"),
                        (2, "ctx_priority_medium"),
                        (3, "ctx_priority_low"),
                        (0, "ctx_priority_clear"),
                    ] {
                        if ui.button(format!("{} [{v}]", tr(&lang, key))).clicked() {
                            batch_set_priority(app, v);
                        }
                    }
                });
            if ui
                .button(if all_pinned { tr(&lang, "ctx_unpin") } else { tr(&lang, "tip_pin") })
                .clicked()
            {
                worker::batch_toggle(&app.app, "pinned");
            }
            if ui
                .button(if all_on_board { tr(&lang, "ctx_remove_board") } else { tr(&lang, "detail_add_board") })
                .clicked()
            {
                worker::batch_toggle(&app.app, "on_board");
            }
            if ui
                .button(if all_rejected { tr(&lang, "ctx_restore") } else { tr(&lang, "ctx_skip") })
                .clicked()
            {
                batch_set_status(app, if all_rejected { "pending" } else { "rejected" });
            }
            if ui.button(tr(&lang, "btn_review_start")).clicked() {
                batch_run_review(app);
            }
            if ui.button(tr(&lang, "ctx_copy_links")).clicked() {
                if let Some(text) = worker::copy_links_text(&app.app) {
                    let _ = app.cmd_tx.send(Cmd::CopyLinks(text));
                }
            }
            if ui.button(tr(&lang, "inbox_open_browser")).clicked() {
                worker::batch_open_browser(&app.app);
            }
            if ui.button(tr(&lang, "ctx_clear_selection")).clicked() {
                app.clear_multi_selection();
            }
        });
    });
}

fn batch_set_priority(app: &EguiApp, p: i64) {
    let keys = {
        let a = app.app.lock_recover();
        effective_targets(&a)
    };
    if !keys.is_empty() {
        worker::batch_local_update(&app.app, &keys, "priority", p);
    }
}

/// 批量状态："rejected"（跳过）/ "pending"（恢复）。
fn batch_set_status(app: &EguiApp, status: &str) {
    let status = match status {
        "rejected" | "pending" => status,
        _ => return, // 白名单外的值不进库
    };
    let keys = {
        let a = app.app.lock_recover();
        effective_targets(&a)
    };
    if !keys.is_empty() {
        worker::batch_local_update(&app.app, &keys, "status", status);
    }
}

/// 批量预评审：逐行发 Cmd::RunReview（review_slots 并发槽排队/去重由 worker 侧保证）。
fn batch_run_review(app: &EguiApp) {
    let keys = {
        let a = app.app.lock_recover();
        effective_targets(&a)
    };
    if keys.is_empty() {
        return;
    }
    let n = keys.len();
    for (repo, number) in keys {
        let _ = app.cmd_tx.send(Cmd::RunReview { repo, number });
    }
    let lang = app.lang();
    worker::send_msg(Msg::Status(trf(&lang, "review_queued", &[&n.to_string()])));
}

// ── PR 列表 ──

/// 行渲染用的快照数据（渲染期间不持 App 锁）。
struct RowData {
    key: (String, i64),
    title: String,
    author: String,
    turn: String,
    review: String,
    ci: String,
    size: String,
    adds: i64,
    dels: i64,
    score: f64,
    age: String,
    activity: String,
    orgs: Vec<String>,
    priority: i64,
    pinned: bool,
    on_board: bool,
}

fn row_data(a: &App, idx: usize) -> RowData {
    let r = &a.rows[idx];
    RowData {
        key: (
            r["repo"].as_str().unwrap_or("").to_string(),
            r["number"].as_i64().unwrap_or(0),
        ),
        title: r["title"].as_str().unwrap_or("").to_string(),
        author: r["author"].as_str().unwrap_or("").to_string(),
        turn: r["turn"].as_str().unwrap_or("").to_string(),
        review: r["review_decision"].as_str().unwrap_or("").to_string(),
        ci: r["ci_status"].as_str().unwrap_or("").to_string(),
        size: r["size_label"].as_str().unwrap_or("").to_string(),
        adds: r["additions"].as_i64().unwrap_or(0),
        dels: r["deletions"].as_i64().unwrap_or(0),
        score: r["score"].as_f64().unwrap_or(0.0),
        age: rel_time(r["updated_at"].as_str().unwrap_or("")),
        activity: activity_text(r, &a.cfg.language),
        orgs: author_orgs(r),
        priority: r["priority"].as_i64().unwrap_or(0),
        pinned: r["pinned"].as_i64() == Some(1),
        on_board: r["on_board"].as_i64() == Some(1),
    }
}

/// 列定义：(排序 id, 表头 i18n key 或固定文本, 宽度；宽度 0 = 弹性)
const COLS: &[(&str, &str, f32)] = &[
    ("priority", "P", 20.0),
    ("num", "#", 56.0),
    ("title", "col_title", 0.0),
    ("author", "col_author", 100.0),
    ("turn", "col_turn", 36.0),
    ("review", "col_review", 44.0),
    ("ci", "CI", 40.0),
    ("size", "col_size", 36.0),
    ("changes", "+/-", 66.0),
    ("score", "★", 36.0),
    ("upd", "col_updated", 46.0),
];

fn col_label<'a>(key: &'a str, lang: &str) -> &'a str {
    match key {
        "col_title" | "col_author" | "col_turn" | "col_review" | "col_size" | "col_updated" => {
            // i18n 键：tr 返回的是输入 key 的 &'static str 等价物，直接返回查询值
            tr(lang, key)
        }
        other => other,
    }
}

pub fn pr_list_view(app: &mut EguiApp, ui: &mut Ui) {
    let lang = app.lang();
    egui::CentralPanel::default().show(ui, |ui| {
        // ── 表头（Excel 式点击排序：同列反向，新列按默认方向）──
        let (sort_col, sort_asc) = {
            let a = app.app.lock_recover();
            (a.sort_col.clone(), a.sort_asc)
        };
        ui.horizontal(|ui| {
            let avail = ui.available_width();
            let fixed: f32 = COLS.iter().map(|c| c.2).sum();
            for &(cid, label_key, w) in COLS {
                let w = if w == 0.0 { (avail - fixed - 24.0).max(120.0) } else { w };
                let arrow = if sort_col == cid {
                    if sort_asc { " ▲" } else { " ▼" }
                } else {
                    ""
                };
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(w, 20.0),
                    egui::Sense::click(),
                );
                ui.painter().text(
                    rect.left_center(),
                    Align2::LEFT_CENTER,
                    format!("{}{arrow}", col_label(label_key, &lang)),
                    FontId::proportional(11.0),
                    ui.visuals().weak_text_color(),
                );
                if resp.clicked() {
                    app.header_clicked(cid);
                }
            }
        });
        ui.separator();

        // ── 分组 + 行 ──
        let (flat, selected, selection) = {
            let a = app.app.lock_recover();
            (a.flat.clone(), a.selected.clone(), a.selection.clone())
        };
        if flat.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.weak(tr(&lang, "empty_no_pr"));
            });
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("pr-list-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for item in &flat {
                    match item {
                        FlatItem::Header { key, title, count } => {
                            let collapsed = app.app.lock_recover().collapsed.contains(key);
                            let arrow = if collapsed { "▶" } else { "▼" };
                            let resp = ui.add(
                                egui::Label::new(RichText::new(format!("{arrow} {title} ({count})")).strong())
                                    .sense(egui::Sense::click()),
                            );
                            if resp.clicked() {
                                app.group_toggled(key);
                            }
                        }
                        FlatItem::Row { rows_idx } => {
                            let row = {
                                let a = app.app.lock_recover();
                                row_data(&a, *rows_idx)
                            };
                            pr_row(app, ui, &row, selected.as_ref() == Some(&row.key), selection.contains(&row.key), &lang);
                        }
                    }
                }
            });
    });
}

fn pr_row(app: &mut EguiApp, ui: &mut Ui, row: &RowData, selected: bool, multi_selected: bool, lang: &str) {
    let height = 24.0;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    let visuals = ui.visuals();
    let bg = if selected {
        visuals.selection.bg_fill
    } else if multi_selected {
        visuals.selection.bg_fill.gamma_multiply(0.4)
    } else if resp.hovered() {
        visuals.widgets.hovered.bg_fill
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 3.0, bg);

    // j/k 导航滚动跟随
    if app.scroll_to.as_ref() == Some(&row.key) {
        resp.scroll_to_me(Some(egui::Align::Center));
        app.scroll_to = None;
    }

    let dark = visuals.dark_mode;
    let text_color = visuals.text_color();
    let weak = visuals.weak_text_color();
    let p = ui.painter();
    let mut x = rect.left() + 4.0;
    let font = FontId::default();
    let small = FontId::proportional(11.0);
    let col = |p: &egui::Painter, x: &mut f32, w: f32, text: String, font: FontId, color: Color32| {
        p.text(egui::pos2(*x, rect.center().y), Align2::LEFT_CENTER, text, font, color);
        *x += w;
    };
    // P（优先级）
    let prio_text = if row.priority > 0 { format!("P{}", row.priority) } else { String::new() };
    col(p, &mut x, 20.0, prio_text, small.clone(), if row.priority > 0 { Color32::from_rgb(209, 100, 36) } else { weak });
    // #
    col(p, &mut x, 56.0, format!("#{}", row.key.1), small.clone(), weak);
    // 标题（弹性宽）：pinned/看板标记前缀
    let fixed: f32 = COLS.iter().map(|c| c.2).sum();
    let title_w = (rect.width() - fixed - 28.0).max(80.0);
    let title_text = format!(
        "{}{}{}",
        if row.pinned { "▲ " } else { "" },
        if row.on_board { "◆ " } else { "" },
        row.title
    );
    col(p, &mut x, title_w, title_text, font.clone(), text_color);
    col(p, &mut x, 100.0, row.author.clone(), small.clone(), weak);
    // 作者公开组织的图标（最多 3 个；图标由 worker 的 prefetch_orgs 预取到磁盘缓存）
    {
        let icons: Vec<_> = row
            .orgs
            .iter()
            .take(3)
            .filter_map(|o| crate::avatars::get_org_icon(ui.ctx(), o))
            .collect();
        for (oi, tex) in icons.iter().enumerate() {
            let r = egui::Rect::from_min_size(
                egui::pos2(x + oi as f32 * 14.0, rect.center().y - 6.0),
                egui::vec2(12.0, 12.0),
            );
            p.image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        if !icons.is_empty() {
            x += icons.len() as f32 * 14.0;
        }
    }
    col(
        p,
        &mut x,
        36.0,
        match row.turn.as_str() {
            "me" => tr(lang, "turn_me").to_string(),
            "them" => tr(lang, "turn_them").to_string(),
            _ => String::new(),
        },
        small.clone(),
        weak,
    );
    // 评审结论 + 活动行
    let rev = if row.review.is_empty() { row.activity.clone() } else { row.review.clone() };
    col(p, &mut x, 44.0, rev, small.clone(), weak);
    // CI
    let ci_color = ui_detail::ci_color(&row.ci);
    if !row.ci.is_empty() {
        p.circle_filled(egui::pos2(x + 8.0, rect.center().y), 3.5, ci_color);
    }
    x += 40.0;
    col(p, &mut x, 36.0, row.size.clone(), small.clone(), weak);
    col(p, &mut x, 66.0, format!("+{}/-{}", row.adds, row.dels), small.clone(), weak);
    col(
        p,
        &mut x,
        36.0,
        if row.score > 0.0 { format!("{:.1}", row.score) } else { String::new() },
        small.clone(),
        if row.score > 0.0 { ui_detail::score_color(row.score, dark) } else { weak },
    );
    col(p, &mut x, 46.0, row.age.clone(), small.clone(), weak);

    // 点击（带修饰键语义）
    if resp.clicked() {
        let mods = ui.ctx().input(|i| i.modifiers);
        app.row_clicked(row.key.clone(), mods.ctrl, mods.shift);
    }
    // hover 快捷按钮：打开 / pin / 看板 / 跳过（行右缘的小按钮区）
    if resp.hovered() {
        let btn_w = 22.0;
        let strip = egui::Rect::from_min_size(
            egui::pos2(rect.right() - btn_w * 4.0 - 4.0, rect.top() + 1.0),
            egui::vec2(btn_w * 4.0, height - 2.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(strip), |ui| {
            ui.horizontal(|ui| {
                let key = row.key.clone();
                if ui.small_button("↗").on_hover_text(tr(lang, "inbox_open_browser")).clicked() {
                    worker::open_pr_url(&app.app, &key.0, &key.1);
                }
                if ui.small_button("P").on_hover_text(tr(lang, "tip_pin")).clicked() {
                    worker::toggle_field(&app.app, &key.0, key.1, "pinned");
                }
                if ui.small_button("＋").on_hover_text(tr(lang, "detail_add_board")).clicked() {
                    worker::toggle_field(&app.app, &key.0, key.1, "on_board");
                }
                if ui.small_button("×").on_hover_text(tr(lang, "tip_reject")).clicked() {
                    // 跳过/恢复：当前 rejected → pending，否则 → rejected
                    let cur = {
                        let a = app.app.lock_recover();
                        find_row(&a, &key.0, key.1)
                            .and_then(|idx| a.rows[idx]["status"].as_str().map(|s| s.to_string()))
                            .unwrap_or_else(|| "pending".into())
                    };
                    let to = if cur == "rejected" { "pending" } else { "rejected" };
                    worker::local_update(&app.app, &key.0, key.1, "status", to);
                }
            });
        });
    }
    // 右键菜单
    resp.context_menu(|ui| {
        context_menu(app, ui, lang);
    });
}

fn context_menu(app: &mut EguiApp, ui: &mut Ui, lang: &str) {
    let (n, all_pinned, all_on_board, all_rejected) = {
        let a = app.app.lock_recover();
        selection_summary(&a)
    };
    if n <= 1 && ui.button(format!("{} [o]", tr(lang, "inbox_open_browser"))).clicked() {
        worker::batch_open_browser(&app.app);
        ui.close();
    }
    ui.separator();
    for (v, key) in [
        (1, "ctx_priority_high"),
        (2, "ctx_priority_medium"),
        (3, "ctx_priority_low"),
        (0, "ctx_priority_clear"),
    ] {
        if ui.button(format!("{} [{v}]", tr(lang, key))).clicked() {
            batch_set_priority(app, v);
            ui.close();
        }
    }
    if ui
        .button(format!(
            "{} [p]",
            if all_pinned { tr(lang, "ctx_unpin") } else { tr(lang, "tip_pin") }
        ))
        .clicked()
    {
        worker::batch_toggle(&app.app, "pinned");
        ui.close();
    }
    if ui
        .button(if all_on_board { tr(lang, "ctx_remove_board") } else { tr(lang, "detail_add_board") })
        .clicked()
    {
        worker::batch_toggle(&app.app, "on_board");
        ui.close();
    }
    if ui
        .button(if all_rejected { tr(lang, "ctx_restore") } else { tr(lang, "ctx_skip") })
        .clicked()
    {
        batch_set_status(app, if all_rejected { "pending" } else { "rejected" });
        ui.close();
    }
    ui.separator();
    if ui.button(tr(lang, "btn_review_start")).clicked() {
        batch_run_review(app);
        ui.close();
    }
    if ui.button(tr(lang, "ctx_copy_links")).clicked() {
        if let Some(text) = worker::copy_links_text(&app.app) {
            let _ = app.cmd_tx.send(Cmd::CopyLinks(text));
        }
        ui.close();
    }
    ui.separator();
    if ui.button(format!("{} [Esc]", tr(lang, "ctx_clear_selection"))).clicked() {
        app.clear_multi_selection();
        ui.close();
    }
}
