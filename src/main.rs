//! PR Desk 桌面端（第一阶段）：本地 SQLite + GitHub 拉取 + 插件同步 + Slint UI。
//! 纯逻辑在 lib（pr_desk_rs）里，`cargo test --lib` 不需要编译 Slint。

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use slint::{ModelRc, VecModel};

slint::include_modules!();

use pr_desk_rs::config::Config;
use pr_desk_rs::db::{self, Db};
use pr_desk_rs::{build_sections, diffview, github, sync};

// ── 后台命令 / 回送消息 ──

enum Cmd {
    RefreshGithub,
    Sync,
    FetchDiff { repo: String, number: i64 },
}

enum Msg {
    Status(String),
    Rows(Vec<PrRow>),
    Detail {
        title: String,
        meta: String,
        score: String,
        summary: String,
    },
    Diff(Vec<DiffLine>),
    CloseDetail,
}

struct App {
    cfg: Config,
    db: Db,
    rows: Vec<serde_json::Value>,
    /// ListView 模型索引 → rows 数据索引（组头为 None）
    model_map: Vec<Option<usize>>,
    selected: Option<usize>,
}

impl App {
    fn selected_row(&self) -> Option<&serde_json::Value> {
        self.selected.and_then(|i| self.rows.get(i))
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::load(&PathBuf::from("config.toml"))?;
    let data_dir = PathBuf::from("data");
    std::fs::create_dir_all(&data_dir)?;
    let db = Db::open(&data_dir.join("pr-desk.db"))?;

    let ui = MainWindow::new()?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();

    let app = Arc::new(Mutex::new(App { cfg: cfg.clone(), db, rows: Vec::new(), model_map: Vec::new(), selected: None }));

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
        ui.on_row_clicked(move |i| select_row(&app, &cmd_tx, i as usize));
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_key_pressed(move |text| handle_key(&app, &cmd_tx, &text));
    }

    // 启动：初始刷新 + 同步
    let _ = cmd_tx.send(Cmd::RefreshGithub);
    let _ = cmd_tx.send(Cmd::Sync);

    ui.run()?;
    Ok(())
}

// ── 行选择 / 详情 ──

fn select_row(app: &Arc<Mutex<App>>, cmd_tx: &mpsc::Sender<Cmd>, model_idx: usize) {
    let (repo, number, meta, data_idx) = {
        let mut a = app.lock().unwrap();
        // ListView 的索引含组头行，经 model_map 映射到数据行
        let data_idx = match a.model_map.get(model_idx).copied().flatten() {
            Some(i) => i,
            None => return, // 组头行不可选
        };
        a.selected = Some(data_idx);
        match a.rows.get(data_idx) {
            Some(r) => (
                r["repo"].as_str().unwrap_or("").to_string(),
                r["number"].as_i64().unwrap_or(0),
                format!(
                    "作者: {}{}",
                    r["author"].as_str().unwrap_or("?"),
                    if r["role"].as_str() == Some("author") { "（我提的）" } else { "" }
                ),
                data_idx,
            ),
            None => return,
        }
    };
    // 选中高亮：重建行模型并标记选中行
    {
        let a = app.lock().unwrap();
        let (mut model, _map) = build_row_model(&a.rows);
        for (mi, m) in model.iter_mut().enumerate() {
            m.selected = a.model_map.get(mi).copied().flatten() == Some(data_idx);
        }
        send_msg(Msg::Rows(model));
    }
    let (review, diff_cached) = {
        let a = app.lock().unwrap();
        (
            a.db.get_review(&repo, number).ok().flatten(),
            a.db.get_diff(&repo, number).ok().flatten(),
        )
    };
    if let Some(rv) = &review {
        let score_text = rv["score"]
            .as_f64()
            .map(|s| format!("评分 {s:.1}/10"))
            .unwrap_or_else(|| "评审解析失败".into());
        send_msg(Msg::Detail {
            title: format!("{repo} #{number}"),
            meta,
            score: score_text,
            summary: rv["summary"].as_str().unwrap_or("").to_string(),
        });
    } else {
        send_msg(Msg::Detail {
            title: format!("{repo} #{number}"),
            meta,
            score: "尚未评审".into(),
            summary: String::new(),
        });
    }
    match diff_cached {
        Some(d) => send_msg(Msg::Diff(classify_diff(&d))),
        None => {
            let _ = cmd_tx.send(Cmd::FetchDiff { repo, number });
        }
    }
}

// 全局发信器（worker 启动时注册；UI 线程的即时反馈也用同一通道回到事件循环）
static MSG_TX: std::sync::OnceLock<mpsc::Sender<Msg>> = std::sync::OnceLock::new();

fn send_msg(msg: Msg) {
    if let Some(tx) = MSG_TX.get() {
        let _ = tx.send(msg);
    }
}

fn classify_diff(diff: &str) -> Vec<DiffLine> {
    diff.lines()
        .map(|l| DiffLine {
            text: l.into(),
            kind: match diffview::classify_line(l) {
                diffview::LineKind::Ctx => 0,
                diffview::LineKind::Add => 1,
                diffview::LineKind::Del => 2,
                diffview::LineKind::Hunk => 3,
                diffview::LineKind::Meta => 4,
            },
        })
        .collect()
}

// ── 快捷键：j/k 上下、Enter 详情、Esc 关、p 置顶、1/2/3 优先级 ──

fn handle_key(app: &Arc<Mutex<App>>, cmd_tx: &mpsc::Sender<Cmd>, text: &str) {
    match text {
        "j" | "k" => {
            // 在数据行空间移动（不含组头），再映射回模型索引
            let mut a = app.lock().unwrap();
            if a.rows.is_empty() {
                return;
            }
            let max = a.rows.len() as i64;
            let cur = a.selected.map(|i| i as i64).unwrap_or(if text == "j" { -1 } else { max });
            let next = (if text == "j" { cur + 1 } else { cur - 1 }).clamp(0, max - 1) as usize;
            a.selected = Some(next);
            let model_idx = a.model_map.iter().position(|m| *m == Some(next));
            drop(a);
            if let Some(mi) = model_idx {
                select_row(app, cmd_tx, mi);
            }
        }
        "\n" | "\r" => {
            let (sel, model_idx) = {
                let a = app.lock().unwrap();
                let s = a.selected;
                (s, s.and_then(|i| a.model_map.iter().position(|m| *m == Some(i))))
            };
            if let Some(mi) = model_idx {
                if sel.is_some() {
                    select_row(app, cmd_tx, mi);
                }
            }
        }
        "\u{1b}" => send_msg(Msg::CloseDetail),
        "p" => local_update(app, "pinned", true_toggle(app, "pinned")),
        "1" | "2" | "3" => local_update(app, "priority", text.parse::<i64>().unwrap_or(0)),
        _ => {}
    }
}

fn true_toggle(app: &Arc<Mutex<App>>, field: &str) -> i64 {
    let a = app.lock().unwrap();
    a.selected_row()
        .and_then(|r| r[field].as_i64())
        .map(|v| if v == 0 { 1 } else { 0 })
        .unwrap_or(1)
}

fn local_update<V: Into<serde_json::Value>>(app: &Arc<Mutex<App>>, field: &str, value: V) {
    let a = app.lock().unwrap();
    if let Some(r) = a.selected_row() {
        let repo = r["repo"].as_str().unwrap_or("").to_string();
        let number = r["number"].as_i64().unwrap_or(0);
        let _ = a.db.local_pr_update(&repo, number, field, &value.into());
        drop(a);
        send_msg(Msg::Status(format!("已更新 {field}（待同步）")));
    }
}

// ── 后台 worker ──

async fn worker(app: Arc<Mutex<App>>, rx: mpsc::Receiver<Cmd>, msg_tx: mpsc::Sender<Msg>) {
    // 注册全局发信器（select_row 在 UI 线程也用）
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
                    // 1) pull
                    let since = { app.lock().unwrap().db.get_cursor()? };
                    let body = sync_client.pull_body(since).await?;
                    let pulled = {
                        let a = app.lock().unwrap();
                        sync::apply_pull(&a.db, &body)?
                    };
                    // 2) push
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
                            let a = app.lock().unwrap();
                            let _ = a.db.put_diff(&repo, number, &diff);
                            drop(a);
                            let _ = msg_tx.send(Msg::Diff(classify_diff(&diff)));
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

fn reload_rows(app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let rows = {
        let a = app.lock().unwrap();
        a.db.list_prs().unwrap_or_default()
    };
    let (model, map) = build_row_model(&rows);
    {
        let mut a = app.lock().unwrap();
        a.rows = rows;
        a.model_map = map;
    }
    let _ = msg_tx.send(Msg::Rows(model));
}

/// 行数据 → 分区行模型（经 lib 的 build_sections 分组，带组头）。
/// 返回 (模型, 模型索引→数据索引映射)（组头映射为 None）。
fn build_row_model(rows: &[serde_json::Value]) -> (Vec<PrRow>, Vec<Option<usize>>) {
    let mut out: Vec<PrRow> = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    for (title, idxs) in build_sections(rows) {
        out.push(PrRow {
            is_header: true,
            key: format!("h:{title}").into(),
            title: title.into(),
            ..Default::default()
        });
        map.push(None);
        for i in idxs {
            out.push(to_row(&rows[i]));
            map.push(Some(i));
        }
    }
    (out, map)
}

fn to_row(r: &serde_json::Value) -> PrRow {
    PrRow {
        is_header: false,
        key: format!("{}#{}", r["repo"].as_str().unwrap_or(""), r["number"].as_i64().unwrap_or(0)).into(),
        repo: r["repo"].as_str().unwrap_or("").into(),
        number: r["number"].as_i64().unwrap_or(0) as i32,
        title: r["title"].as_str().unwrap_or("").into(),
        author: r["author"].as_str().unwrap_or("").into(),
        score: r["score"]
            .as_f64()
            .map(|s| format!("{s:.1}"))
            .unwrap_or_default()
            .into(),
        ci: r["ci_status"].as_str().unwrap_or("").into(),
        review: r["review_decision"].as_str().unwrap_or("").into(),
        turn: r["turn"].as_str().unwrap_or("").into(),
        priority: r["priority"].as_i64().unwrap_or(0) as i32,
        pinned: r["pinned"].as_i64() == Some(1),
        on_board: r["on_board"].as_i64() == Some(1),
        selected: false,
    }
}

// ── UI 消息应用 ──

fn apply_msg(ui: &MainWindow, msg: Msg) {
    match msg {
        Msg::Status(s) => ui.set_status_text(s.into()),
        Msg::Rows(rows) => {
            ui.set_rows(ModelRc::new(VecModel::from(rows)));
        }
        Msg::Detail { title, meta, score, summary } => {
            ui.set_detail_open(true);
            ui.set_d_title(title.into());
            ui.set_d_meta(meta.into());
            ui.set_d_score(score.into());
            ui.set_d_summary(summary.into());
        }
        Msg::Diff(lines) => {
            ui.set_diff_lines(ModelRc::new(VecModel::from(lines)));
        }
        Msg::CloseDetail => ui.set_detail_open(false),
    }
}
