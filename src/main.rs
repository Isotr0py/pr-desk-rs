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
}

enum Msg {
    Status(String),
    // Slint 模型类型（SharedString/Image 基于 Rc）不能跨线程：
    // 行数据以 serde_json（Send）过界，PrListRow/PrGroup 在 UI 线程装配。
    Rows(Vec<serde_json::Value>),
    Drawer {
        title: String,
        subtitle: String,
        body: String,
        convo: String,
        checks: String,
        files: String,
        priority: i32,
    },
    CloseDrawer,
    // 同理：diff 以原始文本过界，DiffFile 模型在 UI 线程的 apply_msg 里构建
    DiffRaw(String),
}

struct App {
    cfg: Config,
    db: Db,
    rows: Vec<serde_json::Value>,           // 扁平数据行（selected 直接索引它）
    selected: Option<usize>,
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
    ui.set_diff_hint("选择 PR 后在此查看 diff".into());
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();

    let app = Arc::new(Mutex::new(App {
        cfg: cfg.clone(),
        db,
        rows: Vec::new(),
        selected: None,
    }));

    // ── 后台工作线程（tokio runtime） ──
    {
        let app = app.clone();
        let msg_tx = msg_tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(worker(app, cmd_rx, msg_tx));
        });
    }

    // 自动同步定时器
    {
        let cmd_tx = cmd_tx.clone();
        let interval = cfg.poll_interval_min.max(1);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(interval * 60));
            if cmd_tx.send(Cmd::Sync).is_err() {
                break;
            }
        });
    }

    // ── 消息回 UI ──
    {
        let weak = ui.as_weak();
        std::thread::spawn(move || loop {
            match msg_rx.recv() {
                Ok(msg) => {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            apply_msg(&ui, msg);
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
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_row_activated(move |i| open_drawer(&app, &cmd_tx, i as usize));
    }
    {
        let app = app.clone();
        ui.on_row_pin(move |i| {
            let v = toggle_of(&app, i as usize, "pinned");
            local_update(&app, i as usize, "pinned", v);
        });
    }
    {
        let app = app.clone();
        ui.on_row_board(move |i| {
            let v = toggle_of(&app, i as usize, "on_board");
            local_update(&app, i as usize, "on_board", v);
        });
    }
    {
        let app = app.clone();
        ui.on_row_reject(move |i| {
            let cur = {
                let a = app.lock().unwrap();
                a.rows
                    .get(i as usize)
                    .and_then(|r| r["status"].as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "pending".into())
            };
            let to = if cur == "rejected" { "pending" } else { "rejected" };
            local_update(&app, i as usize, "status", to);
        });
    }
    ui.on_drawer_close(|| send_msg(Msg::CloseDrawer));
    {
        let app = app.clone();
        ui.on_drawer_reject(move || {
            // 抽屉的不采纳作用于当前选中行
            let sel = app.lock().unwrap().selected;
            if let Some(i) = sel {
                let cur = {
                    let a = app.lock().unwrap();
                    a.rows
                        .get(i)
                        .and_then(|r| r["status"].as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "pending".into())
                };
                let to = if cur == "rejected" { "pending" } else { "rejected" };
                local_update(&app, i, "status", to);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_drawer_set_priority(move |p| {
            let sel = app.lock().unwrap().selected;
            if let Some(i) = sel {
                local_update(&app, i, "priority", p as i64);
            }
        });
    }
    {
        // 抽屉「重新评审」：桌面端无远程重评审通道（phase 2），先提示
        ui.on_drawer_requeue(|| {
            send_msg(Msg::Status("桌面端暂不支持远程重新评审（phase 2 接入插件 API）".into()));
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
    let (repo, number, subtitle, priority) = {
        let mut a = app.lock().unwrap();
        a.selected = Some(idx);
        match a.rows.get(idx) {
            Some(r) => (
                r["repo"].as_str().unwrap_or("").to_string(),
                r["number"].as_i64().unwrap_or(0),
                format!(
                    "{}#{} · {}{}",
                    r["repo"].as_str().unwrap_or(""),
                    r["number"].as_i64().unwrap_or(0),
                    r["author"].as_str().unwrap_or("?"),
                    if r["role"].as_str() == Some("author") { " · 我提的" } else { "" }
                ),
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

    // 会话 / Checks / Files 简易文本（数据模型里有啥用啥）
    let (convo, checks) = row
        .as_ref()
        .map(|r| drawer_extra_texts(r))
        .unwrap_or_default();
    let files = match &diff_cached {
        Some(d) => files_text_from_diff(d),
        None => String::new(),
    };

    send_msg(Msg::Drawer {
        title: row
            .as_ref()
            .and_then(|r| r["title"].as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{repo} #{number}")),
        subtitle,
        body,
        convo,
        checks,
        files,
        priority,
    });

    match diff_cached {
        Some(d) => send_msg(Msg::DiffRaw(d)),
        None => {
            let _ = cmd_tx.send(Cmd::FetchDiff { repo, number });
        }
    }
}

/// extras_detail / recent_commits → 会话与 Checks 的简易文本。
fn drawer_extra_texts(r: &serde_json::Value) -> (String, String) {
    let mut convo = String::new();
    let mut checks = String::new();
    if let Some(detail_str) = r["extras_detail"].as_str() {
        if let Ok(detail) = serde_json::from_str::<serde_json::Value>(detail_str) {
            if let Some(rvs) = detail["reviews"].as_array() {
                for rv in rvs {
                    let state = rv["state"].as_str().unwrap_or("");
                    let body = rv["body"].as_str().unwrap_or("");
                    convo.push_str(&format!(
                        "💬 {}（{}）{}{}\n",
                        rv["author"].as_str().unwrap_or("?"),
                        state,
                        if body.is_empty() { "" } else { "：" },
                        body
                    ));
                }
            }
            if let Some(runs) = detail["check_runs"].as_array() {
                for c in runs {
                    checks.push_str(&format!(
                        "{} {}（{}）\n",
                        c["name"].as_str().unwrap_or("?"),
                        c["conclusion"].as_str().unwrap_or(""),
                        c["status"].as_str().unwrap_or("")
                    ));
                }
            }
            // commits 带日期（extras_detail.commits）
            if let Some(cs) = detail["commits"].as_array() {
                for c in cs {
                    convo.push_str(&format!(
                        "📦 {}\n",
                        c["title"].as_str().unwrap_or("")
                    ));
                }
            }
        }
    }
    // 无 extras_detail 时退回 recent_commits 标题列表
    if convo.is_empty() {
        if let Some(rc) = r["recent_commits"].as_str() {
            if let Ok(cs) = serde_json::from_str::<Vec<String>>(rc) {
                for c in cs {
                    convo.push_str(&format!("📦 {c}\n"));
                }
            }
        }
    }
    (convo, checks)
}

/// 从缓存 diff 文本算 Files 标签页的简易清单（路径 + 增删行数）。
fn files_text_from_diff(diff: &str) -> String {
    let mut out = String::new();
    for (path, text) in diffview::split_files(diff) {
        let adds = text.lines().filter(|l| l.starts_with('+')).count();
        let dels = text.lines().filter(|l| l.starts_with('-')).count();
        out.push_str(&format!("{path}  +{adds} -{dels}\n"));
    }
    out
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

    let token = Config::github_token();
    let gh = match github::GitHub::new(token) {
        Ok(g) => Some(g),
        Err(e) => {
            let _ = msg_tx.send(Msg::Status(format!("GitHub 客户端初始化失败: {e}")));
            None
        }
    };
    let sync_client = {
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
                        }
                        Err(e) => {
                            let _ = msg_tx.send(Msg::Status(format!("diff 拉取失败: {e}")));
                        }
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

fn apply_msg(ui: &MainWindow, msg: Msg) {
    match msg {
        Msg::Status(s) => ui.set_status_text(s.into()),
        Msg::Rows(rows_json) => {
            // 在 UI 线程装配 Slint 模型（SharedString/Image 是 Rc，不跨线程）
            let (rows, mut groups) = model::build_models(&rows_json);
            // 折叠状态持久化：按组 key 从旧模型带回
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
        Msg::Drawer { title, subtitle, body, convo, checks, files, priority } => {
            ui.set_d_title(title.into());
            ui.set_d_subtitle(subtitle.into());
            ui.set_d_body(body.into());
            ui.set_d_convo(convo.into());
            ui.set_d_checks(checks.into());
            ui.set_d_files(files.into());
            ui.set_d_priority(priority);
            ui.set_d_open(true);
        }
        Msg::CloseDrawer => ui.set_d_open(false),
        Msg::DiffRaw(diff) => {
            // 在 UI 线程构建 Slint 模型（DiffView 依赖整体替换）
            ui.set_diff_files(ModelRc::new(VecModel::from(build_diff_files(&diff))));
        }
    }
}
