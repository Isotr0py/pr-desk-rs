//! PR Desk 桌面端（第一阶段）：本地 SQLite + GitHub 拉取 + 插件同步 + Slint UI。
//! 纯逻辑在 lib（pr_desk_rs）里，`cargo test --lib` 不需要编译 Slint。

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use slint::{Model as _, ModelRc, VecModel};

slint::include_modules!();

mod avatars;
mod model;

use pr_desk_rs::config::Config;
use pr_desk_rs::db::{self, Db};
use pr_desk_rs::{diffview, github, sync};

// ── 后台命令 / 回送消息 ──

enum Cmd {
    RefreshGithub,
    Sync,
    FetchDiff { repo: String, number: i64 },
    ReloadConfig,   // 设置保存后热生效：重建客户端并立即刷新+同步
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
    },
    ClosePanel,
    // 同理：diff 以原始文本过界，DiffFile 模型在 UI 线程的 apply_msg 里构建
    DiffRaw(String),
}

struct App {
    cfg: Config,
    db: Db,
    rows: Vec<serde_json::Value>,           // 扁平数据行（selected 直接索引它）
    view: Vec<usize>,                       // 过滤后可见行 → rows 下标（UI 下标经它换算）
    filter: String,                         // 当前过滤词（空 = 全量）
    selected: Option<usize>,                // rows 下标（已从视图下标换算）
    status_msg: String,                     // 状态栏临时消息（最近一条 Status）
    total: usize,                           // 全部行数（状态栏「共 N 个 PR」）
    rate_limit: Arc<Mutex<Option<u64>>>,    // GitHub X-RateLimit-Remaining 共享状态
    last_sync: sync::SharedLastSync,        // 最近同步成功时间
}

/// 状态栏组合逻辑已内联进 update_status_bar（三段：同步/配额/总数）。
fn update_status_bar(ui: &MainWindow, app: &Arc<Mutex<App>>) {
    let a = app.lock().unwrap();
    // 状态栏三段：同步/临时消息 · API 配额 · PR 总数
    let sync_text = if !a.status_msg.is_empty() {
        a.status_msg.clone()
    } else {
        sync::last_sync_text(&a.last_sync).unwrap_or_default()
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

/// UI 传来的可见行下标 → rows 原始下标。
fn resolve_view(app: &Arc<Mutex<App>>, view_idx: usize) -> Option<usize> {
    app.lock().unwrap().view.get(view_idx).copied()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_e| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::load(&PathBuf::from("config.toml"))?;
    let data_dir = PathBuf::from("data");
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
        total: 0,
        rate_limit: Arc::new(Mutex::new(None)),
        last_sync: sync::shared_last_sync(),
    }));

    // 设置页初值 + 首启引导 + 状态栏配置状态
    ui.set_s_server(cfg.server_url.clone().into());
    ui.set_s_sync_token(cfg.sync_token.clone().into());
    ui.set_s_gh_token(cfg.github_token.clone().unwrap_or_default().into());
    ui.set_s_repos(cfg.repos.join("\n").into());
    ui.set_s_poll(cfg.poll_interval_min as i32);
    ui.set_gh_token_from_env(std::env::var("GITHUB_TOKEN").map(|t| !t.is_empty()).unwrap_or(false));
    let unconfigured = cfg.is_unconfigured();
    ui.set_onboarding(unconfigured);
    if unconfigured {
        ui.set_settings_open(true); // 首启自动打开设置
    }
    ui.set_sb_sync(config_status(&cfg).into());

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
        // 顶栏搜索框过滤词 → 重建行模型（view 下标映射在 resolve_view）
        ui.on_filter_changed(|q| send_msg(Msg::FilterChanged(q.to_string())));
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        // UI 传视图下标，先经 view 换算为 rows 原始下标
        ui.on_row_activated(move |i| {
            if let Some(idx) = resolve_view(&app, i as usize) {
                open_drawer(&app, &cmd_tx, idx);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_row_pin(move |i| {
            if let Some(idx) = resolve_view(&app, i as usize) {
                let v = toggle_of(&app, idx, "pinned");
                local_update(&app, idx, "pinned", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_row_board(move |i| {
            if let Some(idx) = resolve_view(&app, i as usize) {
                let v = toggle_of(&app, idx, "on_board");
                local_update(&app, idx, "on_board", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_row_reject(move |i| {
            let Some(idx) = resolve_view(&app, i as usize) else { return };
            let cur = {
                let a = app.lock().unwrap();
                a.rows
                    .get(idx)
                    .and_then(|r| r["status"].as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "pending".into())
            };
            let to = if cur == "rejected" { "pending" } else { "rejected" };
            local_update(&app, idx, "status", to);
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
        // 面板看板按钮：切换当前选中行的 on_board
        let app = app.clone();
        ui.on_panel_board(move || {
            let sel = app.lock().unwrap().selected;
            if let Some(i) = sel {
                let v = toggle_of(&app, i, "on_board");
                local_update(&app, i, "on_board", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_panel_set_priority(move |p| {
            let sel = app.lock().unwrap().selected;
            if let Some(i) = sel {
                local_update(&app, i, "priority", p as i64);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_panel_toggle_pin(move || {
            let sel = app.lock().unwrap().selected;
            if let Some(i) = sel {
                let v = toggle_of(&app, i, "pinned");
                local_update(&app, i, "pinned", v);
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
        let app = app.clone();
        ui.on_key_pin(move || {
            let sel = app.lock().unwrap().selected;
            if let Some(i) = sel {
                let v = toggle_of(&app, i, "pinned");
                local_update(&app, i, "pinned", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_key_priority(move |p| {
            let sel = app.lock().unwrap().selected;
            if let Some(i) = sel {
                local_update(&app, i, "priority", p as i64);
            }
        });
    }
    // ── 设置页 ──
    {
        let ui2 = ui.as_weak();
        ui.on_open_settings(move || {
            if let Some(ui) = ui2.upgrade() {
                ui.set_settings_open(true);
            }
        });
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let ui2 = ui.as_weak();
        ui.on_save_settings(move |server, sync_tok, gh_tok, repos_text, poll_s| {
            let repos: Vec<String> = repos_text
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && l.contains('/'))
                .collect();
            let poll: u64 = poll_s.trim().parse().unwrap_or(5).max(1);
            let new_cfg = Config {
                server_url: server.trim().to_string(),
                sync_token: sync_tok.trim().to_string(),
                github_token: Some(gh_tok.trim().to_string()).filter(|t| !t.is_empty()),
                repos,
                poll_interval_min: poll,
            };
            if let Err(e) = new_cfg.save(&PathBuf::from("config.toml")) {
                send_msg(Msg::Status(format!("配置保存失败: {e}")));
                return;
            }
            {
                let mut a = app.lock().unwrap();
                a.cfg = new_cfg.clone();
                a.status_msg = config_status(&new_cfg);
            }
            if let Some(ui) = ui2.upgrade() {
                ui.set_onboarding(new_cfg.is_unconfigured());
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

fn toggle_of(app: &Arc<Mutex<App>>, idx: usize, field: &str) -> i64 {
    let a = app.lock().unwrap();
    a.rows
        .get(idx)
        .and_then(|r| r[field].as_i64())
        .map(|v| if v == 0 { 1 } else { 0 })
        .unwrap_or(1)
}

fn local_update<V: Into<serde_json::Value>>(app: &Arc<Mutex<App>>, idx: usize, field: &str, value: V) {
    let a = app.lock().unwrap();
    if let Some(r) = a.rows.get(idx) {
        let repo = r["repo"].as_str().unwrap_or("").to_string();
        let number = r["number"].as_i64().unwrap_or(0);
        if a.db.local_pr_update(&repo, number, field, &value.into()).is_ok() {
            drop(a);
            reload_rows_global(app);
            send_msg(Msg::Status(format!("已更新 {field}（待同步）")));
        }
    }
}

fn reload_rows_global(app: &Arc<Mutex<App>>) {
    let rows = {
        let a = app.lock().unwrap();
        a.db.list_prs().unwrap_or_default()
    };
    app.lock().unwrap().rows = rows.clone();
    // serde_json 行可跨线程；Slint 模型在 UI 线程（apply_msg）装配
    send_msg(Msg::Rows(rows));
}

// ── 详情抽屉 ──

fn open_drawer(app: &Arc<Mutex<App>>, cmd_tx: &mpsc::Sender<Cmd>, idx: usize) {
    let (repo, number, priority) = {
        let mut a = app.lock().unwrap();
        a.selected = Some(idx);
        match a.rows.get(idx) {
            Some(r) => (
                r["repo"].as_str().unwrap_or("").to_string(),
                r["number"].as_i64().unwrap_or(0),
                r["priority"].as_i64().unwrap_or(0) as i32,
            ),
            None => return,
        }
    };

    let (review, diff_cached, row) = {
        let a = app.lock().unwrap();
        (
            a.db.get_review(&repo, number).ok().flatten(),
            a.db.get_diff(&repo, number).ok().flatten(),
            a.rows.get(idx).cloned(),
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
    });

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
                    reload_rows(&app, &msg_tx);
                    let _ = msg_tx.send(Msg::Status(format!("GitHub 刷新完成：{total} 个 open PR")));
                }
            }
            Cmd::Sync => {
                if !sync_client.enabled() {
                    let _ = msg_tx.send(Msg::Status("sync 未启用（sync_token 为空）".into()));
                    continue;
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
                    let mut pushed = sync::PushSummary::default();
                    if !updates.is_empty() {
                        let resp = sync_client.push_body(updates).await?;
                        let a = app.lock().unwrap();
                        pushed = sync::apply_push_result(&a.db, &resp)?;
                        for id in ids {
                            a.db.outbox_remove(id)?;
                        }
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
                            "同步完成：拉取 {p} PR/{r} 评审/{t} 待办；推送 {} 条（冲突 {}）",
                            push.applied, push.conflicts
                        )));
                        reload_rows(&app, &msg_tx);
                    }
                    Err(e) => {
                        let _ = msg_tx.send(Msg::Status(format!("同步失败: {e}")));
                    }
                }
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
                // 复用现有流程：直接内联一次刷新 + 同步
                if let Some(g) = &gh {
                    let repos = cfg.repos.clone();
                    let mut total = 0;
                    for repo in &repos {
                        match g.fetch_open_prs(repo).await {
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
                    reload_rows(&app, &msg_tx);
                    let _ = msg_tx.send(Msg::Status(format!("GitHub 刷新完成：{total} 个 open PR")));
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
        a.db.list_prs().unwrap_or_default()
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

fn apply_msg(ui: &MainWindow, app: &Arc<Mutex<App>>, msg: Msg) {
    match msg {
        Msg::Status(s) => {
            app.lock().unwrap().status_msg = s;
            update_status_bar(ui, app);
        }
        Msg::StatusBarRefresh => update_status_bar(ui, app),
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
        Msg::Panel { title, author, state, ci, opened, updated, body, priority, pinned, on_board, convo_json, checks_json, files_json } => {
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
            ui.set_d_open(true);
        }
        Msg::ClosePanel => ui.set_d_open(false),
        Msg::DiffRaw(diff) => {
            // 在 UI 线程构建 Slint 模型（DiffView 依赖整体替换）
            ui.set_diff_files(ModelRc::new(VecModel::from(build_diff_files(&diff))));
        }
    }
}
