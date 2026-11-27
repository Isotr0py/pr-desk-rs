//! 详情面板：头部 + 评审/会话/Checks/Files/Kimi CLI + 行虚拟化 diff 渲染。

use egui::{Color32, FontId, RichText, Ui};
use pr_desk_rs::diffview::LineKind;
use pr_desk_rs::i18n::tr;

use crate::state::*;
use crate::{EguiApp, avatars};

// ── 颜色 ──

pub fn kind_color(kind: LineKind, dark: bool) -> Color32 {
    match (kind, dark) {
        (LineKind::Add, true) => Color32::from_rgb(86, 211, 100),
        (LineKind::Add, false) => Color32::from_rgb(26, 127, 55),
        (LineKind::Del, true) => Color32::from_rgb(248, 81, 73),
        (LineKind::Del, false) => Color32::from_rgb(209, 36, 47),
        (LineKind::Hunk, true) => Color32::from_rgb(151, 128, 255),
        (LineKind::Hunk, false) => Color32::from_rgb(102, 57, 186),
        (LineKind::Meta, true) => Color32::from_rgb(160, 160, 160),
        (LineKind::Meta, false) => Color32::from_rgb(90, 90, 90),
        (LineKind::Ctx, true) => Color32::from_rgb(190, 190, 190),
        (LineKind::Ctx, false) => Color32::from_rgb(80, 80, 80),
    }
}

pub fn ci_color(ci: &str) -> Color32 {
    match ci {
        "success" => Color32::from_rgb(46, 160, 67),
        "failure" => Color32::from_rgb(209, 36, 47),
        "pending" => Color32::from_rgb(191, 135, 0),
        _ => Color32::GRAY,
    }
}

pub fn score_color(s: f64, dark: bool) -> Color32 {
    match (s, dark) {
        (s, false) if s >= 9.0 => Color32::from_rgb(9, 105, 218),
        (s, false) if s >= 8.0 => Color32::from_rgb(26, 127, 55),
        (s, false) if s >= 7.0 => Color32::from_rgb(154, 103, 0),
        (s, false) if s >= 6.0 => Color32::from_rgb(188, 76, 0),
        (s, true) if s >= 9.0 => Color32::from_rgb(88, 166, 255),
        (s, true) if s >= 8.0 => Color32::from_rgb(63, 185, 80),
        (s, true) if s >= 7.0 => Color32::from_rgb(210, 153, 34),
        (s, true) if s >= 6.0 => Color32::from_rgb(240, 136, 62),
        _ => kind_color(LineKind::Del, dark),
    }
}

pub fn state_color(state: &str, dark: bool) -> Color32 {
    match state {
        "merged" => Color32::from_rgb(137, 87, 229),
        "closed" => kind_color(LineKind::Del, dark),
        _ => kind_color(LineKind::Add, dark),
    }
}

pub fn severity_color(sev: &str, dark: bool) -> Color32 {
    match sev {
        "critical" | "high" => kind_color(LineKind::Del, dark),
        "medium" => Color32::from_rgb(191, 135, 0),
        _ => kind_color(LineKind::Ctx, dark),
    }
}

// ── Checks / Files 的 JSON 解析（Msg::Panel / PanelLive / NotifDetail 共用）──

pub fn parse_checks_json(json: &str) -> Vec<(String, String)> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap_or("").to_string(),
                c["conclusion"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

pub fn parse_files_json(json: &str) -> Vec<(String, i64, i64)> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap_or("").to_string(),
                f["adds"].as_i64().unwrap_or(0),
                f["dels"].as_i64().unwrap_or(0),
            )
        })
        .collect()
}

// ── diff 视图（行虚拟化）──

const DIFF_LINE_H: f32 = 17.0;

/// 行背景色（GitHub 风格整行底色；深浅主题各一套，深色用半透明叠加）。
fn kind_bg(kind: LineKind, dark: bool) -> Option<Color32> {
    match (kind, dark) {
        (LineKind::Add, false) => Some(Color32::from_rgb(0xda, 0xfb, 0xe1)),
        (LineKind::Del, false) => Some(Color32::from_rgb(0xff, 0xeb, 0xe9)),
        (LineKind::Hunk, false) => Some(Color32::from_rgb(0xdd, 0xf4, 0xff)),
        (LineKind::Add, true) => Some(Color32::from_rgba_unmultiplied(46, 160, 67, 40)),
        (LineKind::Del, true) => Some(Color32::from_rgba_unmultiplied(248, 81, 73, 40)),
        (LineKind::Hunk, true) => Some(Color32::from_rgba_unmultiplied(56, 139, 253, 32)),
        _ => None,
    }
}

pub(crate) fn disclosure_header(ui: &mut Ui, text: RichText, expanded: bool) -> egui::Response {
    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (rect, icon_resp) =
                ui.allocate_exact_size(egui::vec2(12.0, 24.0), egui::Sense::click());
            let c = rect.center();
            let stroke = egui::Stroke::new(1.5, ui.visuals().weak_text_color());
            let points = if expanded {
                [
                    c + egui::vec2(-3.0, -1.5),
                    c + egui::vec2(0.0, 1.5),
                    c + egui::vec2(3.0, -1.5),
                ]
            } else {
                [
                    c + egui::vec2(-1.5, -3.0),
                    c + egui::vec2(1.5, 0.0),
                    c + egui::vec2(-1.5, 3.0),
                ]
            };
            ui.painter().line_segment([points[0], points[1]], stroke);
            ui.painter().line_segment([points[1], points[2]], stroke);
            let text_resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
            icon_resp.union(text_resp)
        })
        .inner;
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// diff 文件列表渲染：折叠头 + 展开文件的行虚拟化绘制（只画可见行，大 diff 不卡）。
/// 长行不自动换行：内容宽度按最长行估算，横向滚动与纵向滚动解耦。
pub fn diff_view(ui: &mut Ui, files: &mut [DiffFileData], lang: &str, id_prefix: &str) {
    if files.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.weak(tr(lang, "diff_empty_title"));
            ui.weak(tr(lang, "diff_empty_hint"));
        });
        return;
    }
    let dark = ui.visuals().dark_mode;
    let mono = FontId::monospace(12.0);
    // 等宽字体单字符宽（'8' 在等宽字体里与其它 ASCII 同宽；CJK 按 2 倍估）
    let char_w = ui.fonts_mut(|f| f.glyph_width(&mono, '8'));
    for (fi, f) in files.iter_mut().enumerate() {
        let adds = f
            .lines
            .iter()
            .filter(|(k, _, _)| *k == LineKind::Add)
            .count();
        let dels = f
            .lines
            .iter()
            .filter(|(k, _, _)| *k == LineKind::Del)
            .count();
        let header = format!("{}  (+{adds}/-{dels})", f.path);
        let resp = disclosure_header(ui, RichText::new(header).monospace().strong(), f.expanded);
        if resp.clicked() {
            f.expanded = !f.expanded;
        }
        if !f.expanded {
            continue;
        }
        let n = f.lines.len();
        // 最长行的显示宽度（ASCII 计 1、其余计 2）→ 横向滚动内容宽
        let max_disp = f
            .lines
            .iter()
            .map(|(_, t, _)| {
                t.chars()
                    .map(|c| if c.is_ascii() { 1 } else { 2 })
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        let content_w = (max_disp as f32 + 8.0) * char_w + 56.0; // 56 = 行号 gutter
        egui::ScrollArea::horizontal()
            .id_salt(format!("{id_prefix}-diff-h-{fi}"))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(content_w.max(ui.available_width()), n as f32 * DIFF_LINE_H),
                    egui::Sense::hover(),
                );
                // 行虚拟化：只画与可见区相交的行
                let clip = ui.clip_rect();
                let first = ((clip.top() - rect.top()) / DIFF_LINE_H).floor().max(0.0) as usize;
                let last =
                    (((clip.bottom() - rect.top()) / DIFF_LINE_H).ceil() as usize + 1).min(n);
                if first >= last {
                    return;
                }
                let painter = ui.painter();
                for (i, (kind, text, no)) in f.lines[first..last].iter().enumerate() {
                    let y = rect.top() + (first + i) as f32 * DIFF_LINE_H;
                    // 行背景（整行宽，含 gutter）：新增淡绿 / 删除淡红 / hunk 淡蓝
                    if let Some(bg) = kind_bg(*kind, dark) {
                        painter.rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(rect.left(), y),
                                egui::vec2(rect.width(), DIFF_LINE_H),
                            ),
                            0.0,
                            bg,
                        );
                    }
                    // 行号 gutter（0 = 无行号）
                    if *no > 0 {
                        painter.text(
                            egui::pos2(rect.left() + 4.0, y),
                            egui::Align2::LEFT_TOP,
                            format!("{:>5}", no),
                            FontId::monospace(11.0),
                            ui.visuals().weak_text_color(),
                        );
                    }
                    painter.text(
                        egui::pos2(rect.left() + 56.0, y),
                        egui::Align2::LEFT_TOP,
                        text,
                        mono.clone(),
                        kind_color(*kind, dark),
                    );
                }
            });
    }
}

// ── 详情面板 ──

pub fn detail_panel(app: &mut EguiApp, ui: &mut Ui) {
    let (selected, panel_open) = {
        let a = app.app.lock_recover();
        (a.selected.clone(), app.panel.is_some())
    };
    if selected.is_none() || !panel_open {
        return;
    }
    egui::Panel::right("detail")
        .resizable(true)
        .default_size(app.panel_width_cur)
        .size_range(300.0..=600.0)
        .show(ui, |ui| {
            // 面板宽度记忆（退出时写回 config.toml）
            app.panel_width_cur = ui.clip_rect().width();
            // 滚动状态按 PR 隔离：跨 PR 复用会把上一个面板的滚动偏移带过来（内容错位）
            let sid = app
                .panel
                .as_ref()
                .map(|p| format!("detail-scroll-{}#{}", p.repo, p.number))
                .unwrap_or_default();
            if app.panel_tab == 5 {
                detail_body(app, ui);
            } else {
                egui::ScrollArea::vertical()
                    .id_salt(sid)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        detail_body(app, ui);
                    });
            }
        });
}

fn detail_body(app: &mut EguiApp, ui: &mut Ui) {
    let lang = app.lang();
    let Some(p) = app.panel.as_ref() else { return };
    let (repo, number) = (p.repo.clone(), p.number);
    let (title, author, state, ci, updated, priority, pinned, on_board) = (
        p.title.clone(),
        p.author.clone(),
        p.state.clone(),
        p.ci.clone(),
        p.updated.clone(),
        p.priority,
        p.pinned,
        p.on_board,
    );
    let review_status = p.review_status.clone();
    let review_progress = p.review_progress.clone();
    let review = p.review.clone();
    let convo = p.convo.clone();
    let checks = parse_checks_json(&p.checks_json);
    let files = parse_files_json(&p.files_json);

    // ── 头部：✕ 单独一行钉右上；标题独占一行按面板宽度换行 ──
    //（ horizontal 里混排 wrap Label 会把 galley 高度算炸——实测面板全空，勿合并）
    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
        if ui.small_button("✕").clicked() {
            app.app.lock_recover().selected = None;
            app.panel = None;
        }
    });
    ui.add(egui::Label::new(RichText::new(&title).size(15.0).strong()).wrap());
    ui.horizontal_wrapped(|ui| {
        // 作者头像（磁盘缓存；miss → 字母降级）
        // horizontal_wrapped：长 repo/作者名行内换行而非溢出（溢出会撑大
        // 内容 ui 的 max_rect，毒化后续所有内容的换行宽度）
        avatar_chip(ui, &author);
        ui.label(&repo);
        // #编号：可点超链接（浏览器打开 PR；行 url 缺失时兜底拼接）
        let link = ui.add(
            egui::Label::new(
                RichText::new(format!("#{number}"))
                    .color(ui.visuals().hyperlink_color)
                    .underline(),
            )
            .sense(egui::Sense::click()),
        );
        if link.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        link.clone().on_hover_text(tr(&lang, "inbox_open_browser"));
        if link.clicked() {
            crate::worker::open_pr_url(&app.app, &repo, &number);
        }
        if !author.is_empty() {
            ui.weak(format!("· {author}"));
        }
        if !updated.is_empty() {
            ui.weak(updated);
        }
    });
    ui.horizontal(|ui| {
        // 状态徽章
        let state_label = match state.as_str() {
            "merged" => tr(&lang, "state_merged"),
            "closed" => tr(&lang, "state_closed"),
            _ => tr(&lang, "state_open"),
        };
        ui.colored_label(state_color(&state, ui.visuals().dark_mode), state_label);
        if !ci.is_empty() {
            let ci_label = match ci.as_str() {
                "success" => tr(&lang, "ci_success"),
                "failure" => tr(&lang, "ci_failure"),
                "pending" => tr(&lang, "ci_pending"),
                other => other,
            };
            ui.colored_label(ci_color(&ci), ci_label);
        }
        ui.hyperlink_to(
            "GitHub ↗",
            format!("https://github.com/{repo}/pull/{number}"),
        );
    });
    // 操作行：置顶 / 看板 / 优先级 / AI 评审 / 复制评审
    ui.horizontal_wrapped(|ui| {
        if ui
            .button(if pinned {
                tr(&lang, "detail_pinned")
            } else {
                tr(&lang, "detail_pin")
            })
            .clicked()
        {
            crate::worker::toggle_field(&app.app, &repo, number, "pinned");
            if let Some(p) = app.panel.as_mut() {
                p.pinned = !pinned;
            }
        }
        if ui
            .button(if on_board {
                tr(&lang, "ctx_remove_board")
            } else {
                tr(&lang, "detail_add_board")
            })
            .clicked()
        {
            crate::worker::toggle_field(&app.app, &repo, number, "on_board");
            if let Some(p) = app.panel.as_mut() {
                p.on_board = !on_board;
            }
        }
        // 优先级：内部存储 1/2/3，显示层 1→P0（最紧急）2→P1 3→P2；0=未设
        let cur = crate::ui_list::priority_badge(priority.into()).0;
        egui::ComboBox::from_id_salt("panel-prio")
            .selected_text(if cur.is_empty() {
                "—".to_string()
            } else {
                cur.to_string()
            })
            .show_ui(ui, |ui| {
                for (v, key) in [
                    (1i64, "ctx_priority_high"),
                    (2, "ctx_priority_medium"),
                    (3, "ctx_priority_low"),
                    (0, "ctx_priority_clear"),
                ] {
                    if ui
                        .selectable_label(priority as i64 == v, tr(&lang, key))
                        .clicked()
                    {
                        crate::worker::local_update(&app.app, &repo, number, "priority", v);
                        if let Some(p) = app.panel.as_mut() {
                            p.priority = v as i32;
                        }
                    }
                }
            });
        let review_btn = match review_status.as_str() {
            "running" => tr(&lang, "review_running"),
            "failed" => tr(&lang, "review_retry"),
            _ => tr(&lang, "btn_review_start"),
        };
        if ui
            .add_enabled(review_status != "running", egui::Button::new(review_btn))
            .on_hover_text(tr(&lang, "review_failed_hint"))
            .clicked()
        {
            let _ = app.cmd_tx.send(Cmd::RunReview {
                repo: repo.clone(),
                number,
            });
        }
        if ui
            .button(RichText::new("⧉").size(12.0))
            .on_hover_text(tr(&lang, "review_copy_tip"))
            .clicked()
        {
            let _ = app.cmd_tx.send(Cmd::CopyReview);
        }
    });
    if review_status == "running" && !review_progress.is_empty() {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.weak(&review_progress);
        });
    }
    ui.separator();

    // ── tab 条 ──
    let tabs = [
        tr(&lang, "tab_importance"),
        tr(&lang, "tab_quality"),
        tr(&lang, "tab_conversation"),
        tr(&lang, "tab_checks"),
        // Files 与 Diff 合并：文件清单（路径+增删计数）可折叠展开看行内容
        tr(&lang, "tab_files"),
        tr(&lang, "tab_kimi_session"),
    ];
    ui.horizontal_wrapped(|ui| {
        for (i, t) in tabs.iter().enumerate() {
            if ui.selectable_label(app.panel_tab == i, *t).clicked() && app.panel_tab != i {
                app.panel_tab = i;
                app.panel_tab_changed(i, ui.ctx());
            }
        }
    });
    ui.separator();

    match app.panel_tab {
        0 => importance_tab(app, ui, &lang),
        1 => quality_tab(app, ui, &review, &lang),
        2 => convo_tab(app, ui, &convo, &lang),
        3 => checks_tab(ui, &checks, &lang),
        // 合并的 Files changed：有解析后 diff 用折叠 diff 视图；
        // 没有（缓存缺失且实时 files 已到）退回纯文件清单
        4 => {
            if !app.diff_files.is_empty() {
                diff_view(ui, &mut app.diff_files, &lang, "detail");
            } else {
                files_tab(ui, &files, &lang);
            }
        }
        5 => kimi_session_tab(app, ui, &lang),
        _ => importance_tab(app, ui, &lang),
    }
}

/// 作者头像小圆片：缓存命中画纹理，否则字母降级。
pub fn avatar_chip(ui: &mut Ui, author: &str) {
    let size = egui::vec2(20.0, 20.0);
    if author.is_empty() {
        return;
    }
    if let Some(tex) = avatars::get_avatar(ui.ctx(), author) {
        ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(size));
    } else {
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 10.0, Color32::from_rgb(90, 90, 120));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            avatar_initial(author),
            FontId::proportional(11.0),
            Color32::WHITE,
        );
    }
}

const IMPORTANCE_GROUPS: &[&str] = &[
    "how_bad",
    "release",
    "who_it_hits",
    "evidence",
    "community",
    "pr_signals",
    "penalty",
];

fn importance_group_color(group: &str, dark: bool) -> Color32 {
    match (group, dark) {
        ("how_bad", true) => Color32::from_rgb(248, 81, 73),
        ("how_bad", false) => Color32::from_rgb(207, 34, 46),
        ("release", true) => Color32::from_rgb(210, 153, 34),
        ("release", false) => Color32::from_rgb(154, 103, 0),
        ("who_it_hits", true) => Color32::from_rgb(88, 166, 255),
        ("who_it_hits", false) => Color32::from_rgb(9, 105, 218),
        ("evidence", true) => Color32::from_rgb(126, 200, 80),
        ("evidence", false) => Color32::from_rgb(63, 140, 40),
        ("community", true) => Color32::from_rgb(219, 131, 209),
        ("community", false) => Color32::from_rgb(163, 63, 163),
        ("pr_signals", true) => Color32::from_rgb(57, 197, 215),
        ("pr_signals", false) => Color32::from_rgb(9, 130, 150),
        _ if dark => Color32::from_gray(120),
        _ => Color32::from_gray(150),
    }
}

fn importance_summary_bar(ui: &mut Ui, importance: &ImportanceProps, lang: &str) {
    let dark = ui.visuals().dark_mode;
    let groups: Vec<(&str, String, i32)> = IMPORTANCE_GROUPS
        .iter()
        .map(|group| {
            let label = importance
                .factors
                .iter()
                .find(|factor| factor.group == *group)
                .map(|factor| factor.label.clone())
                .unwrap_or_else(|| {
                    let key = match *group {
                        "how_bad" => "importance_group_how_bad",
                        "release" => "importance_group_release",
                        "who_it_hits" => "importance_group_who_it_hits",
                        "evidence" => "importance_group_evidence",
                        "community" => "importance_group_community",
                        "pr_signals" => "importance_group_pr_signals",
                        _ => "importance_group_penalty",
                    };
                    tr(lang, key).to_string()
                });
            let points = importance
                .factors
                .iter()
                .filter(|factor| factor.group == *group)
                .map(|factor| factor.points)
                .sum();
            (*group, label, points)
        })
        .collect();
    let penalty = groups
        .iter()
        .map(|(_, _, points)| (-*points).max(0))
        .sum::<i32>();
    let units = 100 + penalty;
    let (track, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(track, 3.0, ui.visuals().extreme_bg_color);
    let mut left = track.left();
    for (group, _, points) in &groups {
        if *points <= 0 {
            continue;
        }
        let width = track.width() * *points as f32 / units as f32;
        let segment = egui::Rect::from_min_size(
            egui::pos2(left, track.top()),
            egui::vec2(width, track.height()),
        );
        ui.painter()
            .rect_filled(segment, 0.0, importance_group_color(group, dark));
        left += width;
    }
    if penalty > 0 {
        let segment = egui::Rect::from_min_size(
            egui::pos2(left, track.top()),
            egui::vec2(track.right() - left, track.height()),
        );
        let painter = ui.painter().with_clip_rect(segment);
        painter.rect_filled(segment, 0.0, importance_group_color("penalty", dark));
        let stroke = egui::Stroke::new(1.0, ui.visuals().extreme_bg_color);
        let mut x = segment.left() - segment.height();
        while x < segment.right() {
            painter.line_segment(
                [
                    egui::pos2(x, segment.bottom()),
                    egui::pos2(x + segment.height(), segment.top()),
                ],
                stroke,
            );
            x += 6.0;
        }
    }
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        for (group, label, points) in &groups {
            ui.label(
                RichText::new("●")
                    .size(10.0)
                    .color(importance_group_color(group, dark)),
            );
            ui.weak(format!("{label} {:+.1}", *points as f32 / 10.0));
            ui.add_space(8.0);
        }
        if let Some(score) = importance.score {
            ui.strong(format!("= {score:.1}"));
        }
    });
    ui.add_space(4.0);
}

fn importance_section(ui: &mut Ui, importance: &ImportanceProps, lang: &str) {
    ui.horizontal(|ui| {
        if let Some(score) = importance.score {
            ui.label(
                RichText::new(format!("{score:.1}"))
                    .size(22.0)
                    .strong()
                    .color(score_color(score as f64, ui.visuals().dark_mode)),
            );
        } else {
            ui.label(RichText::new("—").size(22.0).strong());
        }
        ui.weak(format!("/10 · {}", tr(lang, "importance_title")));
    });
    ui.weak(format!(
        "{} · {}",
        tr(lang, "importance_subtitle"),
        tr(lang, "importance_factor_count").replacen(
            "{0}",
            &importance.factors.len().to_string(),
            1
        )
    ));
    let Some(_) = importance.score else {
        ui.weak(tr(lang, "importance_pending"));
        return;
    };
    importance_summary_bar(ui, importance, lang);
    for group in IMPORTANCE_GROUPS {
        let factors: Vec<&ImportanceFactorProps> = importance
            .factors
            .iter()
            .filter(|factor| factor.group == *group)
            .collect();
        if factors.is_empty() {
            continue;
        }
        let points: i32 = factors.iter().map(|factor| factor.points).sum();
        let max_points = match *group {
            "how_bad" => 30,
            "release" => 10,
            "who_it_hits" => 15,
            "evidence" | "community" => 10,
            "pr_signals" => 25,
            _ => 15,
        };
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.monospace(&factors[0].label);
            let frac = points.abs() as f32 / max_points as f32;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 8.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
            let mut fill = rect;
            fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
            ui.painter().rect_filled(
                fill,
                4.0,
                importance_group_color(group, ui.visuals().dark_mode),
            );
            ui.weak(format!(
                "{:+.1}/{:.1}",
                points as f32 / 10.0,
                max_points as f32 / 10.0
            ));
        });
        for factor in factors {
            ui.horizontal_wrapped(|ui| {
                ui.weak(&factor.evidence);
                ui.weak(format!("{:+.1}", factor.points as f32 / 10.0));
            });
        }
    }
    if !importance.version.is_empty() {
        ui.weak(format!("scoring {}", importance.version));
    }
}

fn importance_tab(app: &mut EguiApp, ui: &mut Ui, lang: &str) {
    let importance = app
        .panel
        .as_ref()
        .map(|panel| panel.importance.clone())
        .unwrap_or_default();
    importance_section(ui, &importance, lang);
}

fn quality_tab(app: &mut EguiApp, ui: &mut Ui, rv: &ReviewProps, lang: &str) {
    let (running, progress) = app
        .panel
        .as_ref()
        .map(|p| (p.review_status == "running", p.review_progress.clone()))
        .unwrap_or_default();
    if running {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.add(egui::Spinner::new());
            ui.label(tr(lang, "review_running"));
            if !progress.is_empty() {
                ui.weak(progress);
            }
            if let Some(started) = app.review_run_started {
                ui.weak(format_elapsed(started));
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
        });
        return;
    }
    if !rv.has {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.weak(tr(lang, "review_empty_title"));
            ui.weak(tr(lang, "review_empty_hint"));
        });
        return;
    }
    // 评分 + 体量行 + 评审时间
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{:.1}", rv.score))
                .size(22.0)
                .strong()
                .color(score_color(rv.score as f64, ui.visuals().dark_mode)),
        );
        ui.weak(format!("/10 · {}", tr(lang, "review_score_label")));
    });
    if !rv.size_line.is_empty() {
        ui.weak(format!(
            "{}{}",
            tr(lang, "review_size_prefix"),
            rv.size_line
        ));
    }
    if !rv.reviewed_at.is_empty() {
        ui.weak(format!(
            "{}{}",
            tr(lang, "review_time_prefix"),
            rv.reviewed_at
        ));
    }
    // 维度分
    if !rv.dimensions.is_empty() {
        ui.add_space(4.0);
        ui.label(tr(lang, "review_dimensions"));
        for (label, score) in &rv.dimensions {
            // 维度名来自 LLM 输出、长度不可控——行内换行防溢出
            ui.horizontal_wrapped(|ui| {
                ui.monospace(format!("{label}"));
                let frac = (*score / 10.0).clamp(0.0, 1.0);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(120.0, 8.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
                let mut fill = rect;
                fill.set_width(rect.width() * frac);
                ui.painter().rect_filled(
                    fill,
                    4.0,
                    score_color(*score as f64, ui.visuals().dark_mode),
                );
                ui.weak(format!("{score}"));
            });
        }
    }
    // 摘要（markdown）
    if !rv.summary.is_empty() {
        ui.add_space(4.0);
        ui.label(tr(lang, "review_summary_title"));
        egui_commonmark::CommonMarkViewer::new().show(ui, &mut app.md_cache, &rv.summary);
    }
    // 风险文件
    if !rv.flagged.is_empty() {
        ui.add_space(4.0);
        ui.label(tr(lang, "review_flagged_title"));
        for f in &rv.flagged {
            ui.weak(format!("• {f}"));
        }
    }
    // 行级评论
    if !rv.comments.is_empty() {
        ui.add_space(4.0);
        ui.label(tr(lang, "review_comments_title"));
        for c in &rv.comments {
            ui.horizontal_wrapped(|ui| {
                let sev_label = tr(
                    lang,
                    match c.severity.as_str() {
                        "critical" => "severity_critical",
                        "high" => "severity_high",
                        "medium" => "severity_medium",
                        "low" => "severity_low",
                        _ => "severity_other",
                    },
                );
                ui.colored_label(
                    severity_color(&c.severity, ui.visuals().dark_mode),
                    sev_label,
                );
                ui.monospace(&c.location);
            });
            ui.label(&c.content);
        }
    }
}

fn elapsed_text(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_elapsed(started: std::time::Instant) -> String {
    elapsed_text(started.elapsed().as_secs())
}

fn kimi_session_tab(app: &mut EguiApp, ui: &mut Ui, lang: &str) {
    app.ensure_kimi_terminal(ui.ctx());
    if !app.kimi_terminal_error.is_empty() {
        let error = app.kimi_terminal_error.clone();
        ui.colored_label(kind_color(LineKind::Del, ui.visuals().dark_mode), error);
    }
    let theme = app.app.lock_recover().cfg.theme.clone();
    if let Some(terminal) = app.kimi_terminal.as_mut() {
        if terminal.is_closed() {
            ui.weak(tr(lang, "kimi_cli_exited"));
        }
        terminal.show(ui, &theme);
    } else if app.kimi_terminal_error.is_empty() {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.weak(tr(lang, "kimi_cli_starting"));
        });
    }
}

fn convo_tab(app: &mut EguiApp, ui: &mut Ui, convo: &[PlainConvoEntry], lang: &str) {
    if convo.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.weak(tr(lang, "convo_empty_title"));
            ui.weak(tr(lang, "convo_empty_hint"));
        });
        return;
    }
    for e in convo {
        ui.horizontal_wrapped(|ui| {
            avatar_chip(ui, &e.author);
            ui.strong(&e.author);
            ui.weak(&e.time);
            let kind_label = match e.kind.as_str() {
                "commit" => "commit",
                "review" => "review",
                _ => "",
            };
            if !kind_label.is_empty() {
                ui.weak(format!("· {kind_label}"));
            }
        });
        if !e.text.is_empty() {
            egui_commonmark::CommonMarkViewer::new().show(ui, &mut app.md_cache, &e.text);
        }
        ui.separator();
    }
}

fn checks_tab(ui: &mut Ui, checks: &[(String, String)], lang: &str) {
    if checks.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.weak(tr(lang, "checks_empty_title"));
            ui.weak(tr(lang, "checks_empty_hint"));
        });
        return;
    }
    for (name, conclusion) in checks {
        // CI 检查名可非常长（如 "ci/circleci: build_linux_wheel_..."）——行内换行防溢出
        ui.horizontal_wrapped(|ui| {
            let (label, color) = match conclusion.as_str() {
                "success" => (tr(lang, "check_success"), ci_color("success")),
                "pending" | "" => (tr(lang, "check_pending"), ci_color("pending")),
                _ => (tr(lang, "check_failure"), ci_color("failure")),
            };
            ui.colored_label(color, label);
            ui.label(name);
        });
    }
}

pub(crate) fn files_tab(ui: &mut Ui, files: &[(String, i64, i64)], lang: &str) {
    if files.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.weak(tr(lang, "files_empty_title"));
            ui.weak(tr(lang, "files_empty_hint"));
        });
        return;
    }
    for (path, adds, dels) in files {
        // 文件路径可非常长——行内换行防溢出
        ui.horizontal_wrapped(|ui| {
            ui.monospace(path);
            ui.weak(format!("+{adds}/-{dels}"));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> (egui::Rect, bool) {
        let mut rect = egui::Rect::NOTHING;
        let mut clicked = false;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 300.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let resp = disclosure_header(ui, RichText::new("To Review (11)"), false);
                rect = resp.rect;
                clicked = resp.clicked();
            },
        );
        output.drop_without_applying_deltas();
        (rect, clicked)
    }

    #[test]
    fn score_colors_have_five_distinct_tiers() {
        for dark in [false, true] {
            let colors = [5.9, 6.0, 7.0, 8.0, 9.0].map(|score| score_color(score, dark));
            for pair in colors.windows(2) {
                assert_ne!(pair[0], pair[1], "相邻评分档位必须有不同颜色");
            }
            for score in [0.0, 5.0, 5.9] {
                assert_eq!(score_color(score, dark), kind_color(LineKind::Del, dark));
            }
            assert_eq!(score_color(6.9, dark), score_color(6.0, dark));
            assert_eq!(score_color(7.9, dark), score_color(7.0, dark));
            assert_eq!(score_color(8.9, dark), score_color(8.0, dark));
        }
    }

    #[test]
    fn importance_groups_have_distinct_colors() {
        for dark in [false, true] {
            let colors: Vec<Color32> = IMPORTANCE_GROUPS
                .iter()
                .map(|group| importance_group_color(group, dark))
                .collect();
            for (index, color) in colors.iter().enumerate() {
                assert!(colors[..index].iter().all(|other| other != color));
            }
        }
    }

    #[test]
    fn disclosure_header_clickable_on_icon_and_text() {
        for click_icon in [true, false] {
            let ctx = egui::Context::default();
            let (rect, _) = header_frame(&ctx, Vec::new());
            let pos = if click_icon {
                rect.left_center() + egui::vec2(6.0, 0.0)
            } else {
                rect.right_center() - egui::vec2(4.0, 0.0)
            };
            let _ = header_frame(&ctx, vec![egui::Event::PointerMoved(pos)]);
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let (_, pressed) = header_frame(&ctx, vec![button(true)]);
            let (_, released) = header_frame(&ctx, vec![button(false)]);
            assert!(pressed || released, "click_icon={click_icon}");
        }
    }

    #[test]
    fn elapsed_text_uses_compact_clock_format() {
        assert_eq!(elapsed_text(0), "00:00");
        assert_eq!(elapsed_text(65), "01:05");
        assert_eq!(elapsed_text(3599), "59:59");
        assert_eq!(elapsed_text(3661), "01:01:01");
    }
}
