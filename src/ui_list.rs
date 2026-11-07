//! 顶栏 / 状态栏 / 多选操作条 / PR 列表视图（分组 + 排序 + 多选 + hover 快捷 + 右键菜单）。
//! 列布局：每列内容裁剪到自己列宽（clip + 省略号），表头分隔线可拖拽调宽。

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
                if ui
                    .selectable_label(app.view_mode == i, *t)
                    .on_hover_text(tr(&lang, if i == 0 { "tab_list" } else { "tab_inbox" }))
                    .clicked()
                    && app.view_mode != i
                {
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
            let tip = if dark { tr(&lang, "theme_tooltip_light") } else { tr(&lang, "theme_tooltip_dark") };
            if ui.button(label).on_hover_text(tip).clicked() {
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
            // 优先级下拉（显示层映射：内部 1=P0 最紧急 / 2=P1 / 3=P2）
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

/// 列规格：(排序 id, 表头 i18n key 或固定文本, 默认宽, 最小宽；flex=标题列吃剩余宽)
pub struct ColSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub def_w: f32,
    pub min_w: f32,
    pub flex: bool,
}

pub const COL_SPECS: &[ColSpec] = &[
    ColSpec { id: "priority", label: "P", def_w: 26.0, min_w: 24.0, flex: false },
    ColSpec { id: "num", label: "#", def_w: 60.0, min_w: 44.0, flex: false },
    ColSpec { id: "title", label: "col_title", def_w: 0.0, min_w: 120.0, flex: true },
    ColSpec { id: "author", label: "col_author", def_w: 110.0, min_w: 60.0, flex: false },
    ColSpec { id: "turn", label: "col_turn", def_w: 38.0, min_w: 30.0, flex: false },
    ColSpec { id: "review", label: "col_review", def_w: 84.0, min_w: 40.0, flex: false },
    ColSpec { id: "ci", label: "CI", def_w: 40.0, min_w: 32.0, flex: false },
    ColSpec { id: "size", label: "col_size", def_w: 40.0, min_w: 32.0, flex: false },
    ColSpec { id: "changes", label: "+/-", def_w: 70.0, min_w: 52.0, flex: false },
    ColSpec { id: "score", label: "★", def_w: 40.0, min_w: 32.0, flex: false },
    ColSpec { id: "upd", label: "col_updated", def_w: 48.0, min_w: 40.0, flex: false },
];

/// 列宽初值（标题列占位的 0 会在布局时按剩余宽计算，不进 col_widths）。
pub fn default_col_widths() -> Vec<f32> {
    COL_SPECS.iter().map(|c| if c.flex { 0.0 } else { c.def_w }).collect()
}

fn col_label<'a>(key: &'a str, lang: &str) -> &'a str {
    match key {
        "col_title" | "col_author" | "col_turn" | "col_review" | "col_size" | "col_updated" => {
            tr(lang, key)
        }
        other => other,
    }
}

/// 行/表头共用的列布局：返回每列的 x 起点与宽度（标题列 = 剩余宽）。
/// out: Vec<(x, w)>（相对 row/header 矩形左缘）。
fn col_layout(total_w: f32, widths: &[f32]) -> Vec<(f32, f32)> {
    let fixed: f32 = widths.iter().sum();
    let title_w = (total_w - fixed - 8.0).max(COL_SPECS[2].min_w);
    let mut out = Vec::with_capacity(COL_SPECS.len());
    let mut x = 4.0;
    for (i, c) in COL_SPECS.iter().enumerate() {
        let w = if c.flex { title_w } else { widths[i] };
        out.push((x, w));
        x += w;
    }
    out
}

/// 裁剪 + 省略号画单元格文本（防溢出压到相邻列）。
fn paint_cell(ui: &Ui, cell: egui::Rect, text: &str, font: FontId, color: Color32) {
    if text.is_empty() {
        return;
    }
    let text = ellipsize(ui, text, font.clone(), cell.width() - 4.0);
    ui.painter()
        .with_clip_rect(cell)
        .text(egui::pos2(cell.left() + 2.0, cell.center().y), Align2::LEFT_CENTER, text, font, color);
}

/// 超出宽度时截断加「…」；按字符步进测量（行数少，成本可忽略）。
fn ellipsize(ui: &Ui, text: &str, font: FontId, max_w: f32) -> String {
    let measure = |ui: &Ui, s: String| {
        ui.fonts_mut(|f| f.layout_no_wrap(s, font.clone(), Color32::WHITE).rect.width())
    };
    if measure(ui, text.to_string()) <= max_w {
        return text.to_string();
    }
    let ell_w = measure(ui, "…".to_string());
    let mut out = String::new();
    let mut w = 0.0;
    for c in text.chars() {
        let cw = measure(ui, c.to_string());
        if w + cw + ell_w > max_w {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// 优先级徽章：内部存储 1/2/3，显示层 1→P0（最紧急）2→P1 3→P2；0=未设不显示。
pub fn priority_badge(p: i64) -> (&'static str, Color32) {
    match p {
        1 => ("P0", Color32::from_rgb(209, 36, 47)),
        2 => ("P1", Color32::from_rgb(209, 100, 36)),
        3 => ("P2", Color32::from_rgb(158, 126, 0)),
        _ => ("", Color32::TRANSPARENT),
    }
}

/// Rev 列语义图标（painter 矢量，扁平风）：approved=绿勾 / changes_requested=红叹号 /
/// commented=灰气泡 / failed=红叉；未知/空不画。
fn paint_rev_icon(ui: &Ui, c: egui::Rect, review: &str, _dark: bool) {
    let ctr = c.center(); // 列内居中（钉死：任何情况下不随内容漂移出列框）
    let p = ui.painter().with_clip_rect(c);
    let stroke = |p: &egui::Painter, pts: [egui::Pos2; 2], color: Color32| {
        p.line_segment(pts, egui::Stroke::new(1.6, color));
    };
    match review {
        "approved" => {
            let g = Color32::from_rgb(46, 160, 67);
            stroke(&p, [ctr + egui::vec2(-4.0, 0.5), ctr + egui::vec2(-1.0, 3.5)], g);
            stroke(&p, [ctr + egui::vec2(-1.0, 3.5), ctr + egui::vec2(4.5, -3.5)], g);
        }
        "changes_requested" => {
            let r = Color32::from_rgb(209, 36, 47);
            stroke(&p, [ctr + egui::vec2(0.0, -4.0), ctr + egui::vec2(0.0, 1.0)], r);
            p.circle_filled(ctr + egui::vec2(0.0, 4.0), 1.2, r);
        }
        "commented" => {
            let g = Color32::from_rgb(150, 150, 150);
            let b = egui::Rect::from_center_size(ctr, egui::vec2(9.0, 7.0));
            p.rect_stroke(b, 2.0, egui::Stroke::new(1.2, g), egui::StrokeKind::Inside);
            stroke(&p, [b.left_bottom() + egui::vec2(2.0, 0.0), b.left_bottom() + egui::vec2(0.5, 2.5)], g);
        }
        "failed" => {
            let r = Color32::from_rgb(209, 36, 47);
            stroke(&p, [ctr + egui::vec2(-3.5, -3.5), ctr + egui::vec2(3.5, 3.5)], r);
            stroke(&p, [ctr + egui::vec2(3.5, -3.5), ctr + egui::vec2(-3.5, 3.5)], r);
        }
        _ => {}
    }
}

pub fn pr_list_view(app: &mut EguiApp, ui: &mut Ui) {
    let lang = app.lang();
    egui::CentralPanel::default().show(ui, |ui| {
        header_row(app, ui, &lang);
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

/// 表头：点击排序（Excel 式）+ 分隔线拖拽调宽 + hover 反馈。
fn header_row(app: &mut EguiApp, ui: &mut Ui, lang: &str) {
    let (sort_col, sort_asc) = {
        let a = app.app.lock_recover();
        (a.sort_col.clone(), a.sort_asc)
    };
    let total_w = ui.available_width();
    let layout = col_layout(total_w, &app.col_widths);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, 22.0), egui::Sense::hover());
    // debug 覆盖层与数据行同宽（含滚动条偏移差异检查）
    if app.show_col_grid {
        for (x, w) in &layout {
            ui.painter().line_segment(
                [egui::pos2(rect.left() + x + w, rect.top()), egui::pos2(rect.left() + x + w, rect.bottom())],
                egui::Stroke::new(1.0, Color32::from_rgb(255, 0, 200)),
            );
        }
    }
    let sep_base = ui.visuals().weak_text_color().gamma_multiply(0.25);
    let sep_hot = ui.visuals().hyperlink_color;
    for (i, c) in COL_SPECS.iter().enumerate() {
        let (x, w) = layout[i];
        let cell = egui::Rect::from_min_size(egui::pos2(rect.left() + x, rect.top()), egui::vec2(w, rect.height()));
        let resp = ui.interact(cell, egui::Id::unique(("col-h", c.id)), egui::Sense::click());
        // hover 反馈：底色 + 手型 + 提示
        if resp.hovered() {
            ui.painter().rect_filled(cell, 2.0, ui.visuals().widgets.hovered.bg_fill);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        resp.clone().on_hover_text(tr(lang, "tip_sort"));
        let arrow = if sort_col == c.id {
            if sort_asc { " ▲" } else { " ▼" }
        } else {
            ""
        };
        // 表头文字水平居中
        let text = format!("{}{arrow}", col_label(c.label, lang));
        let text = ellipsize(ui, &text, FontId::proportional(11.0), cell.width() - 4.0);
        ui.painter()
            .with_clip_rect(cell)
            .text(cell.center(), Align2::CENTER_CENTER, text, FontId::proportional(11.0), ui.visuals().weak_text_color());
        // 拖拽调宽后松开不得触发排序（拖拽期间/刚松开的 click 被抑制）
        let dragging = app.header_drag_at.is_some_and(|t| t.elapsed().as_millis() < 300);
        if resp.clicked() && !dragging {
            app.header_clicked(c.id);
        }
        // 常驻细分隔线 + grip 指示（列边界；弹性列与最后一列不画右缘）
        let is_last = i + 1 == COL_SPECS.len();
        if !is_last {
            let bx = cell.right();
            // 把手热区加宽到 16px（±8），hit test 优先于表头点击（后建者优先）
            let handle = egui::Rect::from_min_size(
                egui::pos2(bx - 8.0, cell.top()),
                egui::vec2(16.0, cell.height()),
            );
            let dresp = if c.flex {
                None
            } else {
                Some(ui.interact(handle, egui::Id::unique(("col-drag", c.id)), egui::Sense::click_and_drag()))
            };
            let hot = dresp.as_ref().is_some_and(|d| d.hovered() || d.dragged());
            if let Some(d) = &dresp {
                if d.hovered() || d.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if d.dragged() {
                    let dx = d.drag_delta().x;
                    let new_w = (app.col_widths[i] + dx).max(c.min_w);
                    app.col_widths[i] = new_w;
                    // 拖拽发生后抑制本列的排序点击（松开不当成 click）
                    app.header_drag_at = Some(std::time::Instant::now());
                }
                if d.drag_stopped() {
                    app.header_drag_at = Some(std::time::Instant::now());
                }
            }
            // grip：两个竖点（⋮ 风格，painter 画，不依赖字形）；hover 变主题色
            let grip_c = if hot { sep_hot } else { sep_base };
            for dy in [-3.0, 0.0, 3.0] {
                ui.painter().circle_filled(
                    egui::pos2(bx, cell.center().y + dy),
                    if hot { 1.6 } else { 1.2 },
                    grip_c,
                );
            }
            // 整段分隔线（常驻低对比；hover 变亮变粗）
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(bx - 1.0, cell.top() + 4.0),
                    egui::vec2(if hot { 2.0 } else { 1.0 }, cell.height() - 8.0),
                ),
                0.5,
                if hot { sep_hot } else { sep_base },
            );
        }
    }
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
    let font = FontId::default();
    let small = FontId::proportional(11.0);
    let layout = col_layout(rect.width(), &app.col_widths);
    // debug 列边界覆盖层（验证「内容钉死在列框内」用；默认关）
    if app.show_col_grid {
        for (_, w) in &layout {
            let _ = w;
        }
        let mut gx = rect.left() + 4.0;
        for (x, w) in &layout {
            let _ = (x, w);
            gx = rect.left() + x + w;
            ui.painter().line_segment(
                [egui::pos2(gx, rect.top()), egui::pos2(gx, rect.bottom())],
                egui::Stroke::new(1.0, Color32::from_rgb(255, 0, 200)),
            );
        }
        let _ = gx;
    }
    let cell = |i: usize| -> egui::Rect {
        let (x, w) = layout[i];
        egui::Rect::from_min_max(egui::pos2(rect.left() + x, rect.top()), egui::pos2(rect.left() + x + w, rect.bottom()))
    };

    // P（优先级徽章：1→P0 最紧急 / 2→P1 / 3→P2；0=未设不显示）
    let (badge, badge_color) = priority_badge(row.priority);
    if !badge.is_empty() {
        paint_cell(ui, cell(0), badge, small.clone(), badge_color);
    }
    // #编号：可点超链接（浏览器打开 PR）
    {
        let key = row.key.clone();
        let r = ui.put(
            cell(1),
            egui::Label::new(
                RichText::new(format!("#{}", key.1))
                    .size(11.0)
                    .color(visuals.hyperlink_color)
                    .underline(),
            )
            .sense(egui::Sense::click()),
        );
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        r.clone().on_hover_text(tr(lang, "inbox_open_browser"));
        if r.clicked() {
            worker::open_pr_url(&app.app, &key.0, &key.1);
        }
    }
    // 标题（pinned=琥珀三角 / 看板=蓝菱形，painter 绘制，不依赖 emoji 字形）
    {
        let c = cell(2);
        let mut tx = c.left() + 2.0;
        let cy = c.center().y;
        if row.pinned {
            let p = ui.painter().with_clip_rect(c);
            let pts = vec![
                egui::pos2(tx, cy + 4.0),
                egui::pos2(tx + 5.0, cy - 4.0),
                egui::pos2(tx + 10.0, cy + 4.0),
            ];
            p.add(egui::Shape::convex_polygon(pts, Color32::from_rgb(191, 135, 0), egui::Stroke::NONE));
            tx += 14.0;
        }
        if row.on_board {
            let p = ui.painter().with_clip_rect(c);
            let pts = vec![
                egui::pos2(tx + 4.0, cy - 5.0),
                egui::pos2(tx + 9.0, cy),
                egui::pos2(tx + 4.0, cy + 5.0),
                egui::pos2(tx - 1.0, cy),
            ];
            p.add(egui::Shape::convex_polygon(pts, Color32::from_rgb(56, 139, 253), egui::Stroke::NONE));
            tx += 13.0;
        }
        let text_cell = egui::Rect::from_min_max(egui::pos2(tx, c.top()), c.max);
        paint_cell(ui, text_cell, &row.title, font.clone(), text_color);
    }
    // 作者列：圆形头像（固定 16px，加载前后行高一致）+ 名字 + 组织图标紧跟名字
    {
        let c = cell(3);
        let avatar_r = egui::Rect::from_min_size(
            egui::pos2(c.left() + 2.0, c.center().y - 8.0),
            egui::vec2(16.0, 16.0),
        );
        // 头像：磁盘缓存纹理；未命中用圆形字母占位（与详情面板回退同风格）
        match crate::avatars::get_avatar(ui.ctx(), &row.author) {
            Some(tex) => {
                ui.painter().image(
                    tex.id(),
                    avatar_r,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                let cp = ui.painter().with_clip_rect(c);
                cp.circle_filled(avatar_r.center(), 8.0, Color32::from_rgb(90, 90, 120));
                cp.text(
                    avatar_r.center(),
                    Align2::CENTER_CENTER,
                    avatar_initial(&row.author),
                    FontId::proportional(9.0),
                    Color32::WHITE,
                );
            }
        }
        // 组织图标：钉死在列内右缘（不随名字长度漂移），裁进列宽，hover 提示组织名
        let orgs: Vec<&String> = row.orgs.iter().take(3).collect();
        let icons_w = orgs.len() as f32 * 14.0;
        let icons_left = if icons_w > 0.0 { c.right() - 2.0 - icons_w } else { c.right() - 2.0 };
        let name_x = avatar_r.right() + 4.0;
        let name_max_w = (icons_left - 4.0 - name_x).max(0.0);
        let name = ellipsize(ui, &row.author, small.clone(), name_max_w);
        ui.painter().with_clip_rect(c).text(
            egui::pos2(name_x, c.center().y),
            Align2::LEFT_CENTER,
            name,
            small.clone(),
            weak,
        );
        let cp = ui.painter().with_clip_rect(c);
        for (oi, org) in orgs.iter().enumerate() {
            let ir = egui::Rect::from_min_size(
                egui::pos2(icons_left + oi as f32 * 14.0, c.center().y - 6.0),
                egui::vec2(12.0, 12.0),
            );
            if let Some(tex) = crate::avatars::get_org_icon(ui.ctx(), org) {
                cp.image(tex.id(), ir, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            // hover 显示组织名（行数据 author_orgs 直接带名字）
            let resp = ui.interact(ir, egui::Id::unique(("org", row.key.clone(), oi)), egui::Sense::hover());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
            }
            resp.on_hover_text((*org).clone());
        }
    }
    // Turn 列：药丸徽章（painter 圆角矩形 + 文字，me=淡蓝/them=灰）
    {
        let c = cell(4);
        let (label, bg, fg) = match row.turn.as_str() {
            "me" => (tr(lang, "turn_me"), Color32::from_rgb(0x1f, 0x6f, 0xeb).gamma_multiply(0.25), Color32::from_rgb(0x58, 0xa6, 0xff)),
            "them" => (tr(lang, "turn_them"), Color32::from_rgb(0x8a, 0x8a, 0x8a).gamma_multiply(0.25), weak),
            _ => ("", Color32::TRANSPARENT, weak),
        };
        if !label.is_empty() {
            let p = ui.painter().with_clip_rect(c);
            // 列内居中（图标列语义）
            let tw = ui.fonts_mut(|f| f.layout_no_wrap(label.to_string(), small.clone(), fg).rect.width());
            let pr = egui::Rect::from_center_size(c.center(), egui::vec2(tw + 10.0, 16.0));
            p.rect_filled(pr, 8.0, bg);
            p.text(pr.center(), Align2::CENTER_CENTER, label, small.clone(), fg);
        }
    }
    // Rev 列：painter 矢量语义图标（旧版是 SVG；文字徽章会让列宽被顶开）
    {
        let c = cell(5);
        paint_rev_icon(ui, c, &row.review, dark);
        if row.review.is_empty() && !row.activity.is_empty() {
            paint_cell(ui, c, &row.activity, small.clone(), weak);
        }
    }
    // CI 圆点
    if !row.ci.is_empty() {
        let c = cell(6);
        ui.painter().circle_filled(c.center(), 3.5, ui_detail::ci_color(&row.ci));
    }
    paint_cell(ui, cell(7), &row.size, small.clone(), weak);
    paint_cell(ui, cell(8), &format!("+{}/-{}", row.adds, row.dels), small.clone(), weak);
    // ★ 评分列：列内居中（图标列语义）
    if row.score > 0.0 {
        let c = cell(9);
        let t = ellipsize(ui, &format!("{:.1}", row.score), small.clone(), c.width() - 4.0);
        ui.painter().with_clip_rect(c).text(
            c.center(),
            Align2::CENTER_CENTER,
            t,
            small.clone(),
            ui_detail::score_color(row.score, dark),
        );
    }
    paint_cell(ui, cell(10), &row.age, small.clone(), weak);

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
                if ui.small_button("+").on_hover_text(tr(lang, "detail_add_board")).clicked() {
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
