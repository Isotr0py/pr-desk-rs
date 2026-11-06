//! PR Desk 桌面端：egui/eframe UI 骨架（替换原 Slint UI）。
//! 纯逻辑在 lib（pr_desk_rs）；本文件只做 UI 装配与后台 worker 胶水。
//!
//! 骨架范围：PR 列表（分组+搜索）+ 详情侧栏（评审+diff）+ 后台刷新 + 主题切换。
//! TODO(egui): Inbox、设置页、多选批量操作、评审触发、托盘、头像、同步模式来源过滤、
//!             窗口/面板几何记忆、列排序表头——均未从旧 Slint 版移植。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, FontFamily, FontId, RichText};
use serde_json::Value;

use pr_desk_rs::config::Config;
use pr_desk_rs::db::Db;
use pr_desk_rs::diffview::{self, LineKind};
use pr_desk_rs::github;
use pr_desk_rs::i18n::{tr, trf};

// ── 后台命令 / 回送消息 ──

enum Cmd {
    Refresh,
    FetchDiff { repo: String, number: i64 },
}

struct DiffLine {
    kind: LineKind,
    text: String,
    no: i32,
}

struct DiffFile {
    path: String,
    lines: Vec<DiffLine>,
}

enum Msg {
    Status(String),
    Rows(Vec<Value>),
    RefreshDone(usize),
    DiffParsed { repo: String, number: i64, files: Vec<DiffFile> },
}

/// 解析整段 diff 为按文件分组、带行号的行序列（diffview 纯逻辑）。
fn parse_diff(diff: &str) -> Vec<DiffFile> {
    diffview::split_files(diff)
        .into_iter()
        .map(|(path, text)| DiffFile {
            path,
            lines: diffview::numbered_lines(&text)
                .into_iter()
                .map(|(kind, text, no)| DiffLine { kind, text, no })
                .collect(),
        })
        .collect()
}

/// 档案名 → db 文件路径（与旧版一致：local → pr-desk.db）。
fn profile_db_path(data_dir: &std::path::Path, name: &str) -> PathBuf {
    if name == "local" || !pr_desk_rs::config::valid_profile_name(name) {
        data_dir.join("pr-desk.db")
    } else {
        data_dir.join("profiles").join(format!("{name}.db"))
    }
}

// ── 行展示辅助（移植自已删除的 model.rs 的纯逻辑部分）──

/// 解析 updated_at：兼容 db::now() 的本地 "YYYY-MM-DDTHH:MM:SS" 与 RFC3339。
fn parse_ts(s: &str) -> Option<chrono::NaiveDateTime> {
    use chrono::NaiveDateTime;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t);
        }
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&chrono::Local).naive_local())
}

/// 相对时间格式："27m" / "2h" / "3d" / "2mo" / "1y"。
fn rel_time(updated_at: &str) -> String {
    let Some(ts) = parse_ts(updated_at) else {
        return String::new();
    };
    let mins = (chrono::Local::now().naive_local() - ts).num_minutes().max(0);
    if mins < 60 {
        format!("{mins}m")
    } else if mins < 24 * 60 {
        format!("{}h", mins / 60)
    } else if mins < 30 * 24 * 60 {
        format!("{}d", mins / (24 * 60))
    } else if mins < 365 * 24 * 60 {
        format!("{}mo", mins / (30 * 24 * 60))
    } else {
        format!("{}y", mins / (365 * 24 * 60))
    }
}

/// 大小写不敏感子串匹配：title / number（query 可带 # 前缀）/ author / repo。
fn row_matches(r: &Value, query: &str) -> bool {
    let q = query.trim().trim_start_matches('#').to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hit = |s: &str| s.to_lowercase().contains(&q);
    hit(r["title"].as_str().unwrap_or(""))
        || hit(r["author"].as_str().unwrap_or(""))
        || hit(r["repo"].as_str().unwrap_or(""))
        || r["number"].as_i64().is_some_and(|n| n.to_string().contains(&q))
}

// ── 后台 worker：独立线程 + 自己的 tokio runtime 与 Db 连接 ──

fn spawn_worker(
    db_path: PathBuf,
    gh: Option<github::GitHub>,
    msg_tx: mpsc::Sender<Msg>,
    ctx: egui::Context,
) -> mpsc::Sender<Cmd> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("tokio runtime 创建失败: {e}");
                return;
            }
        };
        let send = |m: Msg| {
            let _ = msg_tx.send(m);
            ctx.request_repaint();
        };
        let db = match Db::open(&db_path) {
            Ok(db) => db,
            Err(e) => {
                send(Msg::Status(format!("数据库打开失败: {e}")));
                return;
            }
        };
        for cmd in cmd_rx {
            match cmd {
                Cmd::Refresh => refresh(&rt, &db, &gh, &send),
                Cmd::FetchDiff { repo, number } => {
                    fetch_diff(&rt, &db, &gh, &send, &repo, number)
                }
            }
        }
    });
    cmd_tx
}

fn refresh(
    rt: &tokio::runtime::Runtime,
    db: &Db,
    gh: &Option<github::GitHub>,
    send: &dyn Fn(Msg),
) {
    // 先把本地库现有内容上屏（不等网络）
    match db.list_prs(&[]) {
        Ok(rows) => send(Msg::Rows(rows)),
        Err(e) => send(Msg::Status(format!("列表读取失败: {e}"))),
    }
    let Some(gh) = gh else {
        send(Msg::RefreshDone(0));
        return;
    };
    // 刷新在库 PR 的 GitHub 状态（merged/closed 落库后分组自动移出待评审区）；
    // 本地库手动策展，不从 GitHub 拉新列表。TODO(egui): 通知/组织/同步模式。
    let keys = db.list_pr_keys().unwrap_or_default();
    if !keys.is_empty() {
        let updates = rt.block_on(async {
            let mut set = tokio::task::JoinSet::new();
            for (repo, number) in keys {
                let g = gh.clone();
                set.spawn(async move {
                    let meta = g.fetch_pr_meta(&repo, number).await;
                    (repo, number, meta)
                });
            }
            let mut out = Vec::new();
            while let Some(Ok((repo, number, res))) = set.join_next().await {
                match res {
                    Ok(meta) => {
                        let state = if meta["merged_at"].is_null() {
                            meta["state"].as_str().unwrap_or("open").to_string()
                        } else {
                            "merged".to_string()
                        };
                        if state == "merged" || state == "closed" {
                            out.push((repo, number, state));
                        }
                    }
                    Err(e) => tracing::debug!("刷新 PR 状态失败 {repo} #{number}: {e}"),
                }
            }
            out
        });
        for (repo, number, state) in updates {
            let _ = db.local_pr_update(&repo, number, "status", &Value::String(state));
        }
    }
    let (rows, open_count) = match db.list_prs(&[]) {
        Ok(rows) => {
            let open = rows
                .iter()
                .filter(|r| !matches!(r["status"].as_str(), Some("merged") | Some("closed")))
                .count();
            (rows, open)
        }
        Err(e) => {
            send(Msg::Status(format!("列表读取失败: {e}")));
            (Vec::new(), 0)
        }
    };
    send(Msg::Rows(rows));
    send(Msg::RefreshDone(open_count));
}

fn fetch_diff(
    rt: &tokio::runtime::Runtime,
    db: &Db,
    gh: &Option<github::GitHub>,
    send: &dyn Fn(Msg),
    repo: &str,
    number: i64,
) {
    let Some(gh) = gh else { return };
    let lang = "zh"; // worker 侧无界面语言上下文；错误文案以中文兜底
    match rt.block_on(gh.fetch_diff(repo, number)) {
        Ok(diff) => {
            if let Err(e) = db.put_diff(repo, number, &diff) {
                tracing::warn!("diff 缓存写入失败 {repo} #{number}: {e}");
            }
            send(Msg::DiffParsed { repo: repo.into(), number, files: parse_diff(&diff) });
        }
        Err(e) => send(Msg::Status(trf(lang, "diff_fetch_failed", &[&e.to_string()]))),
    }
}

// ── egui 应用 ──

struct DetailState {
    repo: String,
    number: i64,
    review: Option<Value>,
    diff: Option<Vec<DiffFile>>,
    diff_loading: bool,
}

struct PrDeskApp {
    cfg: Config,
    cfg_path: PathBuf,
    db: Db,
    rows: Vec<Value>,
    filter: String,
    collapsed: HashSet<String>,
    selected: Option<(String, i64)>,
    detail: Option<DetailState>,
    status_msg: String,
    refreshing: bool,
    cmd_tx: mpsc::Sender<Cmd>,
    msg_rx: mpsc::Receiver<Msg>,
    rate_limit: Option<Arc<Mutex<Option<u64>>>>,
    last_poll: Instant,
    md_cache: egui_commonmark::CommonMarkCache,
}

impl PrDeskApp {
    fn lang(&self) -> &str {
        &self.cfg.language
    }

    fn open_detail(&mut self, repo: &str, number: i64) {
        let review = self.db.get_review(repo, number).unwrap_or_else(|e| {
            tracing::warn!("评审读取失败 {repo} #{number}: {e}");
            None
        });
        let diff_cached = self.db.get_diff(repo, number).unwrap_or_else(|e| {
            tracing::warn!("diff 缓存读取失败 {repo} #{number}: {e}");
            None
        });
        let (diff, diff_loading) = match diff_cached {
            Some(text) => (Some(parse_diff(&text)), false),
            None => {
                let _ = self.cmd_tx.send(Cmd::FetchDiff { repo: repo.into(), number });
                (None, true)
            }
        };
        self.detail = Some(DetailState {
            repo: repo.into(),
            number,
            review,
            diff,
            diff_loading,
        });
    }

    fn handle_msgs(&mut self) {
        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                Msg::Status(s) => self.status_msg = s,
                Msg::Rows(rows) => self.rows = rows,
                Msg::RefreshDone(open) => {
                    self.refreshing = false;
                    if open > 0 {
                        self.status_msg =
                            trf(self.lang(), "gh_refresh_done", &[&open.to_string()]);
                    }
                }
                Msg::DiffParsed { repo, number, files } => {
                    if let Some(d) = self.detail.as_mut() {
                        if d.repo == repo && d.number == number
                            && self.selected.as_ref().is_some_and(|s| s.0 == repo && s.1 == number)
                        {
                            d.diff = Some(files);
                            d.diff_loading = false;
                        }
                    }
                }
            }
        }
    }

    /// 轮询定时：到点且空闲时发刷新；request_repaint_after 保证零 CPU 空闲。
    fn poll_tick(&mut self, ctx: &egui::Context) {
        let interval = Duration::from_secs(self.cfg.poll_interval_min.max(1) * 60);
        let elapsed = self.last_poll.elapsed();
        if elapsed >= interval {
            self.last_poll = Instant::now();
            if !self.refreshing {
                self.refreshing = true;
                let _ = self.cmd_tx.send(Cmd::Refresh);
            }
        } else {
            ctx.request_repaint_after((interval - elapsed).min(Duration::from_secs(5)));
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let lang = self.lang().to_string();
                if ui.button(tr(&lang, "tb_refresh")).clicked() && !self.refreshing {
                    self.refreshing = true;
                    self.last_poll = Instant::now();
                    let _ = self.cmd_tx.send(Cmd::Refresh);
                }
                if self.refreshing {
                    ui.add(egui::Spinner::new());
                }
                ui.separator();
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text(tr(&lang, "tb_search_hint"))
                        .desired_width(260.0),
                );
                ui.separator();
                let dark = self.cfg.theme == "dark";
                let label = if dark { tr(&lang, "tb_theme_light") } else { tr(&lang, "tb_theme_dark") };
                if ui.button(label).clicked() {
                    self.cfg.theme = if dark { "light".into() } else { "dark".into() };
                    apply_theme(ui.ctx(), &self.cfg.theme);
                    if let Err(e) = self.cfg.save(&self.cfg_path) {
                        self.status_msg =
                            trf(&lang, "cfg_save_failed", &[&e.to_string()]);
                    }
                }
            });
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let lang = self.lang();
                ui.label(&self.status_msg);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(trf(lang, "sb_pr_count", &[&self.rows.len().to_string()]));
                    if let Some(rl) = &self.rate_limit {
                        if let Some(q) = *rl.lock().unwrap_or_else(|e| e.into_inner()) {
                            ui.label(trf(lang, "sb_api_quota", &[&q.to_string()]));
                        }
                    }
                });
            });
        });
    }

    fn pr_list(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let sections =
                pr_desk_rs::build_sections(&self.rows, self.lang(), "upd", false);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for sec in &sections {
                        let idxs: Vec<usize> = sec
                            .idxs
                            .iter()
                            .copied()
                            .filter(|&i| row_matches(&self.rows[i], &self.filter))
                            .collect();
                        if idxs.is_empty() {
                            continue;
                        }
                        let collapsed = self.collapsed.contains(sec.key);
                        // ▼/▶ 在微软雅黑中有字形；▸/▾（U+25B8/25BE）无（会豆腐块）
                        let arrow = if collapsed { "▶" } else { "▼" };
                        let header = format!("{arrow} {} ({})", sec.title, idxs.len());
                        let resp = ui.add(
                            egui::Label::new(RichText::new(header).strong())
                                .sense(egui::Sense::click()),
                        );
                        if resp.clicked() {
                            if collapsed {
                                self.collapsed.remove(sec.key);
                            } else {
                                self.collapsed.insert(sec.key.to_string());
                            }
                        }
                        if collapsed {
                            continue;
                        }
                        for i in idxs {
                            self.pr_row(ui, &self.rows[i].clone());
                        }
                    }
                });
        });
    }

    fn pr_row(&mut self, ui: &mut egui::Ui, row: &Value) {
        let repo = row["repo"].as_str().unwrap_or("").to_string();
        let number = row["number"].as_i64().unwrap_or(0);
        let key = (repo.clone(), number);
        let selected = self.selected.as_ref() == Some(&key);

        let title = row["title"].as_str().unwrap_or("").to_string();
        let author = row["author"].as_str().unwrap_or("");
        let ci = row["ci_status"].as_str().unwrap_or("");
        let score = row["score"].as_f64();
        let age = rel_time(row["updated_at"].as_str().unwrap_or(""));

        let height = 36.0;
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
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 3.0, bg);

        let title_color = if selected {
            visuals.selection.stroke.color
        } else {
            visuals.text_color()
        };
        ui.painter().text(
            rect.left_top() + egui::vec2(6.0, 3.0),
            egui::Align2::LEFT_TOP,
            &title,
            FontId::default(),
            title_color,
        );
        let mut meta = format!("{repo} #{number} · {author}");
        if !age.is_empty() {
            meta.push_str(&format!(" · {age}"));
        }
        ui.painter().text(
            rect.left_bottom() + egui::vec2(6.0, -3.0),
            egui::Align2::LEFT_BOTTOM,
            meta,
            FontId::proportional(11.0),
            visuals.weak_text_color(),
        );
        // 右侧：CI 圆点 + 评分
        let mut right = rect.right_center() + egui::vec2(-6.0, 0.0);
        if let Some(s) = score {
            ui.painter().text(
                right,
                egui::Align2::RIGHT_CENTER,
                format!("{s:.1}"),
                FontId::default(),
                score_color(s, visuals.dark_mode),
            );
            right.x -= 34.0;
        }
        if !ci.is_empty() {
            ui.painter().circle_filled(right - egui::vec2(5.0, 0.0), 4.0, ci_color(ci));
        }

        if resp.clicked() {
            self.selected = Some(key);
            self.open_detail(&repo, number);
        }
    }

    fn detail_panel(&mut self, ui: &mut egui::Ui) {
        if self.selected.is_none() {
            return;
        }
        let mut open = true;
        egui::Panel::right("detail")
            .resizable(true)
            .default_size(self.cfg.panel_width.unwrap_or(420.0) as f32)
            .min_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.detail_body(ui, &mut open);
                    });
            });
        if !open {
            self.selected = None;
            self.detail = None;
        }
    }

    fn detail_body(&mut self, ui: &mut egui::Ui, open: &mut bool) {
        let Some(sel) = self.selected.clone() else { return };
        let Some(detail) = &self.detail else { return };
        if (detail.repo.as_str(), detail.number) != (sel.0.as_str(), sel.1) {
            return;
        }
        let lang = self.lang().to_string();
        let Some(row) = self
            .rows
            .iter()
            .find(|r| r["repo"].as_str() == Some(&sel.0) && r["number"].as_i64() == Some(sel.1))
            .cloned()
        else {
            ui.label("…");
            return;
        };

        ui.horizontal(|ui| {
            ui.heading(row["title"].as_str().unwrap_or(""));
            if ui.small_button("✕").clicked() {
                *open = false;
            }
        });
        let url = row["url"].as_str().unwrap_or("").to_string();
        ui.label(format!(
            "{} #{} · {} · {}",
            sel.0,
            sel.1,
            row["author"].as_str().unwrap_or(""),
            row["status"].as_str().unwrap_or(""),
        ));
        if !url.is_empty() {
            ui.hyperlink_to(&url, &url);
        }
        ui.separator();

        // ── 评审结果 ──
        match &detail.review {
            Some(rv) => {
                let score = rv["score"].as_f64();
                if let Some(s) = score {
                    ui.label(
                        RichText::new(trf(&lang, "body_score", &[&format!("{s:.1}")]))
                            .strong()
                            .size(15.0),
                    );
                } else {
                    ui.label(tr(&lang, "body_score_failed"));
                }
                let mut size_bits = Vec::new();
                if let Some(l) = rv["size_label"].as_str() {
                    size_bits.push(l.to_string());
                }
                if let Some(f) = rv["files_changed"].as_i64() {
                    size_bits.push(trf(&lang, "size_files", &[&f.to_string()]));
                }
                let adds = rv["additions"].as_i64().unwrap_or(0);
                let dels = rv["deletions"].as_i64().unwrap_or(0);
                if adds > 0 || dels > 0 {
                    size_bits.push(format!("+{adds}/-{dels}"));
                }
                if !size_bits.is_empty() {
                    ui.weak(size_bits.join(" · "));
                }
                if let Some(dims) = rv["dimensions"].as_str() {
                    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(dims) {
                        let text = map
                            .iter()
                            .map(|(k, v)| format!("{k}: {}", v.as_f64().unwrap_or(0.0)))
                            .collect::<Vec<_>>()
                            .join("  ");
                        ui.label(trf(&lang, "body_dimensions", &[&text]));
                    }
                }
                if let Some(summary) = rv["summary"].as_str().filter(|s| !s.is_empty()) {
                    ui.separator();
                    egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.md_cache, summary);
                }
            }
            None => {
                ui.weak(tr(&lang, "detail_no_review"));
            }
        }
        ui.separator();

        // ── diff 视图 ──
        match &detail.diff {
            Some(files) => {
                for f in files {
                    let adds = f.lines.iter().filter(|l| l.kind == LineKind::Add).count();
                    let dels = f.lines.iter().filter(|l| l.kind == LineKind::Del).count();
                    egui::CollapsingHeader::new(format!("{}  (+{adds}/-{dels})", f.path))
                        .id_salt(&f.path)
                        .default_open(false)
                        .show(ui, |ui| {
                            render_diff_file(ui, f, &lang);
                        });
                }
            }
            None => {
                if detail.diff_loading {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(tr(&lang, "prog_fetch_diff"));
                    });
                } else {
                    ui.weak(tr(&lang, "detail_no_diff"));
                }
            }
        }
    }
}

/// 单文件 diff 行渲染：等宽字体 + LineKind 着色；超长文件截断（大 diff 不整屏渲染）。
/// TODO(egui): 真正的行虚拟化（egui_extras TableBody / show_rows），取消截断上限。
const MAX_DIFF_LINES: usize = 800;

fn render_diff_file(ui: &mut egui::Ui, f: &DiffFile, lang: &str) {
    let dark = ui.visuals().dark_mode;
    let mono = FontId::monospace(12.0);
    let n = f.lines.len().min(MAX_DIFF_LINES);
    // 长行不自动换行：横向滚动区域把内容宽度与面板宽度解耦，防止面板被撑宽
    egui::ScrollArea::horizontal()
        .id_salt(&f.path)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for line in &f.lines[..n] {
                ui.horizontal(|ui| {
                    if line.no > 0 {
                        ui.weak(
                            RichText::new(format!("{:>5}", line.no))
                                .monospace()
                                .size(11.0),
                        );
                    } else {
                        ui.weak(RichText::new("     ").monospace().size(11.0));
                    }
                    let mut text = RichText::new(&line.text)
                        .font(mono.clone())
                        .color(kind_color(line.kind, dark));
                    if line.kind == LineKind::Meta {
                        text = text.strong();
                    }
                    ui.add(egui::Label::new(text).extend());
                });
            }
        });
    if f.lines.len() > MAX_DIFF_LINES {
        ui.weak(trf(
            lang,
            "diff_truncated",
            &[&MAX_DIFF_LINES.to_string(), &f.lines.len().to_string()],
        ));
    }
}

fn kind_color(kind: LineKind, dark: bool) -> Color32 {
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

fn ci_color(ci: &str) -> Color32 {
    match ci {
        "success" => Color32::from_rgb(46, 160, 67),
        "failure" => Color32::from_rgb(209, 36, 47),
        "pending" => Color32::from_rgb(191, 135, 0),
        _ => Color32::GRAY,
    }
}

fn score_color(s: f64, dark: bool) -> Color32 {
    if s >= 7.0 {
        kind_color(LineKind::Add, dark)
    } else if s >= 5.0 {
        Color32::from_rgb(191, 135, 0)
    } else {
        kind_color(LineKind::Del, dark)
    }
}

impl eframe::App for PrDeskApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_msgs();
        self.poll_tick(ui.ctx());
        // 面板顺序即层级：先加的在外层；CentralPanel 永远最后
        self.top_bar(ui);
        self.status_bar(ui);
        self.detail_panel(ui);
        self.pr_list(ui);
    }
}

fn apply_theme(ctx: &egui::Context, theme: &str) {
    ctx.set_theme(if theme == "dark" { egui::Theme::Dark } else { egui::Theme::Light });
}

/// 加载 CJK 字体：egui 内置字体不含中文，Windows 上注册微软雅黑（失败再试黑体/宋体）。
fn install_cjk_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/Deng.ttf",
    ];
    let mut fonts = egui::FontDefinitions::default();
    for path in CANDIDATES {
        match std::fs::read(path) {
            Ok(bytes) => {
                fonts.font_data.insert("cjk".into(), egui::FontData::from_owned(bytes).into());
                fonts
                    .families
                    .entry(FontFamily::Proportional)
                    .or_default()
                    .push("cjk".into());
                fonts
                    .families
                    .entry(FontFamily::Monospace)
                    .or_default()
                    .push("cjk".into());
                tracing::info!("CJK 字体已加载: {path}");
                ctx.set_fonts(fonts);
                return;
            }
            Err(e) => tracing::debug!("字体不可用 {path}: {e}"),
        }
    }
    tracing::warn!("未找到可用 CJK 字体，中文将显示为方块");
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_e| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 配置锚定：cwd 里的 config.toml 优先（开发期从仓库根跑 exe 时，配置+数据都在
    // 仓库根，cargo clean 清 target/ 不会误伤——真实教训）；cwd 没有则用 exe 旁的
    //（便携分发场景：双击 exe 时配置+数据在 exe 旁，行为不变）。
    let exe_dir = pr_desk_rs::exe_dir();
    let exe_cfg = exe_dir.join("config.toml");
    let cwd_cfg = PathBuf::from("config.toml");
    let cfg_path = if cwd_cfg.exists() && cwd_cfg != exe_cfg { cwd_cfg } else { exe_cfg };
    let cfg = Config::load(&cfg_path)?;
    // 数据目录跟随配置目录（同上：防 cargo clean 误伤）
    let data_dir = cfg_path.parent().unwrap_or(&exe_dir).join("data");
    pr_desk_rs::set_data_dir(data_dir.clone());
    std::fs::create_dir_all(&data_dir)?;
    let active_name = if cfg.has_profile(&cfg.active_profile) {
        cfg.active_profile.clone()
    } else {
        "local".to_string()
    };
    let db_path = profile_db_path(&data_dir, &active_name);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Db::open(&db_path)?;
    let cfg = Config { active_profile: active_name, ..cfg };

    // GitHub 客户端（失败降级：本地列表仍可用，网络刷新跳过）
    let gh = match github::GitHub::new(cfg.token(), &cfg.proxy) {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!("GitHub 客户端初始化失败: {e}");
            None
        }
    };
    let rate_limit = gh.as_ref().map(|g| g.rate_limit_handle());

    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();

    let w = cfg.window_width.unwrap_or(1280.0) as f32;
    let h = cfg.window_height.unwrap_or(760.0) as f32;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([w, h]),
        ..Default::default()
    };
    let status_msg = if cfg.is_unconfigured() {
        tr(&cfg.language, "cfg_unconfigured").to_string()
    } else {
        trf(&cfg.language, "cfg_repos", &[&cfg.repos.len().to_string()])
    };

    eframe::run_native(
        "PR Desk",
        options,
        Box::new(move |cc| {
            install_cjk_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, &cfg.theme);
            // 字号档位（0.9 / 1.0 / 1.15）
            cc.egui_ctx.all_styles_mut(|style| {
                for fid in style.text_styles.values_mut() {
                    fid.size *= cfg.font_scale;
                }
            });

            let cmd_tx = spawn_worker(db_path, gh, msg_tx, cc.egui_ctx.clone());
            // 启动即刷新（先本地行上屏，网络在后台）
            let _ = cmd_tx.send(Cmd::Refresh);
            Ok(Box::new(PrDeskApp {
                refreshing: true,
                cfg: cfg.clone(),
                cfg_path: cfg_path.clone(),
                db,
                rows: Vec::new(),
                filter: String::new(),
                collapsed: HashSet::new(),
                selected: None,
                detail: None,
                status_msg,
                cmd_tx,
                msg_rx,
                rate_limit,
                last_poll: Instant::now(),
                md_cache: egui_commonmark::CommonMarkCache::default(),
            }))
        }),
    )
    // eframe 的错误类型不保证 Send+Sync，转字符串收敛进 anyhow
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
