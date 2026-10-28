//! PR Desk 桌面端（第一阶段）：本地 SQLite + GitHub 拉取 + 插件同步 + Slint UI。
//! 纯逻辑在 lib（pr_desk_rs）里，`cargo test --lib` 不需要编译 Slint。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use slint::{Model as _, ModelRc, SharedString, VecModel};

slint::include_modules!();

mod avatars;
mod model;

use pr_desk_rs::config::Config;
use pr_desk_rs::db::{self, Db};
use pr_desk_rs::reviewer::LlmSettings;
use pr_desk_rs::{diffview, github, ocr, reviewer, sync};

// ── 后台命令 / 回送消息 ──

enum Cmd {
    RefreshGithub,
    Sync,
    FetchDiff { repo: String, number: i64 },
    FetchInbox,                 // 拉取 GitHub 通知（RefreshGithub/Sync 也会顺带触发）
    MarkNotifRead(String),      // 标记通知线程已读，完成后重新拉取
    MarkAllNotifsRead,          // PUT /notifications 全部已读，完成后重新拉取
    AddNotifReview { repo: String, number: i64, title: String, url: String },  // Inbox「+ 预评审」入库
    FetchNotifDetail { repo: String, number: i64, kind: String },  // Inbox 预览面板：拉会话/Files/Diff
    ReloadConfig,   // 设置保存后热生效：重建客户端并立即刷新+同步
    RunReview { repo: String, number: i64 },  // 详情面板「AI 评审」：LLM 评审并入库
    FetchModels { base: String, key: String },  // 设置页「获取」：拉 OpenAI 兼容端点的模型列表
}

/// 状态栏配置状态文本。
fn config_status(cfg: &Config) -> String {
    if cfg.is_unconfigured() {
        return "未配置（点 ⚙ 设置）".into();
    }
    let mut parts = vec![format!("GitHub 已配置 {} 个仓库", cfg.repos.len())];
    parts.push(if cfg.sync_token.is_empty() { "sync 未启用".into() } else { "sync 已配置".into() });
    parts.join(" · ")
}

enum Msg {
    Status(String),
    // Slint 模型类型（SharedString/Image 基于 Rc）不能跨线程：
    // 行数据以 serde_json（Send）过界，PrListRow/PrGroup 在 UI 线程装配。
    Rows(Vec<serde_json::Value>),
    // 搜索框过滤词变化（UI 回调 send_msg 进来，apply_msg 里重建行模型）
    FilterChanged(String),
    // 只重组状态栏（配额 / 同步时间变化，不改临时消息）
    StatusBarRefresh,
    // 详情面板：文本/JSON 字符串过界；ConvoEntry/CheckRun/FileChange 模型在 UI 线程装配
    Panel {
        title: String,
        author: String,
        state: String,
        ci: String,
        opened: String,
        updated: String,
        body: String,
        priority: i32,
        pinned: bool,
        on_board: bool,
        convo_json: String,
        checks_json: String,
        files_json: String,
        // 评审 tab 数据（db.get_review 的 JSON 已格式化为 UI 直接可用的字段）
        review: ReviewProps,
    },
    ClosePanel,
    // AI 评审进度：状态栏显示 text；若详情面板正打开该 PR，面板置 running 并显示进度
    ReviewProgress { repo: String, number: i64, text: String },
    // AI 评审完成：状态栏提示；若面板正打开该 PR，刷新评审 tab 并置 done
    ReviewDone { repo: String, number: i64 },
    // AI 评审失败：状态栏报错；若面板正打开该 PR，置 failed
    ReviewFailed { repo: String, number: i64, err: String },
    // 同理：diff 以原始文本过界，DiffFile 模型在 UI 线程的 apply_msg 里构建
    DiffRaw(String),
    // Inbox 通知行：serde_json 过界，NotifRow 模型在 UI 线程装配
    InboxRows(Vec<serde_json::Value>),
    // Inbox 预览面板：JSON 字符串过界，ConvoEntry/FileChange/DiffFile 在 UI 线程装配
    //（标题/类型在点击时已由 on_inbox_activated 设置，这里只回送内容数据）
    NotifDetail {
        convo_json: String,
        files_json: String,
        diff: String,
    },
    // 设置页模型列表拉取结果（id 已排序去重）
    ModelsLoaded(Vec<String>),
}

/// OCR 行级评论展示数据（纯 Rust 字符串可跨线程；Slint OcrComment 在 UI 线程装配）。
struct OcrCommentRow {
    severity: String,
    category: String,
    location: String,   // 如 "src/foo.rs:42-58"
    content: String,
}

/// 详情面板「评审」tab 的展示数据（从 db.get_review 的 JSON 格式化而来）。
struct ReviewProps {
    has: bool,
    score: f32,
    // 旧版纯文本分项行（"可读性  8/10"）：UI 已改用结构化 dimensions，
    // 字段保留以免破坏其它调用方/旧面板文本（open_drawer 的 body 仍按旧格式拼装）
    #[allow(dead_code)]
    dimension_lines: Vec<String>,
    // 结构化分项（中文标签 + 分数），与 dimension_lines 同源，供新版 UI 按条渲染
    dimensions: Vec<(String, f32)>,
    summary: String,
    // 摘要的 Markdown 解析块（LLM 摘要含标记，纯文本可读性差）
    summary_blocks: Vec<pr_desk_rs::markdown::MdBlockOut>,
    // 体量行，如 "XL（+2122/-23，24 个文件）"；缺字段时给空串
    size_line: String,
    flagged: Vec<String>,
    reviewed_at: String,
    comments: Vec<OcrCommentRow>,
}

/// 评审分项键名 → 中文名（未知键原样展示）。
fn dim_label(key: &str) -> &str {
    match key {
        "readability" => "可读性",
        "correctness_risk" => "正确性风险",
        "test_coverage" => "测试覆盖",
        "security" => "安全",
        other => other,
    }
}

/// 评审字段可能是 JSON 字符串（旧数据）或内联对象/数组，两种都解析。
fn json_field(rv: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    let v = &rv[key];
    if let Some(s) = v.as_str() {
        serde_json::from_str(s).ok()
    } else if v.is_null() {
        None
    } else {
        Some(v.clone())
    }
}

/// OCR 评论严重度排序权重：critical → high → medium → low → 其他。
fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

/// OCR 评论位置格式化：path:start-end；start=end 只一个行号；都是 0 只留 path。
fn ocr_comment_location(c: &serde_json::Value) -> String {
    let path = c["path"].as_str().unwrap_or("");
    let start = c["start_line"].as_i64().unwrap_or(0);
    let end = c["end_line"].as_i64().unwrap_or(0);
    if start == 0 && end == 0 {
        path.to_string()
    } else if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

/// db.get_review 的 "comments" 数组 → 行级评论展示数据（按 severity 分组排序，组内保持原序）。
fn ocr_comment_rows(rv: &serde_json::Value) -> Vec<OcrCommentRow> {
    let mut rows: Vec<OcrCommentRow> = json_field(rv, "comments")
        .and_then(|c| c.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|c| OcrCommentRow {
            severity: c["severity"].as_str().filter(|s| !s.is_empty()).unwrap_or("other").to_string(),
            category: c["category"].as_str().filter(|s| !s.is_empty()).unwrap_or("other").to_string(),
            location: ocr_comment_location(c),
            content: c["content"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    rows.sort_by_key(|c| severity_rank(&c.severity));
    rows
}

/// 体量行组装："XL（+2122/-23，24 个文件）"；label/增删行/文件数全缺时给空串。
fn size_line(rv: &serde_json::Value) -> String {
    let label = rv["size_label"].as_str().unwrap_or("");
    let adds = rv["additions"].as_i64();
    let dels = rv["deletions"].as_i64();
    let files = rv["files_changed"].as_i64();
    if label.is_empty() && adds.is_none() && dels.is_none() && files.is_none() {
        return String::new();
    }
    let mut parts = Vec::new();
    if adds.is_some() || dels.is_some() {
        parts.push(format!("+{}/-{}", adds.unwrap_or(0), dels.unwrap_or(0)));
    }
    if let Some(f) = files {
        parts.push(format!("{f} 个文件"));
    }
    if parts.is_empty() {
        label.to_string()
    } else if label.is_empty() {
        format!("（{}）", parts.join("，"))
    } else {
        format!("{label}（{}）", parts.join("，"))
    }
}

fn review_props(review: Option<&serde_json::Value>) -> ReviewProps {
    let Some(rv) = review else {
        return ReviewProps {
            has: false,
            score: 0.0,
            dimension_lines: Vec::new(),
            dimensions: Vec::new(),
            summary: String::new(),
            summary_blocks: Vec::new(),
            size_line: String::new(),
            flagged: Vec::new(),
            reviewed_at: String::new(),
            comments: Vec::new(),
        };
    };
    let dims = json_field(rv, "dimensions")
        .and_then(|d| d.as_object().cloned())
        .unwrap_or_default();
    let dimension_lines = dims
        .iter()
        .map(|(k, v)| format!("{}  {}/10", dim_label(k), v))
        .collect();
    let dimensions = dims
        .iter()
        .map(|(k, v)| (dim_label(k).to_string(), v.as_f64().unwrap_or(0.0) as f32))
        .collect();
    let summary = rv["summary"].as_str().unwrap_or("").to_string();
    let flagged = json_field(rv, "flagged_files")
        .and_then(|f| f.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|f| {
            format!(
                "{} — {}",
                f["path"].as_str().unwrap_or("?"),
                f["issue"].as_str().unwrap_or("")
            )
        })
        .collect();
    ReviewProps {
        has: true,
        score: rv["score"].as_f64().unwrap_or(0.0) as f32,
        dimension_lines,
        dimensions,
        summary_blocks: pr_desk_rs::markdown::parse_markdown(&summary),
        summary,
        size_line: size_line(rv),
        flagged,
        reviewed_at: rv["reviewed_at"].as_str().unwrap_or("").to_string(),
        comments: ocr_comment_rows(rv),
    }
}

/// 详情面板当前是否正打开指定 PR（评审进度/结果只刷对应面板）。
/// 选中态存身份（repo, number），刷新后行序变化/行被移除都不影响匹配。
fn panel_matches(app: &Arc<Mutex<App>>, repo: &str, number: i64) -> bool {
    let a = app.lock().unwrap();
    a.selected
        .as_ref()
        .map(|(r, n)| r == repo && *n == number)
        .unwrap_or(false)
}

/// 预评审并发槽位（纯逻辑，可单测）：限制同时在跑的评审数，并防止同一 PR 重入。
/// 容量来自配置 review_concurrency（设置页可改，保存即时生效）。
struct ReviewSlots {
    capacity: usize,
    inflight: HashSet<(String, i64)>,
}

#[derive(Debug, PartialEq, Eq)]
enum SlotError {
    AlreadyRunning, // 同一 PR 已有评审在跑
    AtCapacity,     // 槽位已满
}

impl ReviewSlots {
    fn new(capacity: usize) -> Self {
        Self { capacity, inflight: HashSet::new() }
    }

    /// 占用一个槽位；同一 PR 重入报 AlreadyRunning，超额报 AtCapacity。
    fn try_acquire(&mut self, repo: &str, number: i64) -> Result<(), SlotError> {
        if self.inflight.contains(&(repo.to_string(), number)) {
            return Err(SlotError::AlreadyRunning);
        }
        if self.inflight.len() >= self.capacity {
            return Err(SlotError::AtCapacity);
        }
        self.inflight.insert((repo.to_string(), number));
        Ok(())
    }

    fn release(&mut self, repo: &str, number: i64) {
        self.inflight.remove(&(repo.to_string(), number));
    }

    /// 面板打开时查询：该 PR 是否正在评审。
    fn is_running(&self, repo: &str, number: i64) -> bool {
        self.inflight.contains(&(repo.to_string(), number))
    }

    /// 设置页调整并发数（不影响已占用的槽位）。
    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
    }
}

/// 槽位 RAII 守卫：评审任务结束（无论成败）自动释放，不泄漏槽位。
struct SlotGuard {
    slots: Arc<Mutex<ReviewSlots>>,
    repo: String,
    number: i64,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.slots.lock().unwrap().release(&self.repo, self.number);
    }
}

struct App {
    cfg: Config,
    db: Db,
    rows: Vec<serde_json::Value>,           // 扁平数据行（按身份查找，见 find_row）
    view: Vec<usize>,                       // 过滤后可见行 → rows 下标（UI 下标经它换算）
    filter: String,                         // 当前过滤词（空 = 全量）
    selected: Option<(String, i64)>,        // 选中 PR 身份（repo, number）；刷新后行序变化也不漂移
    status_msg: String,                     // 状态栏临时消息（最近一条 Status）
    inbox: Vec<serde_json::Value>,          // 最近一批通知行（inbox_activated 按下标取 thread id）
    inbox_all: bool,                        // 通知拉取范围：true = 含已读（&all=true），false = 只看未读
    inbox_detail: Option<(String, i64, String, String)>,  // 预览面板当前行（repo, number, kind, url）
    total: usize,                           // 全部行数（状态栏「共 N 个 PR」）
    rate_limit: Arc<Mutex<Option<u64>>>,    // GitHub X-RateLimit-Remaining 共享状态
    last_sync: sync::SharedLastSync,        // 最近同步成功时间
    review_slots: Arc<Mutex<ReviewSlots>>,  // 预评审并发槽（worker 与 UI 侧 open_drawer 共享）
}

/// 状态栏组合逻辑已内联进 update_status_bar（三段：同步/配额/总数）。
fn update_status_bar(ui: &MainWindow, app: &Arc<Mutex<App>>) {
    let a = app.lock().unwrap();
    // 状态栏三段：同步/临时消息 · API 配额 · PR 总数
    let sync_text = if !a.status_msg.is_empty() {
        a.status_msg.clone()
    } else if a.cfg.sync_token.is_empty() {
        // 未配置同步：明确提示本地模式，不留空白
        "本地模式 · 未配置同步服务器".into()
    } else {
        sync::last_sync_text(&a.last_sync).unwrap_or_else(|| "等待首次同步…".into())
    };
    let quota = a.rate_limit.lock().unwrap()
        .map(|n| format!("API 配额 {n}"))
        .unwrap_or_default();
    ui.set_sb_sync(sync_text.into());
    ui.set_sb_quota(quota.into());
    ui.set_sb_total(a.total as i32);
}

/// 当前过滤词下可见行的原始下标。
fn filtered_view(rows: &[serde_json::Value], filter: &str) -> Vec<usize> {
    let q = filter.trim().to_lowercase();
    if q.is_empty() {
        return (0..rows.len()).collect();
    }
    rows.iter()
        .enumerate()
        .filter(|(_, r)| model::row_matches(r, &q))
        .map(|(i, _)| i)
        .collect()
}

/// 按身份（repo, number）在 rows 里现找下标。
/// 调用方须已持有 app 锁：查找与随后读行在同一锁内完成，避免与刷新竞态。
fn find_row(a: &App, repo: &str, number: i64) -> Option<usize> {
    a.rows
        .iter()
        .position(|r| r["repo"].as_str() == Some(repo) && r["number"].as_i64() == Some(number))
}

/// UI 传来的可见行下标 → 该行 PR 的身份（repo, number）。
fn resolve_view(app: &Arc<Mutex<App>>, view_idx: usize) -> Option<(String, i64)> {
    let a = app.lock().unwrap();
    let idx = *a.view.get(view_idx)?;
    let r = a.rows.get(idx)?;
    Some((r["repo"].as_str()?.to_string(), r["number"].as_i64()?))
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_e| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 配置/数据目录锚定到 exe 所在目录（便携应用惯例）：
    // 防止从不同工作目录启动时读到另一份 config.toml / 数据库（曾导致「双配置」分裂）。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let cfg_path = exe_dir.join("config.toml");
    // 兼容迁移：exe 旁没有但旧式 cwd 里有配置时，搬过来再用
    let cwd_cfg = PathBuf::from("config.toml");
    if !cfg_path.exists() && cwd_cfg.exists() && cwd_cfg != cfg_path {
        let _ = std::fs::copy(&cwd_cfg, &cfg_path);
    }
    let cfg = Config::load(&cfg_path)?;
    let data_dir = exe_dir.join("data");
    std::fs::create_dir_all(&data_dir)?;
    let db = Db::open(&data_dir.join("pr-desk.db"))?;

    let ui = MainWindow::new()?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();

    let app = Arc::new(Mutex::new(App {
        cfg: cfg.clone(),
        db,
        rows: Vec::new(),
        view: Vec::new(),
        filter: String::new(),
        selected: None,
        status_msg: config_status(&cfg),
        inbox: Vec::new(),
        inbox_all: true,
        inbox_detail: None,
        total: 0,
        rate_limit: Arc::new(Mutex::new(None)),
        last_sync: sync::shared_last_sync(),
        review_slots: Arc::new(Mutex::new(ReviewSlots::new(cfg.review_concurrency as usize))),
    }));

    // 设置页初值 + 首启引导 + 状态栏配置状态
    ui.set_s_server(cfg.server_url.clone().into());
    ui.set_s_sync_token(cfg.sync_token.clone().into());
    ui.set_s_ak(cfg.astrbot_api_key.clone().into());
    ui.set_s_gh_token(cfg.github_token.clone().unwrap_or_default().into());
    ui.set_s_repos(cfg.repos.join("\n").into());
    ui.set_s_poll(cfg.poll_interval_min as i32);
    ui.set_s_llm_base(cfg.llm_base_url.clone().into());
    ui.set_s_llm_key(cfg.llm_api_key.clone().unwrap_or_default().into());
    ui.set_s_llm_model(cfg.llm_model.clone().into());
    ui.set_s_ocr_path(cfg.ocr_path.clone().into());
    ui.set_s_review_concurrency(cfg.review_concurrency as i32);
    ui.set_gh_token_from_env(std::env::var("GITHUB_TOKEN").map(|t| !t.is_empty()).unwrap_or(false));
    let unconfigured = cfg.is_unconfigured();
    ui.set_onboarding(unconfigured);
    if unconfigured {
        ui.set_settings_open(true); // 首启自动打开设置
    }
    ui.set_sb_sync(config_status(&cfg).into());
    // 通知接口需要鉴权：无 token 时 Inbox 视图显示提示空态
    ui.set_inbox_needs_token(cfg.token().is_none());
    // 启动时把配置的主题应用到 UI
    ui.set_dark_mode(cfg.is_dark());

    // 主题切换：即时持久化到 config.toml
    {
        let app = app.clone();
        let cfg_path = cfg_path.clone();
        ui.on_theme_toggled(move |dark| {
            let mut a = app.lock().unwrap();
            a.cfg.theme = if dark { "dark".into() } else { "light".into() };
            if let Err(e) = a.cfg.save(&cfg_path) {
                tracing::warn!("主题写入 config.toml 失败: {e}");
            }
        });
    }

    // ── 后台工作线程（tokio runtime） ──
    {
        let app = app.clone();
        let msg_tx = msg_tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(worker(app, cmd_rx, msg_tx));
        });
    }

    // 自动同步定时器（每轮重新读当前间隔，设置页改间隔下轮生效）
    {
        let cmd_tx = cmd_tx.clone();
        let app2 = app.clone();
        std::thread::spawn(move || loop {
            let interval = app2.lock().unwrap().cfg.poll_interval_min.max(1);
            std::thread::sleep(std::time::Duration::from_secs(interval * 60));
            if cmd_tx.send(Cmd::Sync).is_err() {
                break;
            }
        });
    }

    // ── 消息回 UI ──
    {
        let weak = ui.as_weak();
        let app = app.clone();
        std::thread::spawn(move || loop {
            match msg_rx.recv() {
                Ok(msg) => {
                    let weak = weak.clone();
                    let app = app.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            apply_msg(&ui, &app, msg);
                        }
                    });
                }
                Err(_) => break,
            }
        });
    }

    // ── UI 回调 ──
    {
        let cmd_tx = cmd_tx.clone();
        ui.on_refresh(move || {
            let _ = cmd_tx.send(Cmd::RefreshGithub);
        });
    }
    {
        let cmd_tx = cmd_tx.clone();
        ui.on_sync_now(move || {
            let _ = cmd_tx.send(Cmd::Sync);
        });
    }
    {
        let cmd_tx = cmd_tx.clone();
        ui.on_fetch_inbox(move || {
            let _ = cmd_tx.send(Cmd::FetchInbox);
        });
    }
    {
        let cmd_tx = cmd_tx.clone();
        ui.on_inbox_mark_all_read(move || {
            let _ = cmd_tx.send(Cmd::MarkAllNotifsRead);
        });
    }
    {
        // 「只看未读」开关：状态存 App（worker 拉取时读取），切完立即重拉
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_inbox_filter_toggled(move |unread_only| {
            app.lock().unwrap().inbox_all = !unread_only;
            let _ = cmd_tx.send(Cmd::FetchInbox);
        });
    }
    {
        // 「+ 预评审」：按 UI 下标取 repo/number/title 交给 worker 入库
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_inbox_add_review(move |i| {
            let row = app.lock().unwrap().inbox.get(i as usize).cloned();
            if let Some(n) = row {
                let repo = n["repo"].as_str().unwrap_or("").to_string();
                let title = n["title"].as_str().unwrap_or("").to_string();
                let url = n["url"].as_str().unwrap_or("").to_string();
                if let Some(number) = notif_url_number(&url) {
                    let _ = cmd_tx.send(Cmd::AddNotifReview { repo, number, title, url });
                }
            }
        });
    }
    {
        // 顶栏搜索框过滤词 → 重建行模型（view 下标映射在 resolve_view）
        ui.on_filter_changed(|q| send_msg(Msg::FilterChanged(q.to_string())));
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        // UI 传视图下标，先经 view 换算为该行的身份（repo, number）
        ui.on_row_activated(move |i| {
            if let Some((repo, number)) = resolve_view(&app, i as usize) {
                open_drawer(&app, &cmd_tx, repo, number);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_row_pin(move |i| {
            if let Some((repo, number)) = resolve_view(&app, i as usize) {
                let v = toggle_of(&app, &repo, number, "pinned");
                local_update(&app, &repo, number, "pinned", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_row_board(move |i| {
            if let Some((repo, number)) = resolve_view(&app, i as usize) {
                let v = toggle_of(&app, &repo, number, "on_board");
                local_update(&app, &repo, number, "on_board", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_row_reject(move |i| {
            let Some((repo, number)) = resolve_view(&app, i as usize) else { return };
            let cur = {
                let a = app.lock().unwrap();
                find_row(&a, &repo, number)
                    .and_then(|idx| a.rows[idx]["status"].as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "pending".into())
            };
            let to = if cur == "rejected" { "pending" } else { "rejected" };
            local_update(&app, &repo, number, "status", to);
        });
    }
    {
        // 详情面板：关闭（Esc 语义的第一支在 app.slint 里，这里同时清选中态）
        let app = app.clone();
        ui.on_panel_close(move || {
            app.lock().unwrap().selected = None;
            send_msg(Msg::ClosePanel);
        });
    }
    {
        // 面板看板按钮：切换当前选中 PR 的 on_board
        let app = app.clone();
        ui.on_panel_board(move || {
            let sel = app.lock().unwrap().selected.clone();
            if let Some((repo, number)) = sel {
                let v = toggle_of(&app, &repo, number, "on_board");
                local_update(&app, &repo, number, "on_board", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_panel_set_priority(move |p| {
            let sel = app.lock().unwrap().selected.clone();
            if let Some((repo, number)) = sel {
                local_update(&app, &repo, number, "priority", p as i64);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_panel_toggle_pin(move || {
            let sel = app.lock().unwrap().selected.clone();
            if let Some((repo, number)) = sel {
                let v = toggle_of(&app, &repo, number, "pinned");
                local_update(&app, &repo, number, "pinned", v);
            }
        });
    }
    {
        // 详情面板「AI 评审」按钮：取当前选中 PR 的身份交给 worker 跑 LLM 评审
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_action_run_review(move || {
            let target = app.lock().unwrap().selected.clone();
            if let Some((repo, number)) = target {
                let _ = cmd_tx.send(Cmd::RunReview { repo, number });
            }
        });
    }
    {
        // DiffView 要求整体替换模型（原地改会导致其计数器重复累加）；
        // 折叠状态以 UI 侧当前模型为准（Slint 数据不过线程边界）。
        let weak = ui.as_weak();
        ui.on_diff_file_toggled(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let old = ui.get_diff_files();
            let mut files: Vec<DiffFile> = (0..old.row_count())
                .filter_map(|gi| old.row_data(gi))
                .collect();
            if let Some(f) = files.get_mut(i as usize) {
                f.expanded = !f.expanded;
            }
            ui.set_diff_files(ModelRc::new(VecModel::from(files)));
        });
    }
    {
        // Inbox 行点击 / Enter：选中 + 展开右侧预览面板（发 FetchNotifDetail），并标记已读
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_inbox_activated(move |i| {
            let row = app.lock().unwrap().inbox.get(i as usize).cloned();
            let Some(n) = row else { return };
            let repo = n["repo"].as_str().unwrap_or("").to_string();
            let kind = n["kind"].as_str().unwrap_or("").to_string();
            let title = n["title"].as_str().unwrap_or("").to_string();
            let url = n["url"].as_str().unwrap_or("").to_string();
            let Some(number) = notif_url_number(&url) else { return };
            app.lock().unwrap().inbox_detail = Some((repo.clone(), number, kind.clone(), url));
            // 立即开面板并进加载态（数据由 worker 回送）
            let added = kind == "PullRequest" && {
                let a = app.lock().unwrap();
                a.db.pr_exists(&repo, number).unwrap_or(false)
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_nd_title(title.into());
                ui.set_nd_kind(kind.clone().into());
                ui.set_nd_added(added);
                ui.set_nd_loading(true);
                ui.set_inbox_detail_open(true);
            }
            let _ = cmd_tx.send(Cmd::FetchNotifDetail { repo, number, kind });
            if let Some(id) = n["id"].as_str().filter(|s| !s.is_empty()) {
                let _ = cmd_tx.send(Cmd::MarkNotifRead(id.to_string()));
            }
        });
    }
    {
        // 预览面板「在浏览器打开」/ Enter（面板已开时）
        let app = app.clone();
        ui.on_inbox_open_browser(move || {
            let detail = app.lock().unwrap().inbox_detail.clone();
            if let Some((_, _, _, url)) = detail {
                if !url.is_empty() {
                    if let Err(e) = open::that(&url) {
                        send_msg(Msg::Status(format!("打开浏览器失败: {e}")));
                    }
                }
            }
        });
    }
    {
        // 预览面板关闭（× 按钮 / Esc）
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_inbox_close_detail(move || {
            app.lock().unwrap().inbox_detail = None;
            if let Some(ui) = weak.upgrade() {
                ui.set_inbox_detail_open(false);
            }
        });
    }
    {
        // 预览面板 Diff 标签页：整体替换模型（同主面板 on_diff_file_toggled 的注释）
        let weak = ui.as_weak();
        ui.on_inbox_diff_toggled(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let old = ui.get_nd_diff_files();
            let mut files: Vec<DiffFile> = (0..old.row_count())
                .filter_map(|gi| old.row_data(gi))
                .collect();
            if let Some(f) = files.get_mut(i as usize) {
                f.expanded = !f.expanded;
            }
            ui.set_nd_diff_files(ModelRc::new(VecModel::from(files)));
        });
    }
    {
        let app = app.clone();
        ui.on_key_pin(move || {
            let sel = app.lock().unwrap().selected.clone();
            if let Some((repo, number)) = sel {
                let v = toggle_of(&app, &repo, number, "pinned");
                local_update(&app, &repo, number, "pinned", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_key_priority(move |p| {
            let sel = app.lock().unwrap().selected.clone();
            if let Some((repo, number)) = sel {
                local_update(&app, &repo, number, "priority", p as i64);
            }
        });
    }
    // ── 设置页 ──
    {
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_open_settings(move || {
            if let Some(ui) = ui2.upgrade() {
                // 每次打开回填当前配置（LLM 字段可能在别处被改动）
                let cfg = app.lock().unwrap().cfg.clone();
                ui.set_s_ak(cfg.astrbot_api_key.clone().into());
                ui.set_s_llm_base(cfg.llm_base_url.into());
                ui.set_s_llm_key(cfg.llm_api_key.unwrap_or_default().into());
                ui.set_s_llm_model(cfg.llm_model.into());
                ui.set_s_ocr_path(cfg.ocr_path.into());
                ui.set_s_review_concurrency(cfg.review_concurrency as i32);
                ui.set_settings_open(true);
            }
        });
    }
    // 设置页「获取」模型列表：用弹窗里正在编辑的 base/key 拉取（无需先保存）
    {
        let cmd_tx = cmd_tx.clone();
        ui.on_fetch_models(move |base, key| {
            let _ = cmd_tx.send(Cmd::FetchModels { base: base.to_string(), key: key.to_string() });
        });
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let ui2 = ui.as_weak();
        let cfg_path = cfg_path.clone();
        ui.on_save_settings(move |server, sync_tok, gh_tok, repos_text, poll_s, llm_base, llm_key, llm_model, ocr_path, review_conc_s, ak| {
            let repos: Vec<String> = repos_text
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && l.contains('/'))
                .collect();
            let poll: u64 = poll_s.trim().parse().unwrap_or(5).max(1);
            let review_concurrency: u64 = match review_conc_s.trim().parse::<u64>() {
                Ok(n) if (1..=8).contains(&n) => n,
                _ => 3,
            };
            let ocr_path = ocr_path.trim();
            let new_cfg = Config {
                server_url: server.trim().to_string(),
                sync_token: sync_tok.trim().to_string(),
                astrbot_api_key: ak.trim().to_string(),
                github_token: Some(gh_tok.trim().to_string()).filter(|t| !t.is_empty()),
                repos,
                poll_interval_min: poll,
                theme: app.lock().unwrap().cfg.theme.clone(), // 主题不在设置页，保留当前值
                llm_base_url: llm_base.trim().to_string(),
                llm_api_key: Some(llm_key.trim().to_string()).filter(|t| !t.is_empty()),
                llm_model: llm_model.trim().to_string(),
                ocr_path: if ocr_path.is_empty() { "ocr".into() } else { ocr_path.to_string() },
                review_concurrency,
            };
            if let Err(e) = new_cfg.save(&cfg_path) {
                send_msg(Msg::Status(format!("配置保存失败: {e}")));
                return;
            }
            {
                let mut a = app.lock().unwrap();
                a.cfg = new_cfg.clone();
                a.status_msg = config_status(&new_cfg);
                // 并发上限即时生效（不影响已占用的槽位）
                a.review_slots.lock().unwrap().set_capacity(review_concurrency as usize);
            }
            if let Some(ui) = ui2.upgrade() {
                ui.set_onboarding(new_cfg.is_unconfigured());
                ui.set_inbox_needs_token(new_cfg.token().is_none());
                update_status_bar(&ui, &app);
            }
            send_msg(Msg::Status("配置已保存并生效".into()));
            let _ = cmd_tx.send(Cmd::ReloadConfig); // 热生效，不用重启
        });
    }

    // 启动：初始刷新 + 同步
    let _ = cmd_tx.send(Cmd::RefreshGithub);
    let _ = cmd_tx.send(Cmd::Sync);

    ui.run()?;
    Ok(())
}

// 全局发信器（worker 启动时注册；UI 线程的即时反馈也经它回到事件循环）
static MSG_TX: std::sync::OnceLock<mpsc::Sender<Msg>> = std::sync::OnceLock::new();

fn send_msg(msg: Msg) {
    if let Some(tx) = MSG_TX.get() {
        let _ = tx.send(msg);
    }
}

fn toggle_of(app: &Arc<Mutex<App>>, repo: &str, number: i64, field: &str) -> i64 {
    let a = app.lock().unwrap();
    find_row(&a, repo, number)
        .and_then(|idx| a.rows[idx][field].as_i64())
        .map(|v| if v == 0 { 1 } else { 0 })
        .unwrap_or(1)
}

/// 本地字段更新（pinned/on_board/priority/status）：按身份直改 DB，
/// 不依赖行是否还在当前列表里（面板开着但行被刷新滤掉时操作仍生效）。
fn local_update<V: Into<serde_json::Value>>(app: &Arc<Mutex<App>>, repo: &str, number: i64, field: &str, value: V) {
    let a = app.lock().unwrap();
    if a.db.local_pr_update(repo, number, field, &value.into()).is_ok() {
        drop(a);
        reload_rows_global(app);
        send_msg(Msg::Status(format!("已更新 {field}（待同步）")));
    }
}

fn reload_rows_global(app: &Arc<Mutex<App>>) {
    let rows = {
        let a = app.lock().unwrap();
        a.db.list_prs(list_sources(&a.cfg)).unwrap_or_default()
    };
    app.lock().unwrap().rows = rows.clone();
    // serde_json 行可跨线程；Slint 模型在 UI 线程（apply_msg）装配
    send_msg(Msg::Rows(rows));
}

// ── 详情抽屉 ──

fn open_drawer(app: &Arc<Mutex<App>>, cmd_tx: &mpsc::Sender<Cmd>, repo: String, number: i64) {
    // 选中态存身份而非下标：刷新后行序变化，面板仍指向同一个 PR；
    // 行被刷新滤掉时面板也保持打开（数据来自 DB/快照，row 允许为 None）
    let (priority, row_idx) = {
        let mut a = app.lock().unwrap();
        a.selected = Some((repo.clone(), number));
        let idx = find_row(&a, &repo, number);
        let priority = idx
            .and_then(|i| a.rows[i]["priority"].as_i64())
            .unwrap_or(0) as i32;
        (priority, idx)
    };

    let (review, diff_cached, row) = {
        let a = app.lock().unwrap();
        (
            a.db.get_review(&repo, number).ok().flatten(),
            a.db.get_diff(&repo, number).ok().flatten(),
            row_idx.and_then(|i| a.rows.get(i).cloned()),
        )
    };

    // tab0 评审文本：评分 + 分项 + 摘要 + flagged
    let mut body = String::new();
    if let Some(rv) = &review {
        if let Some(s) = rv["score"].as_f64() {
            body.push_str(&format!("综合评分：{s:.1}/10\n"));
        } else {
            body.push_str("评审解析失败（分数为空）\n");
        }
        if let Some(dims) = rv["dimensions"].as_str() {
            if let Ok(d) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(dims) {
                let parts: Vec<String> = d.iter().map(|(k, v)| format!("{k} {}", v)).collect();
                body.push_str(&format!("分项：{}\n", parts.join(" | ")));
            }
        }
        if let Some(sz) = rv["size_label"].as_str() {
            body.push_str(&format!("体量：{sz}\n"));
        }
        if let Some(s) = rv["summary"].as_str() {
            body.push_str(&format!("\n{s}"));
        }
        if let Some(fl) = rv["flagged_files"].as_str() {
            if let Ok(files) = serde_json::from_str::<Vec<serde_json::Value>>(fl) {
                if !files.is_empty() {
                    body.push_str("\n\n⚠ 风险文件：");
                    for f in files {
                        body.push_str(&format!(
                            "\n- {}：{}",
                            f["path"].as_str().unwrap_or("?"),
                            f["issue"].as_str().unwrap_or("")
                        ));
                    }
                }
            }
        }
    }

    // 会话 / Checks / Files：JSON 过界（Slint 模型不跨线程），UI 线程装配
    let (convo_json, checks_json) = row
        .as_ref()
        .map(|r| drawer_extras_json(r))
        .unwrap_or_default();
    let files_json = match &diff_cached {
        Some(d) => files_json_from_diff(d),
        None => "[]".into(),
    };
    let state = row
        .as_ref()
        .and_then(|r| r["status"].as_str().map(|s| s.to_string()))
        .map(|s| if s == "merged" || s == "closed" { s } else { "open".into() })
        .unwrap_or_else(|| "open".into());
    let ci = row
        .as_ref()
        .and_then(|r| r["ci_status"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let age = row.as_ref().map(|r| model::age_text(r)).unwrap_or_default();
    let pinned = row.as_ref().map(|r| r["pinned"].as_i64() == Some(1)).unwrap_or(false);
    let on_board = row.as_ref().map(|r| r["on_board"].as_i64() == Some(1)).unwrap_or(false);

    send_msg(Msg::Panel {
        title: row
            .as_ref()
            .and_then(|r| r["title"].as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{repo} #{number}")),
        author: row
            .as_ref()
            .and_then(|r| r["author"].as_str().map(|s| s.to_string()))
            .unwrap_or_default(),
        state,
        ci,
        opened: age.clone(),
        updated: age,
        body,
        priority,
        pinned,
        on_board,
        convo_json,
        checks_json,
        files_json,
        review: review_props(review.as_ref()),
    });

    // 评审进行中打开该 PR 面板：紧随 Panel 之后把面板置 running
    //（apply_msg 顺序处理，ReviewProgress 在 Panel 重置状态后生效）
    let running = app.lock().unwrap().review_slots.lock().unwrap().is_running(&repo, number);
    if running {
        send_msg(Msg::ReviewProgress { repo: repo.clone(), number, text: "评审进行中…".into() });
    }

    match diff_cached {
        Some(d) => send_msg(Msg::DiffRaw(d)),
        None => {
            let _ = cmd_tx.send(Cmd::FetchDiff { repo, number });
        }
    }
}

/// extras_detail / recent_commits → (会话条目 JSON, Checks JSON)。
/// 条目结构对齐 detail_panel.slint 的 ConvoEntry/CheckRun（kind/author/time/text、name/conclusion）。
fn drawer_extras_json(r: &serde_json::Value) -> (String, String) {
    let mut convo: Vec<serde_json::Value> = Vec::new();
    let mut checks: Vec<serde_json::Value> = Vec::new();
    if let Some(detail_str) = r["extras_detail"].as_str() {
        if let Ok(detail) = serde_json::from_str::<serde_json::Value>(detail_str) {
            if let Some(rvs) = detail["reviews"].as_array() {
                for rv in rvs {
                    convo.push(serde_json::json!({
                        "kind": "review",
                        "author": rv["author"].as_str().unwrap_or(""),
                        "time": rv["submitted_at"].as_str().unwrap_or(""),
                        "text": rv["body"].as_str().unwrap_or(""),
                    }));
                }
            }
            if let Some(runs) = detail["check_runs"].as_array() {
                for c in runs {
                    checks.push(serde_json::json!({
                        "name": c["name"].as_str().unwrap_or(""),
                        "conclusion": if c["status"].as_str() == Some("completed") {
                            c["conclusion"].as_str().unwrap_or("")
                        } else {
                            "pending"
                        },
                    }));
                }
            }
            if let Some(cs) = detail["commits"].as_array() {
                for c in cs {
                    convo.push(serde_json::json!({
                        "kind": "commit",
                        "author": c["author"].as_str().unwrap_or(""),
                        "time": c["date"].as_str().unwrap_or(""),
                        "text": c["title"].as_str().unwrap_or(""),
                    }));
                }
            }
        }
    }
    // 无 extras_detail 时退回 recent_commits 标题列表
    if convo.is_empty() {
        if let Some(rc) = r["recent_commits"].as_str() {
            if let Ok(cs) = serde_json::from_str::<Vec<String>>(rc) {
                for c in cs {
                    convo.push(serde_json::json!({"kind": "commit", "author": "", "time": "", "text": c}));
                }
            }
        }
    }
    // 时间线按时间升序（空时间排最后）
    convo.sort_by(|a, b| {
        let ta = a["time"].as_str().unwrap_or("~");
        let tb = b["time"].as_str().unwrap_or("~");
        ta.cmp(tb)
    });
    (
        serde_json::to_string(&convo).unwrap_or_else(|_| "[]".into()),
        serde_json::to_string(&checks).unwrap_or_else(|_| "[]".into()),
    )
}

/// 从缓存 diff 文本算 Files 标签页清单（JSON：[{path, adds, dels}]）。
fn files_json_from_diff(diff: &str) -> String {
    let files: Vec<serde_json::Value> = diffview::split_files(diff)
        .into_iter()
        .map(|(path, text)| {
            let adds = text.lines().filter(|l| l.starts_with('+')).count();
            let dels = text.lines().filter(|l| l.starts_with('-')).count();
            serde_json::json!({"path": path, "adds": adds, "dels": dels})
        })
        .collect();
    serde_json::to_string(&files).unwrap_or_else(|_| "[]".into())
}

/// MdBlockOut 块 → Slint MdBlock 模型。
fn md_blocks_of(blocks: Vec<pr_desk_rs::markdown::MdBlockOut>) -> ModelRc<MdBlock> {
    let blocks: Vec<MdBlock> = blocks
        .into_iter()
        .map(|b| MdBlock {
            kind: b.kind.into(),
            text: b.text.into(),
            lang: b.lang.into(),
            depth: b.depth,
            cols: b.cols,
            cells: ModelRc::new(VecModel::from(
                b.cells.into_iter().map(slint::SharedString::from).collect::<Vec<_>>(),
            )),
        })
        .collect();
    ModelRc::new(VecModel::from(blocks))
}

/// 评论正文 Markdown → MdBlock 模型（会话 tab 按块渲染；text 字段仍保留原文兼容）。
fn md_blocks(md: &str) -> ModelRc<MdBlock> {
    md_blocks_of(pr_desk_rs::markdown::parse_markdown(md))
}

/// diff 文本 → DiffFile 模型（前 3 个文件展开）。
/// 在 UI 线程调用（Slint 模型不跨线程）；DiffView 依赖模型整体替换。
fn build_diff_files(diff: &str) -> Vec<DiffFile> {
    diffview::split_files(diff)
        .into_iter()
        .enumerate()
        .map(|(i, (path, text))| DiffFile {
            path: path.into(),
            lines: text
                .lines()
                .map(|l| DiffLine {
                    kind: match diffview::classify_line(l) {
                        diffview::LineKind::Add => "add",
                        diffview::LineKind::Del => "del",
                        diffview::LineKind::Hunk => "hunk",
                        diffview::LineKind::Meta => "meta",
                        diffview::LineKind::Ctx => "ctx",
                    }
                    .into(),
                    text: l.into(),
                })
                .collect::<Vec<_>>()
                .as_slice()
                .into(),
            expanded: i < 3,
        })
        .collect()
}

// ── 后台 worker ──

/// subject.url（API URL）→ 浏览器可打开的 HTML URL：
/// `api.github.com/repos/` → `github.com/`，`/pulls/` → `/pull/`（issues 路径不变）。
fn notif_html_url(api_url: &str) -> String {
    api_url
        .replace("api.github.com/repos/", "github.com/")
        .replace("/pulls/", "/pull/")
}

/// 从 HTML URL 末段解析 PR/Issue 编号（…/pull/123 → 123）；非数字末段返回 None。
fn notif_url_number(html_url: &str) -> Option<i64> {
    html_url.rsplit('/').next()?.parse().ok()
}

/// 拉取 GitHub 通知并回送 UI（无 token 时静默跳过：通知接口必须鉴权）。
async fn fetch_inbox(gh: &github::GitHub, app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let (has_token, all) = {
        let a = app.lock().unwrap();
        (a.cfg.token().is_some(), a.inbox_all)
    };
    tracing::info!("fetch_inbox: has_token={} all={}", has_token, all);
    if !has_token {
        return;
    }
    match gh.fetch_notifications(all).await {
        Ok(ns) => {
            tracing::info!("fetch_inbox: got {} notifications", ns.len());
            let rows: Vec<serde_json::Value> = ns
                .into_iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.id, "repo": n.repo, "title": n.title, "kind": n.kind,
                        "reason": n.reason, "updated_at": n.updated_at, "unread": n.unread,
                        "url": notif_html_url(&n.url),
                    })
                })
                .collect();
            let _ = msg_tx.send(Msg::InboxRows(rows));
        }
        Err(e) => {
            let _ = msg_tx.send(Msg::Status(format!("通知拉取失败: {e}")));
        }
    }
}

/// 单个预评审任务体：评分 + OCR 行级评论两段式流程，结果入库并回送消息。
/// 由 Cmd::RunReview 臂在并发槽位内 tokio::spawn 调用（GitHub 客户端每任务自建）。
async fn run_review_task(
    cfg: &Config,
    llm: &LlmSettings,
    g: &github::GitHub,
    app: &Arc<Mutex<App>>,
    msg_tx: &mpsc::Sender<Msg>,
    repo: &str,
    number: i64,
) {
    let _ = msg_tx.send(Msg::ReviewProgress {
        repo: repo.to_string(),
        number,
        text: format!("开始评审 {repo} #{number}…"),
    });
    // 进度回调 → channel（状态栏 + 面板进度，见 Msg::ReviewProgress）
    let tx = msg_tx.clone();
    let (r2, n2) = (repo.to_string(), number);
    let mut progress = move |text: String| {
        let _ = tx.send(Msg::ReviewProgress { repo: r2.clone(), number: n2, text });
    };
    match reviewer::run_review(llm, g, repo, number, &mut progress).await {
        Ok(mut v) => {
            // 第二段：OCR 行级评论（评分已拿到，OCR 失败不拖垮整单）
            match g.fetch_pr_meta(repo, number).await {
                Ok(meta) => {
                    match ocr::run_ocr_review(cfg, g, repo, number, &meta, &mut progress).await {
                        Ok(outcome) => {
                            let _ = msg_tx.send(Msg::ReviewProgress {
                                repo: repo.to_string(),
                                number,
                                text: format!(
                                    "OCR 完成：{} 条评论（{} 个文件）",
                                    outcome.comments.len(),
                                    outcome.files_reviewed
                                ),
                            });
                            v["comments"] = serde_json::Value::Array(outcome.comments);
                        }
                        Err(e) => {
                            let _ = msg_tx.send(Msg::Status(format!("OCR 明细获取失败：{e}")));
                        }
                    }
                }
                Err(e) => {
                    let _ = msg_tx.send(Msg::Status(format!("OCR 明细获取失败：{e}")));
                }
            }
            let ok = { app.lock().unwrap().db.upsert_review(&v).is_ok() };
            if ok {
                let _ = msg_tx.send(Msg::ReviewDone { repo: repo.to_string(), number });
            } else {
                let _ = msg_tx.send(Msg::ReviewFailed {
                    repo: repo.to_string(),
                    number,
                    err: "评审结果入库失败".into(),
                });
            }
        }
        Err(e) => {
            let _ = msg_tx.send(Msg::ReviewFailed {
                repo: repo.to_string(),
                number,
                err: format!("{e}"),
            });
        }
    }
}

/// 列表来源过滤：sync 启用时只显示服务器插件数据库跟踪的 PR + 手动预评审加的；
/// 本地模式显示全部（含 GitHub 火hose 拉下来的）。
fn list_sources(cfg: &Config) -> &'static [&'static str] {
    if cfg.sync_token.is_empty() {
        &[]
    } else {
        &["server", "inbox"]
    }
}

/// 本地模式刷新：拉配置仓库的全部 open PR upsert 进列表库（GitHub 火hose）。
/// sync 启用时不走这条路——列表以服务器插件数据库为准。
async fn refresh_from_github(gh: &github::GitHub, app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let repos = app.lock().unwrap().cfg.repos.clone();
    let mut total = 0;
    for repo in &repos {
        match gh.fetch_open_prs(repo).await {
            Ok(prs) => {
                total += prs.len();
                let a = app.lock().unwrap();
                for p in prs {
                    let _ = a.db.upsert_pr(&serde_json::json!({
                        "repo": p.repo, "number": p.number, "title": p.title,
                        "author": p.author, "url": p.url, "head_sha": p.head_sha,
                        "status": "pending", "updated_at": db::now(),
                    }));
                }
            }
            Err(e) => {
                let _ = msg_tx.send(Msg::Status(format!("拉取 {repo} 失败: {e}")));
            }
        }
    }
    reload_rows(app, msg_tx);
    let _ = msg_tx.send(Msg::Status(format!("GitHub 刷新完成：{total} 个 open PR")));
}

/// 一次 sync_pull → 本地应用 → outbox/reviews push 流程（Cmd::Sync 与 sync 模式下的
/// RefreshGithub/ReloadConfig 共用）。sync 未启用时静默跳过。
async fn do_sync(app: &Arc<Mutex<App>>, sync_client: &sync::SyncClient, msg_tx: &mpsc::Sender<Msg>) {
    if !sync_client.enabled() {
        // 本地模式：静默跳过，状态栏由 update_status_bar 显示「本地模式」
        return;
    }
    // 不跨 await 持有 DB 锁：HTTP 与 DB 应用分步，各自短时加锁
    let result: anyhow::Result<((usize, usize, usize), sync::PushSummary)> = async {
        let since = { app.lock().unwrap().db.get_cursor()? };
        let body = sync_client.pull_body(since).await?;
        let pulled = {
            let a = app.lock().unwrap();
            sync::apply_pull(&a.db, &body)?
        };
        let outbox = { app.lock().unwrap().db.outbox_drain()? };
        let (updates, ids) = sync::merge_outbox(&outbox);
        let reviews_new = { app.lock().unwrap().db.unpushed_reviews()? };
        let mut pushed = sync::PushSummary::default();
        if !updates.is_empty() || !reviews_new.is_empty() {
            // 推送成功后才标记 reviews 已推（失败则下轮重推）
            let review_keys: Vec<(String, i64, String)> = reviews_new
                .iter()
                .map(|v| {
                    (
                        v["repo"].as_str().unwrap_or("").to_string(),
                        v["number"].as_i64().unwrap_or(0),
                        v["head_sha"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect();
            let resp = sync_client.push_body(updates, reviews_new).await?;
            let a = app.lock().unwrap();
            pushed = sync::apply_push_result(&a.db, &resp)?;
            for id in ids {
                a.db.outbox_remove(id)?;
            }
            a.db.mark_reviews_pushed(&review_keys)?;
        }
        Ok((pulled, pushed))
    }
    .await;
    match result {
        Ok(((p, r, t), push)) => {
            // 记入「已同步 X 前」共享状态（状态栏组合读取）
            let last_sync = app.lock().unwrap().last_sync.clone();
            sync::mark_synced(&last_sync);
            let _ = msg_tx.send(Msg::Status(format!(
                "已同步服务器列表：拉取 {p} PR/{r} 评审/{t} 待办；推送 {} 条（冲突 {}）",
                push.applied, push.conflicts
            )));
            reload_rows(app, msg_tx);
        }
        Err(e) => {
            let _ = msg_tx.send(Msg::Status(format!("同步失败: {e}")));
        }
    }
}

async fn worker(app: Arc<Mutex<App>>, rx: mpsc::Receiver<Cmd>, msg_tx: mpsc::Sender<Msg>) {
    let _ = MSG_TX.set(msg_tx.clone());

    let token = {
        let cfg = app.lock().unwrap().cfg.clone();
        cfg.token()
    };
    let mut gh = match github::GitHub::new(token) {
        Ok(g) => Some(g),
        Err(e) => {
            let _ = msg_tx.send(Msg::Status(format!("GitHub 客户端初始化失败: {e}")));
            None
        }
    };
    // 配额共享句柄交给 App（UI 状态栏读取 X-RateLimit-Remaining）
    if let Some(g) = &gh {
        app.lock().unwrap().rate_limit = g.rate_limit_handle();
    }
    let mut sync_client = {
        let cfg = app.lock().unwrap().cfg.clone();
        sync::SyncClient::new(&cfg)
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::RefreshGithub => {
                if let Some(gh) = &gh {
                    if sync_client.enabled() {
                        // sync 模式：列表来自服务器插件数据库（sync_pull）+ 手动预评审，
                        // 不再把配置仓库的全部 open PR 灌进列表库；通知仍从 GitHub 拉
                        fetch_inbox(gh, &app, &msg_tx).await;
                        do_sync(&app, &sync_client, &msg_tx).await;
                    } else {
                        refresh_from_github(gh, &app, &msg_tx).await;
                        // 顺便拉一次通知
                        fetch_inbox(gh, &app, &msg_tx).await;
                    }
                }
            }
            Cmd::Sync => {
                // 通知与同步服务器无关，先顺手拉一次
                if let Some(g) = &gh {
                    fetch_inbox(g, &app, &msg_tx).await;
                }
                do_sync(&app, &sync_client, &msg_tx).await;
            }
            Cmd::FetchDiff { repo, number } => {
                if let Some(gh) = &gh {
                    match gh.fetch_diff(&repo, number).await {
                        Ok(diff) => {
                            {
                                let a = app.lock().unwrap();
                                let _ = a.db.put_diff(&repo, number, &diff);
                            }
                            let _ = msg_tx.send(Msg::DiffRaw(diff));
                            // diff 响应也带配额头，状态栏重组刷新配额
                            let _ = msg_tx.send(Msg::StatusBarRefresh);
                        }
                        Err(e) => {
                            let _ = msg_tx.send(Msg::Status(format!("diff 拉取失败: {e}")));
                        }
                    }
                }
            }
            Cmd::FetchInbox => {
                if let Some(gh) = &gh {
                    fetch_inbox(gh, &app, &msg_tx).await;
                }
            }
            Cmd::MarkNotifRead(thread_id) => {
                if let Some(gh) = &gh {
                    match gh.mark_notification_read(&thread_id).await {
                        Ok(()) => fetch_inbox(gh, &app, &msg_tx).await,
                        Err(e) => {
                            let _ = msg_tx.send(Msg::Status(format!("标记已读失败: {e}")));
                        }
                    }
                    let _ = msg_tx.send(Msg::StatusBarRefresh);
                }
            }
            Cmd::FetchNotifDetail { repo, number, kind } => {
                if let Some(gh) = &gh {
                    // 会话对 PR/Issue 都有；Files/Diff 仅 PullRequest
                    let convo = match gh.fetch_pr_convo(&repo, number).await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = msg_tx.send(Msg::Status(format!("会话拉取失败: {e}")));
                            Vec::new()
                        }
                    };
                    let (files, diff) = if kind == "PullRequest" {
                        let files = match gh.fetch_pr_files(&repo, number).await {
                            Ok(f) => f,
                            Err(e) => {
                                let _ = msg_tx.send(Msg::Status(format!("文件列表拉取失败: {e}")));
                                Vec::new()
                            }
                        };
                        let diff = match gh.fetch_diff(&repo, number).await {
                            Ok(d) => d,
                            Err(e) => {
                                let _ = msg_tx.send(Msg::Status(format!("diff 拉取失败: {e}")));
                                String::new()
                            }
                        };
                        (files, diff)
                    } else {
                        (Vec::new(), String::new())
                    };
                    let _ = msg_tx.send(Msg::NotifDetail {
                        convo_json: serde_json::to_string(&convo).unwrap_or_else(|_| "[]".into()),
                        files_json: serde_json::to_string(&files).unwrap_or_else(|_| "[]".into()),
                        diff,
                    });
                    let _ = msg_tx.send(Msg::StatusBarRefresh);
                }
            }
            Cmd::MarkAllNotifsRead => {                if let Some(gh) = &gh {
                    match gh.mark_all_notifications_read().await {
                        Ok(()) => {
                            let _ = msg_tx.send(Msg::Status("通知已全部标记已读".into()));
                            fetch_inbox(gh, &app, &msg_tx).await;
                        }
                        Err(e) => {
                            let _ = msg_tx.send(Msg::Status(format!("全部已读失败: {e}")));
                        }
                    }
                    let _ = msg_tx.send(Msg::StatusBarRefresh);
                }
            }
            Cmd::AddNotifReview { repo, number, title, url } => {
                // 已存在则跳过（upsert 会把已有行的 author/head_sha 等覆盖成空）
                let exists = {
                    let a = app.lock().unwrap();
                    a.db.pr_exists(&repo, number).unwrap_or(false)
                };
                if exists {
                    let _ = msg_tx.send(Msg::Status(format!("{repo} #{number} 已在列表中")));
                } else {
                    let ok = {
                        let a = app.lock().unwrap();
                        a.db.upsert_pr(&serde_json::json!({
                            "repo": repo, "number": number, "title": title,
                            "author": "", "url": url, "head_sha": "",
                            "status": "pending", "updated_at": db::now(),
                            "source": "inbox",
                        })).is_ok()
                    };
                    let _ = msg_tx.send(Msg::Status(if ok {
                        format!("已加入预评审：{repo} #{number}")
                    } else {
                        format!("加入预评审失败：{repo} #{number}")
                    }));
                    if ok {
                        reload_rows(&app, &msg_tx);
                    }
                }
                // 重发当前通知行：apply_msg 重新计算「已加入」标记
                let rows = app.lock().unwrap().inbox.clone();
                let _ = msg_tx.send(Msg::InboxRows(rows));
            }
            Cmd::RunReview { repo, number } => {
                let (cfg, slots) = {
                    let a = app.lock().unwrap();
                    (a.cfg.clone(), a.review_slots.clone())
                };
                // LLM 配置缺失：立即提示，不占并发槽
                let Some(llm) = LlmSettings::from_config(&cfg) else {
                    let _ = msg_tx.send(Msg::Status("请先在设置里配置 LLM API Key 和模型".into()));
                    continue;
                };
                // 并发槽：同一 PR 重入 / 超额都立即拒绝（槽位由任务内 SlotGuard 释放）
                // 注意：try_acquire 的锁守卫随 let 语句结束即释放，match 臂内可再次加锁
                let acquired = slots.lock().unwrap().try_acquire(&repo, number);
                match acquired {
                    Ok(()) => {}
                    Err(SlotError::AlreadyRunning) => {
                        let _ = msg_tx.send(Msg::Status("该 PR 正在评审中".into()));
                        continue;
                    }
                    Err(SlotError::AtCapacity) => {
                        let _ = msg_tx.send(Msg::Status(format!(
                            "已有 {} 个评审进行中，稍后再试",
                            slots.lock().unwrap().capacity
                        )));
                        continue;
                    }
                }
                // 并行评审：spawn 后立即返回，worker 循环继续处理后续命令。
                // 每个任务自建 GitHub 客户端，结束后把配额合并回 app.rate_limit。
                let app2 = app.clone();
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    let _guard = SlotGuard { slots, repo: repo.clone(), number };
                    let rl = match github::GitHub::new(cfg.token()) {
                        Ok(g) => {
                            let h = g.rate_limit_handle();
                            run_review_task(&cfg, &llm, &g, &app2, &tx, &repo, number).await;
                            Some(h)
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::ReviewFailed {
                                repo: repo.clone(),
                                number,
                                err: format!("GitHub 客户端初始化失败: {e}"),
                            });
                            None
                        }
                    };
                    if let Some(h) = rl {
                        if let Some(n) = *h.lock().unwrap() {
                            *app2.lock().unwrap().rate_limit.lock().unwrap() = Some(n);
                        }
                    }
                });
            }
            Cmd::FetchModels { base, key } => {
                // 设置页「获取」：拉 OpenAI 兼容端点的模型列表（base/key 是弹窗里正在编辑的值）
                let base = if base.trim().is_empty() {
                    app.lock().unwrap().cfg.llm_base_url.clone()
                } else {
                    base.trim().trim_end_matches('/').to_string()
                };
                let key = if key.trim().is_empty() {
                    app.lock().unwrap().cfg.llm_key().unwrap_or_default()
                } else {
                    key.trim().to_string()
                };
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Msg::Status("正在拉取模型列表…".into()));
                    let url = format!("{base}/models");
                    let result = async {
                        let resp = reqwest::Client::new()
                            .get(&url)
                            .header("Authorization", format!("Bearer {key}"))
                            .timeout(std::time::Duration::from_secs(15))
                            .send()
                            .await?;
                        if !resp.status().is_success() {
                            anyhow::bail!("HTTP {}", resp.status());
                        }
                        let v: serde_json::Value = resp.json().await?;
                        let mut ids: Vec<String> = v["data"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        ids.sort();
                        ids.dedup();
                        Ok::<Vec<String>, anyhow::Error>(ids)
                    }
                    .await;
                    match result {
                        Ok(ids) => {
                            let n = ids.len();
                            let _ = tx.send(Msg::ModelsLoaded(ids));
                            let _ = tx.send(Msg::Status(format!("拉取到 {n} 个模型")));
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Status(format!("模型列表拉取失败：{e}")));
                        }
                    }
                });
            }
            Cmd::ReloadConfig => {
                // 设置保存后热生效：按新配置重建客户端，随后立即刷新 + 同步
                let cfg = app.lock().unwrap().cfg.clone();
                match github::GitHub::new(cfg.token()) {
                    Ok(g) => {
                        // 重建客户端后替换配额共享句柄
                        app.lock().unwrap().rate_limit = g.rate_limit_handle();
                        gh = Some(g);
                    }
                    Err(e) => {
                        let _ = msg_tx.send(Msg::Status(format!("GitHub 客户端重建失败: {e}")));
                    }
                }
                sync_client = sync::SyncClient::new(&cfg);
                let _ = msg_tx.send(Msg::Status("配置已重载".into()));
                // 复用现有流程：sync 模式同步服务器列表；本地模式拉全部 open PR
                if let Some(g) = &gh {
                    if sync_client.enabled() {
                        fetch_inbox(g, &app, &msg_tx).await;
                        do_sync(&app, &sync_client, &msg_tx).await;
                    } else {
                        refresh_from_github(g, &app, &msg_tx).await;
                        fetch_inbox(g, &app, &msg_tx).await;
                    }
                }
            }
        }
    }
}

/// 重建行模型并预取头像/组织图标（有新拉取时再次重建让 UI 拿到图片）。
/// 同步函数（DB 操作均为同步），不进入 async —— Slint 模型类型（SharedString）
/// 不是 Send，async fn 会在 await 点持有它们导致 future 非 Send。
fn reload_rows(app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let rows = {
        let a = app.lock().unwrap();
        a.db.list_prs(list_sources(&a.cfg)).unwrap_or_default()
    };
    // 头像 / 组织图标预取名单
    let mut logins = Vec::new();
    let mut orgs = Vec::new();
    for r in &rows {
        if let Some(a) = r["author"].as_str() {
            if !a.is_empty() {
                logins.push(a.to_string());
            }
        }
        orgs.extend(model::author_orgs(r));
    }
    app.lock().unwrap().rows = rows.clone();
    // serde_json 行可跨线程；Slint 模型在 UI 线程（apply_msg）装配
    let _ = msg_tx.send(Msg::Rows(rows));
    let app2 = app.clone();
    let msg_tx2 = msg_tx.clone();
    tokio::spawn(async move {
        let fetched = avatars::prefetch(logins).await + avatars::prefetch_orgs(orgs).await;
        if fetched > 0 {
            reload_rows(&app2, &msg_tx2);
        }
    });
}

// ── UI 消息应用 ──

/// 用当前 filter 过滤 app.rows 并重建 rows/groups 模型（折叠状态按组 key 从旧模型带回）。
fn rebuild_rows_ui(ui: &MainWindow, app: &Arc<Mutex<App>>) {
    // 在 UI 线程装配 Slint 模型（SharedString/Image 是 Rc，不跨线程）
    let rows_json: Vec<serde_json::Value> = {
        let a = app.lock().unwrap();
        a.view.iter().map(|&i| a.rows[i].clone()).collect()
    };
    let (rows, mut groups) = model::build_models(&rows_json);
    let old = ui.get_groups();
    for g in groups.iter_mut() {
        for gi in 0..old.row_count() {
            let og = old.row_data(gi).unwrap();
            if og.key == g.key {
                g.collapsed = og.collapsed;
                break;
            }
        }
    }
    ui.set_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_groups(ModelRc::new(VecModel::from(groups)));
}

/// 评审 tab 属性整体写入（Msg::Panel 初次装载与 Msg::ReviewDone 刷新共用）。
fn apply_review_props(ui: &MainWindow, p: &ReviewProps) {
    ui.set_d_review_has(p.has);
    ui.set_d_review_score(p.score);
    // 新版结构化数据：体量行 / 摘要 Markdown 块 / 维度分数条
    ui.set_d_review_size_line(p.size_line.clone().into());
    ui.set_d_review_summary_blocks(md_blocks_of(p.summary_blocks.clone()));
    ui.set_d_review_dimensions(ModelRc::new(VecModel::from(
        p.dimensions
            .iter()
            .map(|(label, score)| DimScore { label: label.as_str().into(), score: *score })
            .collect::<Vec<_>>(),
    )));
    ui.set_d_review_summary(p.summary.clone().into());
    ui.set_d_review_flagged(ModelRc::new(VecModel::from(
        p.flagged
            .iter()
            .map(|s| s.into())
            .collect::<Vec<slint::SharedString>>(),
    )));
    ui.set_d_review_reviewed_at(p.reviewed_at.clone().into());
    // OCR 行级评论：纯字符串行 → Slint OcrComment 模型
    ui.set_d_review_comments(ModelRc::new(VecModel::from(
        p.comments
            .iter()
            .map(|c| OcrComment {
                severity: c.severity.as_str().into(),
                category: c.category.as_str().into(),
                location: c.location.as_str().into(),
                content: c.content.as_str().into(),
            })
            .collect::<Vec<_>>(),
    )));
}

fn apply_msg(ui: &MainWindow, app: &Arc<Mutex<App>>, msg: Msg) {
    match msg {
        Msg::Status(s) => {
            app.lock().unwrap().status_msg = s;
            update_status_bar(ui, app);
        }
        Msg::StatusBarRefresh => update_status_bar(ui, app),
        Msg::ModelsLoaded(models) => {
            let v: Vec<SharedString> = models.iter().map(|m| m.as_str().into()).collect();
            ui.set_model_list(ModelRc::new(slint::VecModel::from(v)));
        }
        Msg::FilterChanged(q) => {
            {
                let mut a = app.lock().unwrap();
                a.filter = q;
                a.view = filtered_view(&a.rows, &a.filter);
            }
            rebuild_rows_ui(ui, app);
        }
        Msg::Rows(rows_json) => {
            {
                let mut a = app.lock().unwrap();
                a.total = rows_json.len();
                a.view = filtered_view(&a.rows, &a.filter);
            }
            rebuild_rows_ui(ui, app);
            update_status_bar(ui, app);
        }
        Msg::Panel { title, author, state, ci, opened, updated, body, priority, pinned, on_board, convo_json, checks_json, files_json, review } => {
            ui.set_d_title(title.into());
            ui.set_d_author(author.clone().into());
            // 头像在 UI 线程从磁盘缓存读（slint::Image 是 Rc，不过线程边界）
            ui.set_d_avatar(avatars::get_avatar(&author).unwrap_or_default());
            ui.set_d_state(state.into());
            ui.set_d_ci(ci.into());
            ui.set_d_opened(opened.into());
            ui.set_d_updated(updated.into());
            ui.set_d_body(body.into());
            ui.set_d_priority(priority);
            ui.set_d_pinned(pinned);
            ui.set_d_on_board(on_board);
            // JSON → Slint 模型（UI 线程装配）
            let convo: Vec<ConvoEntry> = serde_json::from_str::<Vec<serde_json::Value>>(&convo_json)
                .unwrap_or_default()
                .iter()
                .map(|e| ConvoEntry {
                    kind: e["kind"].as_str().unwrap_or("").into(),
                    author: e["author"].as_str().unwrap_or("").into(),
                    time: model::rel_time(e["time"].as_str().unwrap_or("")).into(),
                    text: e["text"].as_str().unwrap_or("").into(),
                    blocks: md_blocks(e["text"].as_str().unwrap_or("")),
                })
                .collect();
            ui.set_d_convo(ModelRc::new(VecModel::from(convo)));
            let checks: Vec<CheckRun> = serde_json::from_str::<Vec<serde_json::Value>>(&checks_json)
                .unwrap_or_default()
                .iter()
                .map(|c| CheckRun {
                    name: c["name"].as_str().unwrap_or("").into(),
                    conclusion: c["conclusion"].as_str().unwrap_or("").into(),
                })
                .collect();
            ui.set_d_checks(ModelRc::new(VecModel::from(checks)));
            let files: Vec<FileChange> = serde_json::from_str::<Vec<serde_json::Value>>(&files_json)
                .unwrap_or_default()
                .iter()
                .map(|f| FileChange {
                    path: f["path"].as_str().unwrap_or("").into(),
                    adds: f["adds"].as_i64().unwrap_or(0) as i32,
                    dels: f["dels"].as_i64().unwrap_or(0) as i32,
                })
                .collect();
            ui.set_d_files(ModelRc::new(VecModel::from(files)));
            // 评审 tab：装载已有评审；状态重置（避免带出上一个 PR 的 running/failed）
            apply_review_props(ui, &review);
            ui.set_d_review_status("".into());
            ui.set_d_review_progress("".into());
            ui.set_d_open(true);
        }
        Msg::ClosePanel => ui.set_d_open(false),
        Msg::ReviewProgress { repo, number, text } => {
            app.lock().unwrap().status_msg = text.clone();
            update_status_bar(ui, app);
            if panel_matches(app, &repo, number) {
                ui.set_d_review_status("running".into());
                ui.set_d_review_progress(text.into());
            }
        }
        Msg::ReviewDone { repo, number } => {
            app.lock().unwrap().status_msg = format!("评审完成：{repo} #{number}");
            update_status_bar(ui, app);
            // 评审分数进列表 Rev 列：重建行模型
            reload_rows_global(app);
            if panel_matches(app, &repo, number) {
                let rv = { app.lock().unwrap().db.get_review(&repo, number).ok().flatten() };
                apply_review_props(ui, &review_props(rv.as_ref()));
                ui.set_d_review_status("done".into());
                ui.set_d_review_progress("".into());
            }
        }
        Msg::ReviewFailed { repo, number, err } => {
            app.lock().unwrap().status_msg = format!("评审失败：{repo} #{number}：{err}");
            update_status_bar(ui, app);
            if panel_matches(app, &repo, number) {
                ui.set_d_review_status("failed".into());
            }
        }
        Msg::InboxRows(rows_json) => {
            // 存一份原始 JSON（inbox_activated 按下标取 thread id）
            app.lock().unwrap().inbox = rows_json.clone();
            // JSON → NotifRow 模型（UI 线程装配）；updated_at 换算成相对时间；
            // PullRequest 行查库计算「已加入」标记
            let rows: Vec<NotifRow> = {
                let a = app.lock().unwrap();
                rows_json
                    .iter()
                    .map(|n| {
                        let url = n["url"].as_str().unwrap_or("");
                        let number = notif_url_number(url);
                        let added = n["kind"].as_str() == Some("PullRequest")
                            && number.is_some()
                            && a.db
                                .pr_exists(n["repo"].as_str().unwrap_or(""), number.unwrap_or(0))
                                .unwrap_or(false);
                        NotifRow {
                            id: n["id"].as_str().unwrap_or("").into(),
                            repo: n["repo"].as_str().unwrap_or("").into(),
                            title: n["title"].as_str().unwrap_or("").into(),
                            kind: n["kind"].as_str().unwrap_or("").into(),
                            reason: n["reason"].as_str().unwrap_or("").into(),
                            updated: model::rel_time(n["updated_at"].as_str().unwrap_or("")).into(),
                            unread: n["unread"].as_bool().unwrap_or(false),
                            url: url.into(),
                            added,
                        }
                    })
                    .collect()
            };
            tracing::info!("InboxRows: setting {} rows to UI", rows.len());
            ui.set_inbox_rows(ModelRc::new(VecModel::from(rows)));
            // 预览面板开着时同步刷新「已加入」状态（+ 预评审后重发 InboxRows 会走到这里）
            let detail_added = {
                let a = app.lock().unwrap();
                a.inbox_detail.as_ref().map(|(repo, number, kind, _)| {
                    kind == "PullRequest" && a.db.pr_exists(repo, *number).unwrap_or(false)
                })
            };
            if let Some(added) = detail_added {
                ui.set_nd_added(added);
            }
        }
        Msg::NotifDetail { convo_json, files_json, diff } => {
            // JSON → Slint 模型（UI 线程装配，同 Msg::Panel 的模式）
            let convo: Vec<ConvoEntry> = serde_json::from_str::<Vec<serde_json::Value>>(&convo_json)
                .unwrap_or_default()
                .iter()
                .map(|e| ConvoEntry {
                    kind: "comment".into(),
                    author: e["author"].as_str().unwrap_or("").into(),
                    time: model::rel_time(e["time"].as_str().unwrap_or("")).into(),
                    text: e["text"].as_str().unwrap_or("").into(),
                    blocks: md_blocks(e["text"].as_str().unwrap_or("")),
                })
                .collect();
            ui.set_nd_convo(ModelRc::new(VecModel::from(convo)));
            let files: Vec<FileChange> = serde_json::from_str::<Vec<serde_json::Value>>(&files_json)
                .unwrap_or_default()
                .iter()
                .map(|f| FileChange {
                    path: f["path"].as_str().unwrap_or("").into(),
                    adds: f["adds"].as_i64().unwrap_or(0) as i32,
                    dels: f["dels"].as_i64().unwrap_or(0) as i32,
                })
                .collect();
            ui.set_nd_files(ModelRc::new(VecModel::from(files)));
            ui.set_nd_diff_files(ModelRc::new(VecModel::from(build_diff_files(&diff))));
            ui.set_nd_loading(false);
        }
        Msg::DiffRaw(diff) => {
            // 在 UI 线程构建 Slint 模型（DiffView 依赖整体替换）
            ui.set_diff_files(ModelRc::new(VecModel::from(build_diff_files(&diff))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notif_html_url_converts_api_url() {
        assert_eq!(
            notif_html_url("https://api.github.com/repos/vllm-project/vllm/pulls/123"),
            "https://github.com/vllm-project/vllm/pull/123"
        );
        assert_eq!(
            notif_html_url("https://api.github.com/repos/o/r/issues/45"),
            "https://github.com/o/r/issues/45"
        );
    }

    #[test]
    fn notif_url_number_parses_trailing_segment() {
        assert_eq!(notif_url_number("https://github.com/o/r/pull/123"), Some(123));
        assert_eq!(notif_url_number("https://github.com/o/r/releases"), None);
        assert_eq!(notif_url_number(""), None);
    }

    #[test]
    fn review_slots_acquire_and_release() {
        let mut s = ReviewSlots::new(2);
        assert!(s.try_acquire("o/r", 1).is_ok());
        assert!(s.is_running("o/r", 1));
        s.release("o/r", 1);
        assert!(!s.is_running("o/r", 1));
        // 释放后可再获取
        assert!(s.try_acquire("o/r", 1).is_ok());
    }

    #[test]
    fn review_slots_reject_duplicate_acquire() {
        let mut s = ReviewSlots::new(3);
        s.try_acquire("o/r", 1).unwrap();
        assert_eq!(s.try_acquire("o/r", 1), Err(SlotError::AlreadyRunning));
        // 同一 PR 不同 number / 不同 repo 不受影响
        assert!(s.try_acquire("o/r", 2).is_ok());
        assert!(s.try_acquire("a/b", 1).is_ok());
    }

    #[test]
    fn review_slots_enforce_capacity() {
        let mut s = ReviewSlots::new(3);
        s.try_acquire("o/r", 1).unwrap();
        s.try_acquire("o/r", 2).unwrap();
        s.try_acquire("o/r", 3).unwrap();
        assert_eq!(s.try_acquire("o/r", 4), Err(SlotError::AtCapacity));
        // 释放一个后腾出槽位
        s.release("o/r", 2);
        assert!(s.try_acquire("o/r", 4).is_ok());
        assert_eq!(s.try_acquire("o/r", 5), Err(SlotError::AtCapacity));
    }

    #[test]
    fn review_slots_release_unknown_is_noop() {
        let mut s = ReviewSlots::new(1);
        s.release("o/r", 99); // 未占用：不 panic
        assert!(s.try_acquire("o/r", 1).is_ok());
    }

    #[test]
    fn review_slots_duplicate_check_beats_capacity() {
        // 满槽时对重复 PR 报 AlreadyRunning 而非 AtCapacity
        let mut s = ReviewSlots::new(1);
        s.try_acquire("o/r", 1).unwrap();
        assert_eq!(s.try_acquire("o/r", 1), Err(SlotError::AlreadyRunning));
    }

    #[test]
    fn slot_guard_releases_on_drop() {
        let slots = Arc::new(Mutex::new(ReviewSlots::new(1)));
        slots.lock().unwrap().try_acquire("o/r", 1).unwrap();
        {
            let _g = SlotGuard { slots: slots.clone(), repo: "o/r".into(), number: 1 };
        }
        assert!(!slots.lock().unwrap().is_running("o/r", 1));
        assert!(slots.lock().unwrap().try_acquire("o/r", 1).is_ok());
    }

    // ── 选中态按身份（repo, number）查找 ──

    fn test_app(rows: Vec<serde_json::Value>) -> Arc<Mutex<App>> {
        let view = (0..rows.len()).collect();
        Arc::new(Mutex::new(App {
            cfg: Config::default(),
            db: Db::open_memory().unwrap(),
            rows,
            view,
            filter: String::new(),
            selected: None,
            status_msg: String::new(),
            inbox: Vec::new(),
            inbox_all: true,
            inbox_detail: None,
            total: 0,
            rate_limit: Arc::new(Mutex::new(None)),
            last_sync: sync::shared_last_sync(),
            review_slots: Arc::new(Mutex::new(ReviewSlots::new(3))),
        }))
    }

    #[test]
    fn find_row_locates_by_identity_surviving_reorder() {
        let app = test_app(vec![
            serde_json::json!({"repo": "o/r", "number": 1}),
            serde_json::json!({"repo": "o/r", "number": 2}),
        ]);
        {
            let a = app.lock().unwrap();
            assert_eq!(find_row(&a, "o/r", 2), Some(1));
            assert_eq!(find_row(&a, "o/r", 99), None);
            assert_eq!(find_row(&a, "x/y", 1), None);
        }
        // 模拟刷新后行序颠倒：身份查找仍命中同一个 PR，不会货不对板
        app.lock().unwrap().rows = vec![
            serde_json::json!({"repo": "o/r", "number": 2}),
            serde_json::json!({"repo": "o/r", "number": 1}),
        ];
        let a = app.lock().unwrap();
        assert_eq!(find_row(&a, "o/r", 2), Some(0));
        assert_eq!(find_row(&a, "o/r", 1), Some(1));
    }

    #[test]
    fn panel_matches_identity_surviving_row_removal() {
        let app = test_app(vec![serde_json::json!({"repo": "o/r", "number": 1})]);
        app.lock().unwrap().selected = Some(("o/r".into(), 1));
        assert!(panel_matches(&app, "o/r", 1));
        assert!(!panel_matches(&app, "o/r", 2));
        assert!(!panel_matches(&app, "x/y", 1));
        // 刷新后该行不在列表里：面板保持打开，身份保留，匹配继续工作
        app.lock().unwrap().rows = Vec::new();
        assert!(panel_matches(&app, "o/r", 1));
        // 关闭面板后不再匹配
        app.lock().unwrap().selected = None;
        assert!(!panel_matches(&app, "o/r", 1));
    }

    #[test]
    fn resolve_view_maps_view_index_to_identity() {
        let app = test_app(vec![
            serde_json::json!({"repo": "o/r", "number": 1, "title": "aaa"}),
            serde_json::json!({"repo": "o/r", "number": 2, "title": "bbb"}),
        ]);
        // 过滤后视图只含第二行：视图下标 0 → (o/r, 2)
        {
            let mut a = app.lock().unwrap();
            a.filter = "bbb".into();
            a.view = filtered_view(&a.rows, &a.filter);
        }
        assert_eq!(resolve_view(&app, 0), Some(("o/r".to_string(), 2)));
        assert_eq!(resolve_view(&app, 1), None);
    }

    // ── 评审展示数据扩展 ──

    #[test]
    fn size_line_formats_and_handles_missing_fields() {
        let rv = serde_json::json!({
            "size_label": "XL", "additions": 2122, "deletions": 23, "files_changed": 24,
        });
        assert_eq!(size_line(&rv), "XL（+2122/-23，24 个文件）");
        assert_eq!(size_line(&serde_json::json!({})), "");
        assert_eq!(size_line(&serde_json::json!({"size_label": "S"})), "S");
        assert_eq!(
            size_line(&serde_json::json!({"additions": 10, "deletions": 2, "files_changed": 3})),
            "（+10/-2，3 个文件）"
        );
    }

    #[test]
    fn review_props_exposes_structured_dimensions_blocks_and_size() {
        let rv = serde_json::json!({
            "score": 8.0,
            "dimensions": "{\"readability\": 8, \"security\": 6}",
            "summary": "## 结论\n整体可用",
            "size_label": "M", "additions": 100, "deletions": 5, "files_changed": 4,
        });
        let p = review_props(Some(&rv));
        assert!(p.has);
        assert_eq!(p.size_line, "M（+100/-5，4 个文件）");
        assert!(p.dimensions.contains(&("可读性".to_string(), 8.0)));
        assert!(p.dimensions.contains(&("安全".to_string(), 6.0)));
        // dimension_lines 旧字段保持可用
        assert!(!p.dimension_lines.is_empty());
        // 摘要按 Markdown 解析成块
        assert!(!p.summary_blocks.is_empty());
        assert_eq!(p.summary_blocks[0].kind, "h2");
        assert_eq!(p.summary_blocks[0].text, "结论");
        // 无评审：空态各字段为空
        let p0 = review_props(None);
        assert!(!p0.has);
        assert!(p0.dimensions.is_empty());
        assert!(p0.summary_blocks.is_empty());
        assert_eq!(p0.size_line, "");
    }
}
