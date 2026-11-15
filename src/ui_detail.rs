//! 详情面板：头部 + 评审/会话/Checks/Files/Deep Review tabs + 行虚拟化 diff 渲染。

use egui::{Color32, FontId, RichText, Ui};

use pr_desk_rs::deep_review::{DeepFindingData, DeepQaMessage, finding_level};
use pr_desk_rs::diffview::LineKind;
use pr_desk_rs::i18n::{tr, trf};

use crate::state::*;
use crate::{avatars, EguiApp};

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
    let resp = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (rect, icon_resp) = ui.allocate_exact_size(egui::vec2(12.0, 24.0), egui::Sense::click());
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
    }).inner;
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
        let adds = f.lines.iter().filter(|(k, _, _)| *k == LineKind::Add).count();
        let dels = f.lines.iter().filter(|(k, _, _)| *k == LineKind::Del).count();
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
            .map(|(_, t, _)| t.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum::<usize>())
            .max()
            .unwrap_or(0);
        let content_w = (max_disp as f32 + 8.0) * char_w + 56.0; // 56 = 行号 gutter
        egui::ScrollArea::horizontal()
            .id_salt(format!("{id_prefix}-diff-h-{fi}"))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(content_w.max(ui.available_width()), n as f32 * DIFF_LINE_H),
                    egui::Sense::hover(),
                );
                // 行虚拟化：只画与可见区相交的行
                let clip = ui.clip_rect();
                let first = ((clip.top() - rect.top()) / DIFF_LINE_H).floor().max(0.0) as usize;
                let last = (((clip.bottom() - rect.top()) / DIFF_LINE_H).ceil() as usize + 1).min(n);
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
            let sid = app.panel.as_ref().map(|p| format!("detail-scroll-{}#{}", p.repo, p.number)).unwrap_or_default();
            egui::ScrollArea::vertical()
                .id_salt(sid)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    detail_body(app, ui);
                });
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
        ui.hyperlink_to("GitHub ↗", format!("https://github.com/{repo}/pull/{number}"));
    });
    // 操作行：置顶 / 看板 / 优先级 / AI 评审 / 复制评审
    ui.horizontal_wrapped(|ui| {
        if ui
            .button(if pinned { tr(&lang, "detail_pinned") } else { tr(&lang, "detail_pin") })
            .clicked()
        {
            crate::worker::toggle_field(&app.app, &repo, number, "pinned");
            if let Some(p) = app.panel.as_mut() {
                p.pinned = !pinned;
            }
        }
        if ui
            .button(if on_board { tr(&lang, "ctx_remove_board") } else { tr(&lang, "detail_add_board") })
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
            .selected_text(if cur.is_empty() { "—".to_string() } else { cur.to_string() })
            .show_ui(ui, |ui| {
                for (v, key) in [
                    (1i64, "ctx_priority_high"),
                    (2, "ctx_priority_medium"),
                    (3, "ctx_priority_low"),
                    (0, "ctx_priority_clear"),
                ] {
                    if ui.selectable_label(priority as i64 == v, tr(&lang, key)).clicked() {
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
            let _ = app.cmd_tx.send(Cmd::RunReview { repo: repo.clone(), number });
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
        tr(&lang, "tab_review"),
        tr(&lang, "tab_conversation"),
        tr(&lang, "tab_checks"),
        // Files 与 Diff 合并：文件清单（路径+增删计数）可折叠展开看行内容
        tr(&lang, "tab_files"),
        tr(&lang, "tab_deep_review"),
    ];
    ui.horizontal_wrapped(|ui| {
        for (i, t) in tabs.iter().enumerate() {
            if ui.selectable_label(app.panel_tab == i, *t).clicked() && app.panel_tab != i {
                app.panel_tab = i;
                app.panel_tab_changed(i);
            }
        }
    });
    ui.separator();

    match app.panel_tab {
        0 => review_tab(app, ui, &review, &lang),
        1 => convo_tab(app, ui, &convo, &lang),
        2 => checks_tab(ui, &checks, &lang),
        // 合并的 Files changed：有解析后 diff 用折叠 diff 视图；
        // 没有（缓存缺失且实时 files 已到）退回纯文件清单
        3 => {
            if !app.diff_files.is_empty() {
                diff_view(ui, &mut app.diff_files, &lang, "detail");
            } else {
                files_tab(ui, &files, &lang);
            }
        }
        4 => deep_review_tab(app, ui, &lang),
        _ => review_tab(app, ui, &review, &lang),
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
        ui.painter().circle_filled(rect.center(), 10.0, Color32::from_rgb(90, 90, 120));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            avatar_initial(author),
            FontId::proportional(11.0),
            Color32::WHITE,
        );
    }
}

fn review_tab(app: &mut EguiApp, ui: &mut Ui, rv: &ReviewProps, lang: &str) {
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
        ui.weak(format!("{}{}", tr(lang, "review_size_prefix"), rv.size_line));
    }
    if !rv.reviewed_at.is_empty() {
        ui.weak(format!("{}{}", tr(lang, "review_time_prefix"), rv.reviewed_at));
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
                let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 8.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
                let mut fill = rect;
                fill.set_width(rect.width() * frac);
                ui.painter().rect_filled(fill, 4.0, score_color(*score as f64, ui.visuals().dark_mode));
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
                let sev_label = tr(lang, match c.severity.as_str() {
                    "critical" => "severity_critical",
                    "high" => "severity_high",
                    "medium" => "severity_medium",
                    "low" => "severity_low",
                    _ => "severity_other",
                });
                ui.colored_label(severity_color(&c.severity, ui.visuals().dark_mode), sev_label);
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

fn deep_review_tab(app: &mut EguiApp, ui: &mut Ui, lang: &str) {
    let (status, progress, error, current_head, workspace) = {
        let Some(panel) = app.panel.as_ref() else { return };
        (
            panel.deep_status.clone(),
            panel.deep_progress.clone(),
            panel.deep_error.clone(),
            panel.head_sha.clone(),
            panel.deep.clone(),
        )
    };
    let running = status == "running";
    let cancelling = status == "cancelling";
    let active_run = running || cancelling;
    ui.horizontal_wrapped(|ui| {
        if active_run {
            if ui
                .add_enabled(running, egui::Button::new(tr(lang, "deep_cancel")))
                .clicked()
            {
                app.cancel_deep_review();
            }
        } else if ui.button(tr(lang, "deep_run")).clicked() {
            app.start_deep_review();
        }
        if !active_run && workspace.is_some() {
            if ui.button(tr(lang, "deep_refresh")).clicked() {
                app.refresh_deep_findings();
            }
        }
        if workspace.as_ref().is_some_and(|w| w.findings.iter().any(|f| !f.discarded)) {
            if ui.button(tr(lang, "deep_copy_all")).clicked() {
                app.copy_all_deep_findings();
            }
        }
    });
    ui.weak(tr(lang, "deep_read_only"));
    if active_run {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Spinner::new());
            let progress = if progress.is_empty() {
                tr(lang, "deep_starting").into()
            } else {
                progress.clone()
            };
            ui.weak(format!(
                "{} · {}",
                progress,
                app.deep_run_started.map(format_elapsed).unwrap_or_default()
            ));
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    } else if !progress.is_empty() {
        ui.weak(&progress);
    }
    if !error.is_empty() {
        ui.colored_label(kind_color(LineKind::Del, ui.visuals().dark_mode), &error);
    }
    let Some(workspace) = workspace else {
        if !active_run {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.weak(tr(lang, "deep_empty_title"));
                ui.weak(tr(lang, "deep_empty_hint"));
            });
        }
        return;
    };

    let short_head = workspace.head_sha.chars().take(12).collect::<String>();
    ui.weak(trf(lang, "deep_based_on", &[&short_head]));
    if !current_head.is_empty() && current_head != workspace.head_sha {
        ui.colored_label(
            Color32::from_rgb(191, 135, 0),
            trf(lang, "deep_stale", &[&short_head]),
        );
    }
    let active = workspace.findings.iter().filter(|f| !f.discarded).count();
    let discarded = workspace.findings.len() - active;
    ui.strong(trf(
        lang,
        "deep_suggested_count",
        &[&active.to_string(), &discarded.to_string()],
    ));
    pr_agent_card(app, ui, &workspace, lang);
    if workspace.findings.is_empty() {
        ui.add_space(12.0);
        ui.weak(tr(lang, "deep_no_findings"));
        return;
    }
    ui.add_space(6.0);
    for finding in workspace.findings.iter().filter(|f| !f.discarded).cloned() {
        deep_finding_card(app, ui, &finding, lang);
    }
    let resolved = workspace
        .findings
        .iter()
        .filter(|f| f.discarded && !f.resolution.is_empty() && f.resolution != "manual")
        .count();
    if resolved > 0 {
        egui::CollapsingHeader::new(format!("{} · {resolved}", tr(lang, "deep_resolved")))
            .id_salt("deep-resolved-findings")
            .default_open(false)
            .show(ui, |ui| {
                for finding in workspace
                    .findings
                    .iter()
                    .filter(|f| f.discarded && !f.resolution.is_empty() && f.resolution != "manual")
                    .cloned()
                {
                    deep_finding_card(app, ui, &finding, lang);
                }
            });
    }
    if discarded > resolved {
        egui::CollapsingHeader::new(format!("{} · {}", tr(lang, "deep_discarded"), discarded - resolved))
            .id_salt("deep-manual-discarded-findings")
            .default_open(false)
            .show(ui, |ui| {
                for finding in workspace
                    .findings
                    .iter()
                    .filter(|f| f.discarded && matches!(f.resolution.as_str(), "" | "manual"))
                    .cloned()
                {
                    deep_finding_card(app, ui, &finding, lang);
                }
            });
    }
}

fn qa_message(
    app: &mut EguiApp,
    ui: &mut Ui,
    message: &DeepQaMessage,
    lang: &str,
    viewer_id: egui::Id,
) {
    let user = message.role == "user";
    let color = if user {
        Color32::from_rgb(59, 130, 246)
    } else {
        Color32::from_rgb(16, 145, 110)
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 13.0, color);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if user { "U" } else { "AI" },
            FontId::proportional(if user { 11.0 } else { 9.0 }),
            Color32::WHITE,
        );
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.strong(tr(lang, if user { "deep_user" } else { "deep_assistant" }));
            if user {
                ui.add(egui::Label::new(&message.content).wrap());
            } else {
                egui_commonmark::CommonMarkViewer::new().show_with_id(
                    viewer_id,
                    ui,
                    &mut app.md_cache,
                    &message.content,
                );
            }
        });
    });
    ui.add_space(6.0);
}

fn enter_submit(ui: &mut Ui, id: egui::Id) -> bool {
    let focused = ui.memory(|memory| memory.focused() == Some(id));
    focused && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
}

fn pr_agent_card(
    app: &mut EguiApp,
    ui: &mut Ui,
    workspace: &pr_desk_rs::deep_review::DeepWorkspaceData,
    lang: &str,
) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.strong(tr(lang, "deep_ask_pr"));
        ui.weak(tr(lang, "deep_ask_pr_hint"));
        if !workspace.qa.is_empty() {
            egui::CollapsingHeader::new(format!("{} · {}", tr(lang, "deep_agent_work"), workspace.qa.len()))
                .id_salt("deep-pr-agent")
                .default_open(true)
                .show(ui, |ui| {
                    for (index, message) in workspace.qa.iter().enumerate() {
                        qa_message(
                            app,
                            ui,
                            message,
                            lang,
                            ui.make_persistent_id(("deep-pr-qa", index)),
                        );
                    }
                });
        }
        let pending = app.deep_pr_pending.is_some();
        let input_id = egui::Id::unique("deep-pr-question");
        let enter_send = enter_submit(ui, input_id);
        ui.add_enabled(
            !pending,
            egui::TextEdit::multiline(&mut app.deep_pr_question)
                .id(input_id)
                .hint_text(tr(lang, "deep_pr_question_placeholder"))
                .desired_width(f32::INFINITY)
                .desired_rows(2),
        );
        ui.weak(tr(lang, "deep_enter_hint"));
        if pending {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.weak(tr(lang, "deep_pr_pending"));
            });
        }
        if !app.deep_pr_error.is_empty() {
            let error = app.deep_pr_error.clone();
            ui.colored_label(kind_color(LineKind::Del, ui.visuals().dark_mode), error);
        }
        let can_ask = !pending && !app.deep_pr_question.trim().is_empty();
        if (ui
            .add_enabled(can_ask, egui::Button::new(tr(lang, "deep_ask_pr")))
            .clicked()
            || enter_send)
            && can_ask
        {
            app.ask_deep_pr();
        }
    });
    ui.add_space(6.0);
}

fn readonly_code(ui: &mut Ui, text: &str, rows: usize) {
    let mut text = text.to_string();
    ui.add(
        egui::TextEdit::multiline(&mut text)
            .desired_width(f32::INFINITY)
            .desired_rows(rows)
            .interactive(false),
    );
}

fn readonly_diff(ui: &mut Ui, text: &str, expanded: bool) {
    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(4.0)
        .inner_margin(6.0)
        .show(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt(("deep-finding-diff", text))
                .max_height(if expanded { f32::INFINITY } else { 560.0 })
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in text.lines() {
                        let kind = pr_desk_rs::diffview::classify_line(line);
                        let color = kind_color(kind, ui.visuals().dark_mode);
                        ui.add(
                            egui::Label::new(
                                RichText::new(if line.is_empty() { " " } else { line })
                                    .monospace()
                                    .color(color),
                            )
                            .selectable(true),
                        );
                    }
                });
        });
}

fn deep_finding_card(app: &mut EguiApp, ui: &mut Ui, finding: &DeepFindingData, lang: &str) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                severity_color(&finding.severity, ui.visuals().dark_mode),
                finding_level(&finding.severity),
            );
            if !finding.category.is_empty() {
                ui.weak(&finding.category);
            }
            ui.monospace(finding.location());
        });
        ui.add(egui::Label::new(&finding.content).wrap());
        if finding.discarded {
            if !finding.resolution.is_empty() && finding.resolution != "manual" {
                ui.weak(tr(
                    lang,
                    if finding.resolution == "superseded" {
                        "deep_superseded"
                    } else {
                        "deep_not_found"
                    },
                ));
            }
            if ui.button(tr(lang, "deep_restore")).clicked() {
                app.discard_deep_finding(finding.id);
            }
            return;
        }

        egui::CollapsingHeader::new(tr(lang, "deep_evidence"))
            .id_salt(("deep-evidence", finding.id))
            .default_open(false)
            .show(ui, |ui| {
                if !finding.existing_code.trim().is_empty() {
                    ui.weak(tr(lang, "deep_existing_code"));
                    readonly_code(ui, &finding.existing_code, 3);
                }
                if !finding.suggestion_code.trim().is_empty() {
                    ui.weak(tr(lang, "deep_suggestion_code"));
                    readonly_code(ui, &finding.suggestion_code, 3);
                }
                if !finding.diff_chunk.trim().is_empty() {
                    let expanded = app.deep_diff_expanded.contains(&finding.id);
                    ui.horizontal(|ui| {
                        ui.weak(tr(lang, "deep_diff"));
                        let label = tr(lang, if expanded { "deep_collapse" } else { "deep_expand" });
                        if ui.small_button(label).clicked() {
                            if expanded {
                                app.deep_diff_expanded.remove(&finding.id);
                            } else {
                                app.deep_diff_expanded.insert(finding.id);
                            }
                        }
                    });
                    readonly_diff(ui, &finding.diff_chunk, expanded);
                }
            });

        let Some(mut editor) = app.deep_editors.remove(&finding.id) else {
            return;
        };
        let pending = editor.pending.map(|(action, _)| action);
        ui.add_enabled(
            pending.is_none(),
            egui::TextEdit::multiline(&mut editor.draft)
                .id_salt(("deep-draft", finding.id))
                .desired_width(f32::INFINITY)
                .desired_rows(5),
        );
        let mut save = false;
        let mut copy = false;
        let mut discard = false;
        let mut rewrite = None;
        ui.horizontal_wrapped(|ui| {
            save = ui.add_enabled(pending.is_none(), egui::Button::new(tr(lang, "deep_save"))).clicked();
            copy = ui.button(tr(lang, "deep_copy")).clicked();
            discard = ui
                .add_enabled(pending.is_none(), egui::Button::new(tr(lang, "deep_discard")))
                .clicked();
        });
        ui.horizontal_wrapped(|ui| {
            ui.weak(tr(lang, "deep_rewrite"));
            for (style, key) in [
                ("shorter", "deep_shorter"),
                ("question", "deep_question_style"),
                ("direct", "deep_direct"),
            ] {
                if ui
                    .add_enabled(pending.is_none(), egui::Button::new(tr(lang, key)))
                    .clicked()
                {
                    rewrite = Some(style);
                }
            }
        });
        if let Some(action) = pending {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.weak(tr(
                    lang,
                    if action == DeepAction::Ask {
                        "deep_pending_ask"
                    } else {
                        "deep_pending_rewrite"
                    },
                ));
            });
        }
        if !editor.error.is_empty() {
            ui.colored_label(kind_color(LineKind::Del, ui.visuals().dark_mode), &editor.error);
        }

        let mut enter_ask = false;
        egui::CollapsingHeader::new(format!("{} · {}", tr(lang, "deep_agent_work"), finding.qa.len()))
            .id_salt(("deep-finding-agent", finding.id))
            .default_open(!finding.qa.is_empty())
            .show(ui, |ui| {
                for (index, message) in finding.qa.iter().enumerate() {
                    qa_message(
                        app,
                        ui,
                        message,
                        lang,
                        ui.make_persistent_id(("deep-finding-qa", finding.id, index)),
                    );
                }
                let input_id = egui::Id::unique(("deep-question", finding.id));
                enter_ask = enter_submit(ui, input_id);
                ui.add(
                    egui::TextEdit::multiline(&mut editor.question)
                        .id(input_id)
                        .hint_text(tr(lang, "deep_ask_placeholder"))
                        .desired_width(f32::INFINITY)
                        .desired_rows(2),
                );
                ui.weak(tr(lang, "deep_enter_hint"));
            });
        let can_ask = pending.is_none() && !editor.question.trim().is_empty();
        let ask = (ui
            .add_enabled(can_ask, egui::Button::new(tr(lang, "deep_ask")))
            .on_hover_text(tr(lang, "deep_ask_about"))
            .clicked()
            || enter_ask)
            && can_ask;
        app.deep_editors.insert(finding.id, editor);

        if save {
            app.save_deep_draft(finding.id);
        } else if let Some(style) = rewrite {
            app.rewrite_deep_finding(finding.id, style);
        } else if ask {
            app.ask_deep_finding(finding.id);
        }
        if copy {
            app.copy_deep_finding(finding.id);
        }
        if discard {
            app.discard_deep_finding(finding.id);
        }
    });
    ui.add_space(6.0);
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
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
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
