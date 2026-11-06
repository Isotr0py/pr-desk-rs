//! 后台 worker：tokio runtime + 命令循环。从旧 Slint 版 main.rs 逐语义移植——
//! 注释里标明的竞态守卫（档案切换串库防护、节流标记乐观记账、在飞去重、
//! 身份守卫）都是踩坑换来的，不得简化。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use pr_desk_rs::config::Config;
use pr_desk_rs::db::{self, Db};
use pr_desk_rs::diffview;
use pr_desk_rs::i18n::{tr, trf};
use pr_desk_rs::reviewer::LlmSettings;
use pr_desk_rs::{github, ocr, reviewer, sync};

use crate::avatars;
use crate::state::*;

/// 全局发信器（worker 启动时注册；UI 线程的即时反馈也经它回到事件循环）。
static MSG_TX: std::sync::OnceLock<mpsc::Sender<Msg>> = std::sync::OnceLock::new();
/// 与 MSG_TX 配套的 egui Context（发消息后请求重绘，空闲零 CPU 的关键）。
static CTX: std::sync::OnceLock<egui::Context> = std::sync::OnceLock::new();

pub fn send_msg(msg: Msg) {
    match MSG_TX.get() {
        Some(tx) => {
            if tx.send(msg).is_err() {
                tracing::warn!("消息通道已关闭（事件循环退出中），消息丢失");
                return;
            }
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        }
        // 启动竞态窗口（worker 尚未注册 MSG_TX）：消息丢失至少要可查
        None => tracing::warn!("MSG_TX 未注册，消息丢失（启动竞态窗口）"),
    }
}

/// worker 内部发信（同 send_msg，但有显式 sender 时用之——测试中不注册全局）。
fn send(tx: &mpsc::Sender<Msg>, msg: Msg) {
    if tx.send(msg).is_err() {
        tracing::warn!("消息通道已关闭，消息丢失");
        return;
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// 启动 worker 线程；返回命令发送端与线程句柄（正常退出时 join 用）。
pub fn spawn(
    app: Arc<Mutex<App>>,
    msg_tx: mpsc::Sender<Msg>,
    ctx: egui::Context,
    cfg_path: PathBuf,
) -> (UnboundedSender<Cmd>, std::thread::JoinHandle<()>) {
    let _ = CTX.set(ctx);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    let tx2 = cmd_tx.clone();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(worker(app, cmd_rx, msg_tx, tx2, cfg_path));
    });
    (cmd_tx, handle)
}

fn toggle_of(app: &Arc<Mutex<App>>, repo: &str, number: i64, field: &str) -> i64 {
    let a = app.lock_recover();
    find_row(&a, repo, number)
        .and_then(|idx| a.rows[idx][field].as_i64())
        .map(|v| if v == 0 { 1 } else { 0 })
        .unwrap_or(1)
}

/// 行字段 toggle 的便捷入口（行内按钮 / 面板按钮 / 快捷键 p 共用）。
pub fn toggle_field(app: &Arc<Mutex<App>>, repo: &str, number: i64, field: &str) {
    let v = toggle_of(app, repo, number, field);
    local_update(app, repo, number, field, v);
}

/// 本地字段更新（pinned/on_board/priority/status）：按身份直改 DB，
/// 不依赖行是否还在当前列表里（面板开着但行被刷新滤掉时操作仍生效）。
pub fn local_update<V: Into<Value>>(app: &Arc<Mutex<App>>, repo: &str, number: i64, field: &str, value: V) {
    let a = app.lock_recover();
    if a.db.local_pr_update(repo, number, field, &value.into()).is_ok() {
        let lang = a.cfg.language.clone();
        drop(a);
        reload_rows_global(app);
        // 内部字段名不直接抛给用户：经 i18n 映射成界面文案（未知字段回退原名）
        let label = match field {
            "pinned" => tr(&lang, "field_pinned"),
            "on_board" => tr(&lang, "field_on_board"),
            "priority" => tr(&lang, "field_priority"),
            "status" => tr(&lang, "field_status"),
            other => other,
        };
        send_msg(Msg::Status(trf(&lang, "field_updated", &[label])));
    }
}

// ── 多选 / 批量操作 ──

/// 批量字段更新：循环走 local_pr_update（字段白名单 + UPDATE/outbox 同事务不变），
/// 全部写完后只重建一次行数据、只发一条状态栏汇总（逐条调 local_update 会
/// 每条都重建 + 刷一条消息，N 大时闪屏且消息互相覆盖）。
pub fn batch_local_update<V: Into<Value> + Clone>(
    app: &Arc<Mutex<App>>,
    keys: &[(String, i64)],
    field: &str,
    value: V,
) {
    let (n, lang) = {
        let a = app.lock_recover();
        let mut n = 0usize;
        for (repo, number) in keys {
            if a.db.local_pr_update(repo, *number, field, &value.clone().into()).is_ok() {
                n += 1;
            }
        }
        (n, a.cfg.language.clone())
    };
    if n > 0 {
        reload_rows_global(app);
        send_msg(Msg::Status(trf(&lang, "fields_updated", &[&n.to_string()])));
    }
}

/// 批量 toggle（pinned/on_board）：目标值按「是否全部已置位」决定（全 1 → 0，否则 1）。
pub fn batch_toggle(app: &Arc<Mutex<App>>, field: &str) {
    let keys = {
        let a = app.lock_recover();
        effective_targets(&a)
    };
    if keys.is_empty() {
        return;
    }
    let target = {
        let a = app.lock_recover();
        let vals: Vec<i64> = keys
            .iter()
            .map(|(r, n)| {
                find_row(&a, r, *n)
                    .and_then(|i| a.rows[i][field].as_i64())
                    .unwrap_or(0)
            })
            .collect();
        pr_desk_rs::selection::batch_toggle_target(vals.iter())
    };
    batch_local_update(app, &keys, field, target);
}

/// 浏览器打开 PR：优先用行数据里的 url，缺失时按 repo/number 拼 GitHub 链接兜底。
pub fn open_pr_url(app: &Arc<Mutex<App>>, repo: &str, number: &i64) {
    let url = {
        let a = app.lock_recover();
        find_row(&a, repo, *number)
            .and_then(|i| a.rows[i]["url"].as_str().map(|s| s.to_string()))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| format!("https://github.com/{repo}/pull/{number}"))
    };
    if let Err(e) = open::that(&url) {
        let lang = lang_of(app);
        send_msg(Msg::Status(trf(&lang, "open_browser_failed", &[&e.to_string()])));
    }
}

/// 批量「浏览器打开」：上限 5 个（防快捷键误触一次弹几十个标签页），超出部分状态栏提示。
pub fn batch_open_browser(app: &Arc<Mutex<App>>) {
    const MAX_OPEN: usize = 5;
    let keys = {
        let a = app.lock_recover();
        effective_targets(&a)
    };
    for (repo, number) in keys.iter().take(MAX_OPEN) {
        open_pr_url(app, repo, number);
    }
    if keys.len() > MAX_OPEN {
        let lang = lang_of(app);
        send_msg(Msg::Status(trf(
            &lang,
            "batch_open_capped",
            &[&MAX_OPEN.to_string(), &(keys.len() - MAX_OPEN).to_string()],
        )));
    }
}

/// 批量复制链接的文本拼接（UI 回调拼好，剪贴板写入在 worker 的 Cmd::CopyLinks）。
pub fn copy_links_text(app: &Arc<Mutex<App>>) -> Option<String> {
    let a = app.lock_recover();
    let keys = effective_targets(&a);
    if keys.is_empty() {
        return None;
    }
    let text = keys
        .iter()
        .map(|(repo, number)| {
            find_row(&a, repo, *number)
                .and_then(|i| a.rows[i]["url"].as_str().map(|s| s.to_string()))
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| format!("https://github.com/{repo}/pull/{number}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(text)
}

/// 重载列表行（UI 线程同步路径：local_update / 批量操作后）。
pub fn reload_rows_global(app: &Arc<Mutex<App>>) {
    let rows = {
        let a = app.lock_recover();
        match a.db.list_prs(list_sources(&a.cfg)) {
            Ok(rows) => rows,
            Err(e) => {
                // 失败时保留旧行并提示——清空成空列表会让用户以为数据全没了
                tracing::warn!("列表加载失败: {e}");
                let lang = a.cfg.language.clone();
                drop(a);
                send_msg(Msg::Status(trf(&lang, "list_load_failed", &[&e.to_string()])));
                return;
            }
        }
    };
    {
        let mut a = app.lock_recover();
        a.total = rows.len();
        a.rows = rows;
        recompute_flat(&mut a);
    }
    send_msg(Msg::StatusBarRefresh);
}

/// 重载列表行（worker 侧：DB 读 + App 更新 + 回送 + 头像预取）。
/// 同步函数（DB 操作均为同步），不进入 async。
fn reload_rows(app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let rows = {
        let a = app.lock_recover();
        match a.db.list_prs(list_sources(&a.cfg)) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("列表加载失败: {e}");
                let lang = a.cfg.language.clone();
                drop(a);
                send(msg_tx, Msg::Status(trf(&lang, "list_load_failed", &[&e.to_string()])));
                return;
            }
        }
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
        orgs.extend(author_orgs(r));
    }
    {
        let mut a = app.lock_recover();
        a.total = rows.len();
        a.rows = rows.clone();
        recompute_flat(&mut a);
    }
    // serde_json 行可跨线程
    send(msg_tx, Msg::Rows(rows));
    let app2 = app.clone();
    let msg_tx2 = msg_tx.clone();
    tokio::spawn(async move {
        // 用 GitHub::new 的标准客户端（与 fetch_inbox 同款构建方式，已证明稳定）；
        // 之前 prefetch 自建裸客户端在 worker 运行时下会挂死在 send()，原因未查明
        let (token, proxy) = {
            let c = app2.lock_recover().cfg.clone();
            (c.token(), c.proxy)
        };
        let Ok(gh) = github::GitHub::new(token, &proxy) else { return };
        let fetched = avatars::prefetch(&gh, logins).await + avatars::prefetch_orgs(&gh, orgs).await;
        if fetched > 0 {
            // 新头像落盘：重建一次让 UI 拿到图（缓存已新鲜，不会递归预取）。
            // egui 侧纹理按 mtime 失效，这里仅需触发一次重绘
            send(&msg_tx2, Msg::StatusBarRefresh);
        }
    });
}

// ── Inbox ──

/// 批量补 PR/Issue 的 open/closed/merged 状态（GraphQL 一次请求）。
/// 失败降级为空表——状态只是行装饰，不拖垮通知拉取本身。
async fn fetch_notif_states(
    gh: &github::GitHub,
    ns: &[github::Notification],
) -> std::collections::HashMap<String, String> {
    let mut seen = HashSet::new();
    let items: Vec<(String, i64, bool)> = ns
        .iter()
        .filter_map(|n| {
            let is_pr = match n.kind.as_str() {
                "PullRequest" => true,
                "Issue" => false,
                _ => return None,
            };
            let number = notif_url_number(&notif_html_url(&n.url))?;
            if seen.insert((n.repo.clone(), number)) {
                Some((n.repo.clone(), number, is_pr))
            } else {
                None
            }
        })
        .collect();
    if items.is_empty() {
        return Default::default();
    }
    match gh.fetch_subject_states(&items).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("通知状态批量查询失败: {e}");
            Default::default()
        }
    }
}

/// 通知 → 行 JSON（state 取自批量查询结果，查不到为空串）。
fn notif_row_json(n: &github::Notification, states: &std::collections::HashMap<String, String>) -> Value {
    let url = notif_html_url(&n.url);
    let state = notif_url_number(&url)
        .and_then(|num| states.get(&format!("{}#{num}", n.repo)).cloned())
        .unwrap_or_default();
    serde_json::json!({
        "id": n.id, "repo": n.repo, "title": n.title, "kind": n.kind,
        "reason": n.reason, "updated_at": n.updated_at, "unread": n.unread,
        "url": url, "state": state,
    })
}

/// 拉取 GitHub 通知并回送 UI（无 token 时静默跳过：通知接口必须鉴权）。
/// 刷新语义：重拉已加载的全部页（1..=inbox_page），保序拼接 + 按 thread id 去重，
/// 这样用户「加载更多」后定时刷新不会把多页数据塌回第一页。
/// 拉取通知（在飞合并去重）：spawn 化后启动路径会同时发 RefreshGithub + Sync，
/// 两臂都拉通知——同一毫秒跑两次全量拉取白烧 API 配额。这里做在飞去重：
/// 已有拉取在飞时只记「结束后补跑一次」（coalesce，不排队 N 次）。
pub async fn fetch_inbox(gh: &github::GitHub, app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    loop {
        {
            let mut a = app.lock_recover();
            if a.inbox_fetching {
                a.inbox_fetch_again = true;
                return;
            }
            a.inbox_fetching = true;
        }
        fetch_inbox_once(gh, app, msg_tx).await;
        let again = {
            let mut a = app.lock_recover();
            a.inbox_fetching = false;
            std::mem::take(&mut a.inbox_fetch_again)
        };
        if !again {
            return;
        }
    }
}

async fn fetch_inbox_once(gh: &github::GitHub, app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let (has_token, all, pages) = {
        let a = app.lock_recover();
        (a.cfg.token().is_some(), a.inbox_all, a.inbox_page.max(1))
    };
    tracing::info!("fetch_inbox: has_token={} all={} pages={}", has_token, all, pages);
    if !has_token {
        // 无 token：关掉可能存在的 UI 加载态（首次进 Inbox 时置过 true）
        send(msg_tx, Msg::InboxLoading(false));
        return;
    }
    let mut all_ns: Vec<github::Notification> = Vec::new();
    let mut seen = HashSet::new();
    let mut has_more = false;
    for page in 1..=pages {
        match gh.fetch_notifications_page(all, page).await {
            Ok((ns, more)) => {
                has_more = more;
                for n in ns {
                    if seen.insert(n.id.clone()) {
                        all_ns.push(n);
                    }
                }
                // 本页未满或没有下一页：后面的页码必然也是空的，提前收工
                if !more {
                    break;
                }
            }
            Err(e) => {
                let lang = lang_of(app);
                send(msg_tx, Msg::Status(trf(&lang, "inbox_fetch_failed", &[&e.to_string()])));
                // 失败也要关首次加载态（否则 Inbox 永远停在「加载中」）
                send(msg_tx, Msg::InboxLoading(false));
                return;
            }
        }
    }
    tracing::info!("fetch_inbox: got {} notifications ({} pages)", all_ns.len(), pages);
    let states = fetch_notif_states(gh, &all_ns).await;
    {
        let mut a = app.lock_recover();
        a.inbox_has_more = has_more;
    }
    let rows: Vec<Value> = all_ns.iter().map(|n| notif_row_json(n, &states)).collect();
    send(msg_tx, Msg::InboxRows(rows));
}

/// 「加载更多」：拉下一页追加到现有列表（去重）。进行中或无下一页时忽略。
async fn fetch_inbox_more(gh: &github::GitHub, app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let (has_token, all, next_page) = {
        let mut a = app.lock_recover();
        if a.inbox_loading_more || !a.inbox_has_more {
            return;
        }
        a.inbox_loading_more = true;
        (a.cfg.token().is_some(), a.inbox_all, a.inbox_page + 1)
    };
    if !has_token {
        app.lock_recover().inbox_loading_more = false;
        return;
    }
    let result = gh.fetch_notifications_page(all, next_page).await;
    // 新页的状态在持锁前查好（GraphQL 一次请求；失败降级为空表）
    let new_states = match &result {
        Ok((ns, _)) => fetch_notif_states(gh, ns).await,
        Err(_) => Default::default(),
    };
    let merged: Vec<Value>;
    {
        let mut a = app.lock_recover();
        a.inbox_loading_more = false;
        match result {
            Ok((ns, more)) => {
                let mut seen: HashSet<String> = a
                    .inbox
                    .iter()
                    .filter_map(|r| r["id"].as_str().map(|s| s.to_string()))
                    .collect();
                a.inbox.extend(
                    ns.iter()
                        .filter(|n| seen.insert(n.id.clone()))
                        .map(|n| notif_row_json(n, &new_states)),
                );
                a.inbox_page = next_page;
                a.inbox_has_more = more;
                merged = a.inbox.clone();
            }
            Err(e) => {
                let lang = a.cfg.language.clone();
                drop(a);
                send(msg_tx, Msg::Status(trf(&lang, "inbox_fetch_failed", &[&e.to_string()])));
                return;
            }
        }
    }
    tracing::info!("fetch_inbox_more: page {} loaded, total {} rows", next_page, merged.len());
    send(msg_tx, Msg::InboxRows(merged));
}

// ── 评审任务 ──

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
    let lang = cfg.language.as_str();
    send(msg_tx, Msg::ReviewProgress {
        repo: repo.to_string(),
        number,
        text: trf(lang, "review_start", &[repo, &number.to_string()]),
    });
    // 进度回调 → channel（状态栏 + 面板进度，见 Msg::ReviewProgress）
    let tx = msg_tx.clone();
    let (r2, n2) = (repo.to_string(), number);
    let mut progress = move |text: String| {
        send(&tx, Msg::ReviewProgress { repo: r2.clone(), number: n2, text });
    };
    // 设置页的自定义评审模板（exe 旁 review_prompt.md；空/缺失 = 默认模板）
    let custom_prompt = std::fs::read_to_string(pr_desk_rs::exe_dir().join("review_prompt.md"))
        .ok()
        .filter(|s| !s.trim().is_empty());
    match reviewer::run_review(llm, g, repo, number, &mut progress, lang, custom_prompt.as_deref()).await {
        Ok(mut v) => {
            // 第二段：OCR 行级评论（评分已拿到，OCR 失败不拖垮整单）
            match g.fetch_pr_meta(repo, number).await {
                Ok(meta) => {
                    match ocr::run_ocr_review(cfg, g, repo, number, &meta, &mut progress, lang).await {
                        Ok(outcome) => {
                            send(msg_tx, Msg::ReviewProgress {
                                repo: repo.to_string(),
                                number,
                                text: trf(
                                    lang,
                                    "ocr_done_status",
                                    &[
                                        &outcome.comments.len().to_string(),
                                        &outcome.files_reviewed.to_string(),
                                    ],
                                ),
                            });
                            v["comments"] = Value::Array(outcome.comments);
                        }
                        Err(e) => {
                            // 状态栏一闪而过很难捉到，同步写日志
                            tracing::warn!("OCR 明细获取失败 {repo} #{number}: {e}");
                            send(msg_tx, Msg::Status(trf(lang, "ocr_detail_failed", &[&e.to_string()])));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("OCR 前置 meta 拉取失败 {repo} #{number}: {e}");
                    send(msg_tx, Msg::Status(trf(lang, "ocr_detail_failed", &[&e.to_string()])));
                }
            }
            let ok = { app.lock_recover().db.upsert_review(&v).is_ok() };
            if ok {
                send(msg_tx, Msg::ReviewDone { repo: repo.to_string(), number });
            } else {
                send(msg_tx, Msg::ReviewFailed {
                    repo: repo.to_string(),
                    number,
                    err: tr(lang, "review_save_failed").into(),
                });
            }
        }
        Err(e) => {
            send(msg_tx, Msg::ReviewFailed {
                repo: repo.to_string(),
                number,
                err: format!("{e}"),
            });
        }
    }
}

// ── 刷新 / 同步 ──

/// 本地模式刷新：本地库是手动策展的（只有 Inbox「+ 预评审」加入的 PR），
/// 不从 GitHub 自动拉任何 PR 列表——这里只刷新在库行的 GitHub 状态 + 通知收件箱。
/// sync 启用时不走这条路——列表以服务器插件数据库为准。
async fn refresh_from_github(gh: &github::GitHub, app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>, for_profile: &str) {
    fetch_inbox(gh, app, msg_tx).await;
    // 本地模式（手动策展）：顺带刷新每个在库 PR 的 GitHub 状态——merged/closed 写入
    // status 列后分组会把已完结 PR 移出待评审区；否则合入了列表还显示 Open。
    // sync 模式跳过：行数据由服务端插件负责保持新鲜。
    let sync_enabled = { app.lock_recover().cfg.active_sync().is_some() };
    if !sync_enabled {
        // 先用 diff 缓存回填增删行数（纯本地计算；打开过 Files/Diff 的 PR 立即有数，
        // 列表 +/- 列不再 +0/-0；只填空值，GitHub files API 的真值优先）
        {
            // 先取数据放锁，解析（逐行扫描 diff）在锁外做，写完再取锁——
            // 持锁解析会堵住 UI 线程的 app.lock()，表现为界面冻结
            let cached = {
                let a = app.lock_recover();
                match a.db.list_diff_cache() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("读取 diff 缓存列表失败: {e}");
                        Vec::new()
                    }
                }
            };
            let sums: Vec<(String, i64, i64, i64)> = cached
                .iter()
                .map(|(repo, number, diff)| {
                    let (adds, dels) = diffview::split_files(diff)
                        .iter()
                        .fold((0i64, 0i64), |(a, d), (_p, text)| {
                            (
                                a + text.lines().filter(|l| l.starts_with('+')).count() as i64,
                                d + text.lines().filter(|l| l.starts_with('-')).count() as i64,
                            )
                        });
                    (repo.clone(), *number, adds, dels)
                })
                .collect();
            {
                // 写回前校验档案未切换（在途数据不写进切后的库）
                if let Some(a) = lock_for_profile(app, for_profile) {
                    for (repo, number, adds, dels) in sums {
                        let _ = a.db.fill_diff_sums_if_empty(&repo, number, adds, dels);
                    }
                }
            }
        }
        // 并行拉每个在库 PR 的 GitHub 状态（merged/closed 写入 status 列后分组会把
        // 已完结 PR 移出待评审区；否则合入了列表还显示 Open）。之前是串行循环，
        // N 个 PR + M 个作者串行各一次请求，切换档案时要卡十几秒
        let keys = {
            let a = app.lock_recover();
            match a.db.list_pr_keys() {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!("列出 PR 键失败，跳过 GitHub 状态刷新: {e}");
                    Vec::new()
                }
            }
        };
        {
            let mut set = tokio::task::JoinSet::new();
            for (repo, number) in keys {
                let g = gh.clone();
                set.spawn(async move {
                    let meta = g.fetch_pr_meta(&repo, number).await;
                    (repo, number, meta)
                });
            }
            while let Some(Ok((repo, number, res))) = set.join_next().await {
                match res {
                    Ok(meta) => {
                        let gh_state = if meta["merged_at"].is_null() {
                            meta["state"].as_str().unwrap_or("open").to_string()
                        } else {
                            "merged".to_string()
                        };
                        if gh_state == "merged" || gh_state == "closed" {
                            // 档案已切走则丢弃这条在途结果，不写进新档案的库
                            if let Some(a) = lock_for_profile(app, for_profile) {
                                let _ = a.db.conn.execute(
                                    "UPDATE prs SET status=?1 WHERE repo=?2 AND number=?3 AND status NOT IN ('merged','closed')",
                                    rusqlite::params![gh_state, repo, number],
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("刷新 PR 状态失败 {repo} #{number}: {e}");
                    }
                }
            }
        }
        // 作者公开组织成员关系（org 徽章数据源；本地行没人填这个字段，永远不会有 org 标）。
        // 同样并行
        let authors = {
            let a = app.lock_recover();
            match a.db.list_authors() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("列出作者失败，跳过组织关系刷新: {e}");
                    Vec::new()
                }
            }
        };
        {
            let mut set = tokio::task::JoinSet::new();
            for author in authors {
                let g = gh.clone();
                set.spawn(async move {
                    let orgs = g.fetch_user_orgs(&author).await;
                    (author, orgs)
                });
            }
            while let Some(Ok((author, res))) = set.join_next().await {
                match res {
                    Ok(orgs) => {
                        // 同上：档案切走则丢弃
                        if let Some(a) = lock_for_profile(app, for_profile) {
                            let _ = a.db.set_author_orgs(&author, &orgs);
                        }
                    }
                    Err(e) => {
                        tracing::debug!("拉取作者组织失败 {author}: {e}");
                    }
                }
            }
        }
    }
    // 本地库是手动策展的：不从 GitHub 拉列表，但必须把库里的现有行装进 UI
    reload_rows(app, msg_tx);
}

/// 当前生效档案仍是 for_profile 时返回持有的锁守卫，否则 None。
/// 后台任务（sync 拉取 / GitHub 状态刷新）的网络请求要几秒，其间用户可能已切走档案；
/// 在途结果若写回「当前」库就会把 A 档案的数据灌进 B 档案（曾致服务器列表串进本地库）。
/// 检查与写入在同一把锁内完成：SwitchProfile 换库也要这把锁，无法插进来。
fn lock_for_profile<'a>(app: &'a Mutex<App>, for_profile: &str) -> Option<std::sync::MutexGuard<'a, App>> {
    let a = app.lock_recover();
    if a.cfg.active_profile == for_profile {
        Some(a)
    } else {
        None
    }
}

/// 一次 sync_pull → 本地应用 → outbox/reviews push 流程（Cmd::Sync 与 sync 模式下的
/// RefreshGithub/ReloadConfig 共用）。sync 未启用时静默跳过。
/// for_profile：发起时的档案名——任何写库阶段发现当前档案已切换则中止（防串库）。
pub async fn do_sync(app: &Arc<Mutex<App>>, sync_client: &sync::SyncClient, msg_tx: &mpsc::Sender<Msg>, for_profile: &str) {
    if !sync_client.enabled() {
        // 本地模式：静默跳过，状态栏由 UI 侧显示「本地模式」
        return;
    }
    // 不跨 await 持有 DB 锁：HTTP 与 DB 应用分步，各自短时加锁。
    // 每次加锁都经 lock_for_profile 校验档案未切换——网络在途期间用户切走档案则
    // 中止写库（Ok(None)），否则在途结果会灌进切后的库（曾把服务器列表写进本地库）。
    // 被丢弃的拉取不丢数据：该档案的游标未前进，下次同步会重新拉。
    let result: anyhow::Result<Option<((usize, usize, usize), sync::PushSummary)>> = async {
        let since = {
            let Some(a) = lock_for_profile(app, for_profile) else { return Ok(None) };
            a.db.get_cursor()?
        };
        let body = sync_client.pull_body(since).await?;
        let pulled = {
            let Some(a) = lock_for_profile(app, for_profile) else { return Ok(None) };
            sync::apply_pull(&a.db, &body)?
        };
        let outbox = {
            let Some(a) = lock_for_profile(app, for_profile) else { return Ok(None) };
            a.db.outbox_drain()?
        };
        let (updates, ids) = sync::merge_outbox(&outbox);
        let reviews_new = {
            let Some(a) = lock_for_profile(app, for_profile) else { return Ok(None) };
            a.db.unpushed_reviews()?
        };
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
            let Some(a) = lock_for_profile(app, for_profile) else { return Ok(None) };
            pushed = sync::apply_push_result(&a.db, &resp)?;
            for id in ids {
                a.db.outbox_remove(id)?;
            }
            a.db.mark_reviews_pushed(&review_keys)?;
        }
        Ok(Some((pulled, pushed)))
    }
    .await;
    match result {
        Ok(Some(((p, r, t), push))) => {
            // 记入「已同步 X 前」共享状态（状态栏组合读取）
            let (last_sync, lang) = {
                let a = app.lock_recover();
                (a.last_sync.clone(), a.cfg.language.clone())
            };
            sync::mark_synced(&last_sync);
            send(msg_tx, Msg::Status(trf(
                &lang,
                "sync_done",
                &[
                    &p.to_string(),
                    &r.to_string(),
                    &t.to_string(),
                    &push.applied.to_string(),
                    &push.conflicts.to_string(),
                ],
            )));
            reload_rows(app, msg_tx);
        }
        Ok(None) => {
            // 档案中途切换：在途结果已丢弃（未写任何库），状态栏不用打扰用户
            tracing::info!("do_sync: 档案已从 {for_profile} 切走，在途结果丢弃");
        }
        Err(e) => {
            let lang = lang_of(app);
            send(msg_tx, Msg::Status(trf(&lang, "sync_failed", &[&e.to_string()])));
        }
    }
}

// ── 详情面板装配（worker 侧）──

/// 打开详情面板（worker 侧，Cmd::OpenDrawer 的处理体）：
/// 全部 DB 读写 + diff 解析在这里完成，UI 线程只负责把回送的 Msg 落地。
/// 选中态（a.selected）由调用方先行设置（UI 点行时立即落地，
/// 后续 Panel/DiffParsed 的身份守卫以此为准）。
fn open_drawer_work(app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>, cmd_tx: &UnboundedSender<Cmd>, repo: String, number: i64, refetch: bool) {
    let lang = lang_of(app);
    // 行数据是快照（row 允许为 None：行被刷新滤掉时面板也保持打开，数据来自 DB）
    let (priority, row) = {
        let a = app.lock_recover();
        let idx = find_row(&a, &repo, number);
        let priority = idx
            .and_then(|i| a.rows[i]["priority"].as_i64())
            .unwrap_or(0) as i32;
        (priority, idx.and_then(|i| a.rows.get(i).cloned()))
    };

    let (review, diff_cached) = {
        let a = app.lock_recover();
        let review = match a.db.get_review(&repo, number) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("读取评审失败 {repo} #{number}: {e}");
                None
            }
        };
        let diff = match a.db.get_diff(&repo, number) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("读取 diff 缓存失败 {repo} #{number}: {e}");
                None
            }
        };
        (review, diff)
    };

    // 会话条目在 worker 装配（纯数据过界）；Checks/Files 保持 JSON，UI 线程解析
    let (convo_json, checks_json) = row
        .as_ref()
        .map(|r| drawer_extras_json(r))
        .unwrap_or_default();
    let convo = plain_convo_entries(
        &serde_json::from_str::<Vec<Value>>(&convo_json).unwrap_or_default(),
    );
    // 会话作者头像预取：UI 装盒时读的是磁盘缓存（miss → 字母降级），这里后台补齐；
    // 有实际新拉取才发 ConvoAvatars 让 UI 原地回填（模式照抄 reload_rows 尾部的头像预取：
    // 必须 GitHub::new 标准客户端——自建裸 reqwest 在 worker 会挂死）。spawn 而非 inline
    // await：open_drawer_work 跑在命令循环里，网络 await 会堵住后续命令（延迟事故的教训）
    let convo_authors: Vec<String> = {
        let mut v: Vec<String> = convo
            .iter()
            .map(|e| e.author.clone())
            .filter(|a| !a.is_empty())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    if !convo_authors.is_empty() {
        let app2 = app.clone();
        let msg_tx2 = msg_tx.clone();
        let (repo2, number2) = (repo.clone(), number);
        tokio::spawn(async move {
            let (token, proxy) = {
                let c = app2.lock_recover().cfg.clone();
                (c.token(), c.proxy)
            };
            let Ok(gh) = github::GitHub::new(token, &proxy) else { return };
            if avatars::prefetch(&gh, convo_authors).await > 0 {
                send(&msg_tx2, Msg::ConvoAvatars { repo: repo2, number: number2 });
            }
        });
    }
    let files_json = match &diff_cached {
        Some(d) => {
            let j = files_json_from_diff(d);
            // diff 缓存的增删行数顺手落库（列表 +/- 列的兜底数据源，GitHub files API 的
            // 真值拉到时会覆盖，见 FetchPrLive 的文件分支）
            if let Ok(files) = serde_json::from_str::<Vec<Value>>(&j) {
                let (adds, dels) = files.iter().fold((0i64, 0i64), |(a, dd), f| {
                    (
                        a + f["adds"].as_i64().unwrap_or(0),
                        dd + f["dels"].as_i64().unwrap_or(0),
                    )
                });
                let a = app.lock_recover();
                if let Err(e) = a.db.conn.execute(
                    "UPDATE prs SET additions=?1, deletions=?2 WHERE repo=?3 AND number=?4",
                    rusqlite::params![adds, dels, repo, number],
                ) {
                    tracing::warn!("diff 增删行数落库失败 {repo} #{number}: {e}");
                }
            }
            j
        }
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
    let age = row
        .as_ref()
        .map(|r| rel_time(r["updated_at"].as_str().unwrap_or("")))
        .unwrap_or_default();
    let pinned = row.as_ref().map(|r| r["pinned"].as_i64() == Some(1)).unwrap_or(false);
    let on_board = row.as_ref().map(|r| r["on_board"].as_i64() == Some(1)).unwrap_or(false);

    send(msg_tx, Msg::Panel(Box::new(PanelData {
        repo: repo.clone(),
        number,
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
        updated: age,
        priority,
        pinned,
        on_board,
        convo,
        checks_json,
        files_json,
        review: review_props(review.as_ref(), &lang),
        review_status: String::new(),
        review_progress: String::new(),
    })));

    // 评审进行中打开该 PR 面板：紧随 Panel 之后把面板置 running
    //（apply 顺序处理，ReviewProgress 在 Panel 重置状态后生效）
    let running = app.lock_recover().review_slots.lock_recover().is_running(&repo, number);
    if running {
        send(msg_tx, Msg::ReviewProgress { repo: repo.clone(), number, text: tr(&lang, "review_running").into() });
    }

    // diff 解析在 worker 完成（大 PR 几万行逐行解析不占 UI 线程），
    // DiffParsed 带身份：面板已切走时 apply 丢弃上屏（缓存可用）
    match diff_cached {
        Some(d) => {
            let files = parse_diff(&d);
            send(msg_tx, Msg::DiffParsed { repo: repo.clone(), number, files });
        }
        None => {
            let _ = cmd_tx.send(Cmd::FetchDiff { repo: repo.clone(), number });
        }
    }

    // 行数据残缺（Inbox「+ 预评审」入库只带 title/url）：后台补齐 author/head_sha，
    // 成功后 worker 发 Msg::ReopenPanel 重新装载面板。
    // 注意：只能看 author——head_sha 不在 list_prs 的返回列里，看它会造成「永远残缺」
    // → FetchPrMeta → ReopenPanel → OpenDrawer 的死循环（烧穿 API 限额的教训）。
    let damaged = match &row {
        Some(r) => r["author"].as_str().unwrap_or("").is_empty(),
        None => false,
    };
    if refetch && damaged {
        let _ = cmd_tx.send(Cmd::FetchPrMeta { repo: repo.clone(), number });
    }

    // 面板先显示缓存（可能陈旧）；后台实时拉取最新状态，回来后覆盖面板与列表行。
    // ReopenPanel 重开时 refetch=false，不再重复派发。
    if refetch {
        let _ = cmd_tx.send(Cmd::FetchPrLive { repo, number, parts: PART_CHECKS });
    }
}

// ── worker 命令循环 ──

async fn worker(app: Arc<Mutex<App>>, mut rx: UnboundedReceiver<Cmd>, msg_tx: mpsc::Sender<Msg>, cmd_tx: UnboundedSender<Cmd>, cfg_path: PathBuf) {
    let _ = MSG_TX.set(msg_tx.clone());

    let (token, proxy) = {
        let cfg = app.lock_recover().cfg.clone();
        (cfg.token(), cfg.proxy.clone())
    };
    let mut gh = match github::GitHub::new(token, &proxy) {
        Ok(g) => Some(g),
        Err(e) => {
            let lang = lang_of(&app);
            send(&msg_tx, Msg::Status(trf(&lang, "gh_client_init_failed", &[&e.to_string()])));
            None
        }
    };
    // 配额共享句柄交给 App（UI 状态栏读取 X-RateLimit-Remaining）
    if let Some(g) = &gh {
        app.lock_recover().rate_limit = g.rate_limit_handle();
    }
    let mut sync_client = {
        let cfg = app.lock_recover().cfg.clone();
        sync::SyncClient::new(&cfg)
    };
    // 实时刷新节流：同一 PR 同一部分（parts 位标志）的上次拉取时间（120s 内不重复拉）。
    // Arc<Mutex>：网络臂已 spawn 化，在飞任务的节流标记由 task 与命令循环共享
    //（spawn 时乐观标记防在飞重入，失败由 task 移除标记——失败不吃满惩罚窗口）
    let live_fetched: Arc<Mutex<std::collections::HashMap<(String, i64, u8), std::time::Instant>>> =
        Default::default();
    // 元数据补齐节流：同一行 300s 内最多补一次（兜底防 open_drawer↔ReopenPanel 循环）
    let meta_healed: Arc<Mutex<std::collections::HashMap<(String, i64), std::time::Instant>>> =
        Default::default();

    // unbounded channel + recv().await：不在 tokio 工作线程上做 std 阻塞 recv；
    // 所有发送端 drop（UI 退出）或收到 Shutdown 时循环退出——进程不再做僵尸
    while let Some(cmd) = rx.recv().await {
        // 每条命令开始时取一次界面语言（设置页切换语言后下条命令即生效）
        let lang = lang_of(&app);
        match cmd {
            Cmd::Shutdown => break,
            Cmd::RefreshGithub => {
                // 先把本地库现有内容装进列表（本地模式手动策展的 PR 启动/刷新时立刻可见，
                // 不能等网络；sync 模式下这是 sync_pull 前的缓存快照）——本地操作，留在循环内
                reload_rows(&app, &msg_tx);
                // 网络部分 spawn：直连 GitHub 不稳（30s 超时），内联 await 会把后面的
                // OpenDrawer（行点击）堵在队列里秒级延迟。连点去重标志 gh_refreshing 在
                // UI 回调置位、这里复位——在途期间重复命令在 UI 侧已被拒，spawn 不改变语义。
                // 档案名随任务走：网络在途期间切档案时，在途结果不得写进切后的库
                let for_profile = app.lock_recover().cfg.active_profile.clone();
                let finish = |app: &Arc<Mutex<App>>, tx: &mpsc::Sender<Msg>| {
                    let open_count = {
                        let mut a = app.lock_recover();
                        a.gh_refreshing = false;
                        a.rows
                            .iter()
                            .filter(|r| !matches!(r["status"].as_str(), Some("merged") | Some("closed")))
                            .count()
                    };
                    send(tx, Msg::RefreshDone(open_count));
                };
                if let Some(g) = gh.clone() {
                    let sc = sync_client.clone();
                    let app2 = app.clone();
                    let tx = msg_tx.clone();
                    tokio::spawn(async move {
                        if sc.enabled() {
                            // sync 模式：列表来自服务器插件数据库（sync_pull）+ 手动预评审，
                            // 不再把配置仓库的全部 open PR 灌进列表库；通知仍从 GitHub 拉
                            fetch_inbox(&g, &app2, &tx).await;
                            do_sync(&app2, &sc, &tx, &for_profile).await;
                        } else {
                            refresh_from_github(&g, &app2, &tx, &for_profile).await;
                            // 顺便拉一次通知
                            fetch_inbox(&g, &app2, &tx).await;
                        }
                        // 收尾：复位连点去重标志 + 停顶栏转圈 + 状态栏报结果（open PR 数）
                        finish(&app2, &tx);
                    });
                } else {
                    finish(&app, &msg_tx);
                }
            }
            Cmd::Sync => {
                // 同 RefreshGithub：网络部分 spawn，去重标志 sync_inflight 在 UI 回调置位、
                // task 尾复位。do_sync 不依赖 gh（同步服务器流量），gh 缺失也要跑。
                let for_profile = app.lock_recover().cfg.active_profile.clone();
                let g = gh.clone();
                let sc = sync_client.clone();
                let app2 = app.clone();
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    // 通知与同步服务器无关，先顺手拉一次
                    if let Some(g) = &g {
                        fetch_inbox(g, &app2, &tx).await;
                    }
                    do_sync(&app2, &sc, &tx, &for_profile).await;
                    // 收尾：复位去重标志 + 停顶栏转圈（结果文案由 do_sync 自己的 Status 负责；
                    // 本地模式 sync 未启用时 do_sync 静默跳过，这里也不打扰状态栏）
                    app2.lock_recover().sync_inflight = false;
                    send(&tx, Msg::SyncFinished);
                });
            }
            Cmd::FetchDiff { repo, number } => {
                // spawn：diff 拉取是纯网络活；DiffParsed 带身份守卫（面板已切走 apply 丢弃）
                if let Some(g) = gh.clone() {
                    let app2 = app.clone();
                    let tx = msg_tx.clone();
                    tokio::spawn(async move {
                        match g.fetch_diff(&repo, number).await {
                            Ok(diff) => {
                                {
                                    let a = app2.lock_recover();
                                    if let Err(e) = a.db.put_diff(&repo, number, &diff) {
                                        tracing::warn!("diff 缓存写入失败 {repo} #{number}: {e}");
                                    }
                                }
                                // 解析在 task 内完成；消息带身份，面板已切走时 apply 丢弃上屏
                                let files = parse_diff(&diff);
                                send(&tx, Msg::DiffParsed { repo, number, files });
                                // diff 响应也带配额头，状态栏重组刷新配额
                                send(&tx, Msg::StatusBarRefresh);
                            }
                            Err(e) => {
                                let lang = lang_of(&app2);
                                send(&tx, Msg::Status(trf(&lang, "diff_fetch_failed", &[&e.to_string()])));
                            }
                        }
                    });
                }
            }
            Cmd::FetchInbox => {
                // spawn：通知拉取是纯网络活；InboxRows 是全量替换语义，无需身份守卫
                let g = gh.clone();
                let app2 = app.clone();
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    if let Some(g) = &g {
                        fetch_inbox(g, &app2, &tx).await;
                    } else {
                        // 客户端初始化失败：也要关掉 UI 的「加载中…」（否则永远转圈）
                        send(&tx, Msg::InboxLoading(false));
                    }
                });
            }
            Cmd::FetchInboxMore => {
                // spawn：去重/追加/页码状态都在 fetch_inbox_more 内部持锁完成
                if let Some(g) = gh.clone() {
                    let app2 = app.clone();
                    let tx = msg_tx.clone();
                    tokio::spawn(async move {
                        fetch_inbox_more(&g, &app2, &tx).await;
                    });
                }
            }
            Cmd::MarkNotifRead(thread_id) => {
                if let Some(g) = gh.clone() {
                    let app2 = app.clone();
                    let tx = msg_tx.clone();
                    tokio::spawn(async move {
                        match g.mark_notification_read(&thread_id).await {
                            Ok(()) => fetch_inbox(&g, &app2, &tx).await,
                            Err(e) => {
                                let lang = lang_of(&app2);
                                send(&tx, Msg::Status(trf(&lang, "mark_read_failed", &[&e.to_string()])));
                            }
                        }
                        send(&tx, Msg::StatusBarRefresh);
                    });
                }
            }
            Cmd::FetchNotifDetail { repo, number, kind } => {
                // spawn：3 个串行网络请求是会阻塞命令循环的重头。消息带身份守卫，
                // 快速连点两条通知时旧结果不上屏（NotifDetail/NotifDetailFailed apply 比对）。
                let g = gh.clone();
                let app2 = app.clone();
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    // 每个出口都必须回送消息：失败也要把预览面板的 loading 置 false，
                    // 否则面板永远停在「加载中」。
                    let Some(g) = &g else {
                        let lang = lang_of(&app2);
                        send(&tx, Msg::NotifDetailFailed {
                            repo,
                            number,
                            err: tr(&lang, "cfg_unconfigured").into(),
                        });
                        return;
                    };
                    // 会话对 PR/Issue 都有；Files/Diff 仅 PullRequest
                    let convo = match g.fetch_pr_convo(&repo, number).await {
                        Ok(c) => c,
                        Err(e) => {
                            let lang = lang_of(&app2);
                            send(&tx, Msg::Status(trf(&lang, "convo_fetch_failed", &[&e.to_string()])));
                            Vec::new()
                        }
                    };
                    let (files, diff) = if kind == "PullRequest" {
                        let files = match g.fetch_pr_files(&repo, number).await {
                            Ok(f) => f,
                            Err(e) => {
                                let lang = lang_of(&app2);
                                send(&tx, Msg::Status(trf(&lang, "files_fetch_failed", &[&e.to_string()])));
                                Vec::new()
                            }
                        };
                        let diff = match g.fetch_diff(&repo, number).await {
                            Ok(d) => d,
                            Err(e) => {
                                let lang = lang_of(&app2);
                                send(&tx, Msg::Status(trf(&lang, "diff_fetch_failed", &[&e.to_string()])));
                                String::new()
                            }
                        };
                        (files, diff)
                    } else {
                        (Vec::new(), String::new())
                    };
                    let convo_entries = plain_convo_entries(&convo);
                    // 会话作者头像：UI 装盒读缓存（miss → 字母降级），随后用现成的标准客户端
                    // 预取补齐（本 task 已在后台，inline await 不堵命令循环）；有新图发回填消息
                    let mut authors: Vec<String> = convo_entries
                        .iter()
                        .map(|e| e.author.clone())
                        .filter(|a| !a.is_empty())
                        .collect();
                    authors.sort();
                    authors.dedup();
                    send(&tx, Msg::NotifDetail {
                        repo: repo.clone(),
                        number,
                        convo: convo_entries,
                        files_json: serde_json::to_string(&files).unwrap_or_else(|_| "[]".into()),
                        diff_files: parse_diff(&diff),
                    });
                    send(&tx, Msg::StatusBarRefresh);
                    if !authors.is_empty() && avatars::prefetch(g, authors).await > 0 {
                        send(&tx, Msg::ConvoAvatars { repo, number });
                    }
                });
            }
            Cmd::OpenDrawer { repo, number, refetch } => {
                open_drawer_work(&app, &msg_tx, &cmd_tx, repo, number, refetch);
            }
            Cmd::CopyReview => {
                // 「复制评审」：get_review SQL + arboard 剪贴板写入都在 worker
                //（Windows 剪贴板打开可能重试阻塞，不占 UI 线程）
                let (sel, review) = {
                    let a = app.lock_recover();
                    let sel = a.selected.clone();
                    let rv = sel.as_ref().and_then(|(repo, number)| {
                        match a.db.get_review(repo, *number) {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::warn!("读取评审失败 {repo} #{number}: {e}");
                                None
                            }
                        }
                    });
                    (sel, rv)
                };
                if let (Some((repo, number)), Some(rv)) = (sel, review) {
                    let md = build_review_markdown(&repo, number, &rv, &lang);
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(&md)) {
                        Ok(()) => {
                            send(&msg_tx, Msg::Status(tr(&lang, "review_copied").into()));
                        }
                        Err(e) => {
                            send(&msg_tx, Msg::Status(trf(&lang, "review_copy_failed", &[&e.to_string()])));
                        }
                    }
                }
            }
            Cmd::CopyLinks(text) => {
                // 多选「复制链接」：文本在 UI 回调拼好，剪贴板写入在 worker
                //（与 CopyReview 同理：Windows 剪贴板打开可能重试阻塞）
                let n = text.lines().count();
                match arboard::Clipboard::new().and_then(|mut c| c.set_text(&text)) {
                    Ok(()) => {
                        send(&msg_tx, Msg::Status(trf(&lang, "links_copied", &[&n.to_string()])));
                    }
                    Err(e) => {
                        send(&msg_tx, Msg::Status(trf(&lang, "review_copy_failed", &[&e.to_string()])));
                    }
                }
            }
            Cmd::LoadReviewPrompt => {
                // 设置页打开时重读自定义评审模板（可能被外部编辑过）；空/缺失 = 默认模板
                let text = std::fs::read_to_string(pr_desk_rs::exe_dir().join("review_prompt.md"))
                    .unwrap_or_default();
                send(&msg_tx, Msg::ReviewPromptLoaded(text));
            }
            Cmd::MarkAllNotifsRead => {
                if let Some(g) = gh.clone() {
                    let app2 = app.clone();
                    let tx = msg_tx.clone();
                    tokio::spawn(async move {
                        match g.mark_all_notifications_read().await {
                            Ok(()) => {
                                let lang = lang_of(&app2);
                                send(&tx, Msg::Status(tr(&lang, "all_marked_read").into()));
                                fetch_inbox(&g, &app2, &tx).await;
                            }
                            Err(e) => {
                                let lang = lang_of(&app2);
                                send(&tx, Msg::Status(trf(&lang, "mark_all_read_failed", &[&e.to_string()])));
                            }
                        }
                        send(&tx, Msg::StatusBarRefresh);
                    });
                }
            }
            Cmd::AddNotifReview { repo, number, title, url } => {
                // 已存在则跳过（upsert 会把已有行的 author/head_sha 等覆盖成空）；
                // 查询失败是错误不是「不存在」：保守按已存在处理（防重复插入）并告警
                let exists = {
                    let a = app.lock_recover();
                    a.db.pr_exists(&repo, number).unwrap_or_else(|e| {
                        tracing::warn!("查询 PR 是否存在失败 {repo} #{number}: {e}");
                        true
                    })
                };
                if exists {
                    send(&msg_tx, Msg::Status(trf(&lang, "already_in_list", &[&repo, &number.to_string()])));
                } else {
                    let ok = {
                        let a = app.lock_recover();
                        a.db.upsert_pr(&serde_json::json!({
                            "repo": repo, "number": number, "title": title,
                            "author": "", "url": url, "head_sha": "",
                            "status": "pending", "updated_at": db::now(),
                            "source": "inbox",
                        })).is_ok()
                    };
                    send(&msg_tx, Msg::Status(if ok {
                        trf(&lang, "added_review", &[&repo, &number.to_string()])
                    } else {
                        trf(&lang, "add_review_failed", &[&repo, &number.to_string()])
                    }));
                    if ok {
                        reload_rows(&app, &msg_tx);
                        // 通知只带 title/url：补拉 author/head_sha 等元数据（入库即补齐，不留残缺行）
                        let _ = cmd_tx.send(Cmd::FetchPrMeta { repo: repo.clone(), number });
                    }
                }
                // 重发当前通知行：apply 重新计算「已加入」标记
                let rows = app.lock_recover().inbox.clone();
                send(&msg_tx, Msg::InboxRows(rows));
            }
            Cmd::RunReview { repo, number } => {
                let (cfg, slots) = {
                    let a = app.lock_recover();
                    (a.cfg.clone(), a.review_slots.clone())
                };
                // LLM 配置缺失：立即提示，不占并发槽
                let Some(llm) = LlmSettings::from_config(&cfg) else {
                    send(&msg_tx, Msg::Status(tr(&lang, "llm_not_configured").into()));
                    continue;
                };
                // 并发槽：同一 PR 重入 / 超额都立即拒绝（槽位由任务内 SlotGuard 释放）
                // 注意：try_acquire 的锁守卫随 let 语句结束即释放，match 臂内可再次加锁
                let acquired = slots.lock_recover().try_acquire(&repo, number);
                match acquired {
                    Ok(()) => {}
                    Err(SlotError::AlreadyRunning) => {
                        send(&msg_tx, Msg::Status(tr(&lang, "review_already_running").into()));
                        continue;
                    }
                    Err(SlotError::AtCapacity) => {
                        let cap = slots.lock_recover().capacity.to_string();
                        send(&msg_tx, Msg::Status(trf(&lang, "review_slots_full", &[&cap])));
                        continue;
                    }
                }
                // 并行评审：spawn 后立即返回，worker 循环继续处理后续命令。
                // 每个任务自建 GitHub 客户端，结束后把配额合并回 app.rate_limit。
                let app2 = app.clone();
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    let _guard = SlotGuard { slots, repo: repo.clone(), number };
                    let rl = match github::GitHub::new(cfg.token(), &cfg.proxy) {
                        Ok(g) => {
                            let h = g.rate_limit_handle();
                            run_review_task(&cfg, &llm, &g, &app2, &tx, &repo, number).await;
                            Some(h)
                        }
                        Err(e) => {
                            send(&tx, Msg::ReviewFailed {
                                repo: repo.clone(),
                                number,
                                err: trf(&cfg.language, "gh_client_init_failed", &[&e.to_string()]),
                            });
                            None
                        }
                    };
                    if let Some(h) = rl {
                        if let Some(n) = *h.lock_recover() {
                            *app2.lock_recover().rate_limit.lock_recover() = Some(n);
                        }
                    }
                });
            }
            Cmd::FetchModels { base, key } => {
                // 设置页「获取」：拉 OpenAI 兼容端点的模型列表（base/key 是弹窗里正在编辑的值）
                let base = if base.trim().is_empty() {
                    app.lock_recover().cfg.llm_base_url.clone()
                } else {
                    base.trim().trim_end_matches('/').to_string()
                };
                let key = if key.trim().is_empty() {
                    app.lock_recover().cfg.llm_key().unwrap_or_default()
                } else {
                    key.trim().to_string()
                };
                // 模型列表请求同属 LLM 流量：走当前配置的代理（弹窗里的代理可能尚未保存，用已生效值）
                let proxy = app.lock_recover().cfg.proxy.clone();
                let tx = msg_tx.clone();
                let lang2 = lang.clone();
                tokio::spawn(async move {
                    send(&tx, Msg::Status(tr(&lang2, "models_fetching").into()));
                    // 弹窗内联反馈：状态栏被设置弹窗遮罩盖住，拉取进度/失败要落到弹窗里
                    send(&tx, Msg::DialogMsg(tr(&lang2, "models_fetching").into()));
                    let url = format!("{base}/models");
                    let result = async {
                        let resp = pr_desk_rs::http_client(&proxy, std::time::Duration::from_secs(15))?
                            .get(&url)
                            .header("Authorization", format!("Bearer {key}"))
                            .send()
                            .await?;
                        if !resp.status().is_success() {
                            anyhow::bail!("HTTP {}", resp.status());
                        }
                        let v: Value = resp.json().await?;
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
                            send(&tx, Msg::ModelsLoaded(ids));
                            send(&tx, Msg::Status(trf(&lang2, "models_fetched", &[&n.to_string()])));
                        }
                        Err(e) => {
                            let msg = trf(&lang2, "models_fetch_failed", &[&e.to_string()]);
                            send(&tx, Msg::Status(msg.clone()));
                            send(&tx, Msg::DialogMsg(msg));
                        }
                    }
                });
            }
            Cmd::FetchPrMeta { repo, number } => {
                // 补齐残缺行（Inbox 入库的 PR 缺 author/head_sha）：拉 GitHub PR meta，
                // 只填空缺字段，成功后刷新列表；面板正开着该 PR 则重新装载。
                // 节流：同一行 300s 内最多一次（兜底防死循环烧穿 API 限额）。
                // spawn 化后改为乐观标记：循环内先记账（防在飞重入），task 失败时移除
                //（拉取失败不该吃满 300s 惩罚窗口）。
                {
                    let mut healed = meta_healed.lock_recover();
                    let recently = healed
                        .get(&(repo.clone(), number))
                        .map(|t| t.elapsed().as_secs() < 300)
                        .unwrap_or(false);
                    if recently {
                        continue;
                    }
                    healed.insert((repo.clone(), number), std::time::Instant::now());
                }
                if let Some(g) = gh.clone() {
                    let app2 = app.clone();
                    let tx = msg_tx.clone();
                    let healed = meta_healed.clone();
                    tokio::spawn(async move {
                        match g.fetch_pr_meta(&repo, number).await {
                            Ok(meta) => {
                                let author = meta["user"]["login"].as_str().unwrap_or("").to_string();
                                let url = meta["html_url"].as_str().unwrap_or("").to_string();
                                let head_sha = meta["head"]["sha"].as_str().unwrap_or("").to_string();
                                let title = meta["title"].as_str().unwrap_or("").to_string();
                                let filled = {
                                    let a = app2.lock_recover();
                                    match a.db.fill_pr_meta_if_empty(&repo, number, &author, &url, &head_sha, &title) {
                                        Ok(f) => f,
                                        Err(e) => {
                                            tracing::warn!("补齐 PR 元数据落库失败 {repo} #{number}: {e}");
                                            false
                                        }
                                    }
                                };
                                if filled {
                                    reload_rows(&app2, &tx);
                                    if panel_matches(&app2, &repo, number) {
                                        send(&tx, Msg::ReopenPanel);
                                    }
                                }
                            }
                            Err(e) => {
                                // 失败：移除节流标记（下次可立即重试）+ 状态栏报错
                                healed.lock_recover().remove(&(repo.clone(), number));
                                let lang = lang_of(&app2);
                                send(&tx, Msg::Status(trf(
                                    &lang,
                                    "meta_fill_failed",
                                    &[&repo, &number.to_string(), &e.to_string()],
                                )));
                            }
                        }
                    });
                } else {
                    // 无 gh 客户端不会发起拉取：刚记的节流标记撤掉
                    meta_healed.lock_recover().remove(&(repo, number));
                }
            }
            Cmd::FetchPrLive { repo, number, parts } => {
                // 面板实时刷新：按位标志惰性拉取——打开面板只带 Checks（2 个请求），
                // 会话/Files 切到对应 tab 才拉。每部分独立 120s 节流；配额 <50 跳过。
                // spawn 化：节流检查 + 乐观记账在循环内做（防在飞重入），网络主体进 task
                let (want_convo, want_files, want_checks) = {
                    let fetched = live_fetched.lock_recover();
                    let fresh = |bit: u8| {
                        fetched
                            .get(&(repo.clone(), number, bit))
                            .map(|t| t.elapsed().as_secs() < 120)
                            .unwrap_or(false)
                    };
                    (
                        parts & PART_CONVO != 0 && !fresh(PART_CONVO),
                        parts & PART_FILES != 0 && !fresh(PART_FILES),
                        parts & PART_CHECKS != 0 && !fresh(PART_CHECKS),
                    )
                };
                let quota_low = app
                    .lock_recover()
                    .rate_limit
                    .lock_recover()
                    .map(|r| r < 50)
                    .unwrap_or(false);
                if (!want_convo && !want_files && !want_checks) || quota_low {
                    if quota_low {
                        send(&msg_tx, Msg::Status(tr(&lang, "quota_low_skip").into()));
                    }
                    continue;
                }
                if let Some(g) = gh.clone() {
                    // 乐观节流记账：spawn 前即刻标记，在飞期间的重复命令直接被节流拒掉；
                    // task 内拉取失败的部分由 task 撤标（失败不吃满 120s 惩罚窗口）。
                    {
                        let mut fetched = live_fetched.lock_recover();
                        let now = std::time::Instant::now();
                        if want_convo {
                            fetched.insert((repo.clone(), number, PART_CONVO), now);
                        }
                        if want_files {
                            fetched.insert((repo.clone(), number, PART_FILES), now);
                        }
                        if want_checks {
                            fetched.insert((repo.clone(), number, PART_CHECKS), now);
                        }
                    }
                    let app = app.clone();
                    let msg_tx = msg_tx.clone();
                    let live_fetched = live_fetched.clone();
                    tokio::spawn(async move {
                    // meta 只在 checks 部分需要（head.sha → check runs；state/updated → 面板头部）
                    let meta = if want_checks {
                        match g.fetch_pr_meta(&repo, number).await {
                            Ok(m) => Some(m),
                            Err(e) => {
                                send(&msg_tx, Msg::Status(trf(
                                    &lang,
                                    "live_refresh_failed",
                                    &[&repo, &number.to_string(), &e.to_string()],
                                )));
                                // meta 失败整轮放弃（原语义）：撤掉本次记的所有节流标记，
                                // 下次操作可立即重试（失败不吃满 120s 惩罚窗口）
                                let mut f = live_fetched.lock_recover();
                                f.remove(&(repo.clone(), number, PART_CONVO));
                                f.remove(&(repo.clone(), number, PART_FILES));
                                f.remove(&(repo.clone(), number, PART_CHECKS));
                                return;
                            }
                        }
                    } else {
                        None
                    };
                    let (convo_pair, files, checks) = tokio::join!(
                        async {
                            if want_convo {
                                // 评论失败 → None（整块不碰面板缓存）；评审失败只少评审条目
                                match g.fetch_pr_convo(&repo, number).await {
                                    Ok(c) => {
                                        let r = g.fetch_pr_reviews(&repo, number).await
                                            .map_err(|e| tracing::debug!("实时评审拉取失败 {repo} #{number}: {e}"))
                                            .unwrap_or_default();
                                        Some((c, r))
                                    }
                                    Err(e) => {
                                        tracing::debug!("实时会话拉取失败 {repo} #{number}: {e}");
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        },
                        async {
                            if want_files {
                                match g.fetch_pr_files(&repo, number).await {
                                    Ok(f) => Some(f),
                                    Err(e) => {
                                        tracing::debug!("实时文件列表拉取失败 {repo} #{number}: {e}");
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        },
                        async {
                            let sha = meta
                                .as_ref()
                                .map(|m| m["head"]["sha"].as_str().unwrap_or("").to_string())
                                .unwrap_or_default();
                            if !want_checks || sha.is_empty() {
                                None
                            } else {
                                g.fetch_pr_checks(&repo, &sha).await
                                    .map_err(|e| tracing::debug!("实时 Checks 拉取失败 {repo} #{number}: {e}"))
                                    .ok()
                            }
                        },
                    );

                    // 撤掉失败部分的乐观节流标记（下次可立即重试）；
                    // checks 部分的代价在 meta 请求上，meta 成功即算 checks 已拉
                    {
                        let mut f = live_fetched.lock_recover();
                        if convo_pair.is_none() {
                            f.remove(&(repo.clone(), number, PART_CONVO));
                        }
                        if files.is_none() {
                            f.remove(&(repo.clone(), number, PART_FILES));
                        }
                        if meta.is_none() {
                            f.remove(&(repo.clone(), number, PART_CHECKS));
                        }
                    }

                    let mut ci_out: Option<String> = None;
                    let mut state_out: Option<String> = None;
                    let mut updated_out: Option<String> = None;
                    let mut checks_json_out: Option<String> = None;
                    let mut convo_out: Option<Vec<PlainConvoEntry>> = None;
                    let mut convo_authors: Vec<String> = Vec::new();
                    let mut files_json_out: Option<String> = None;
                    let mut dirty = false;

                    // 落库（直改 DB，不进 outbox：非用户改动，不该推送）
                    if let Some(meta) = &meta {
                        let author = meta["user"]["login"].as_str().unwrap_or("").to_string();
                        let url = meta["html_url"].as_str().unwrap_or("").to_string();
                        let title = meta["title"].as_str().unwrap_or("").to_string();
                        let sha = meta["head"]["sha"].as_str().unwrap_or("").to_string();
                        let meta_updated = meta["updated_at"].as_str().unwrap_or("").to_string();
                        // GitHub 终态（merged/closed）覆盖 status：分组靠它把已完结 PR 移出
                        // 待评审区；open 不回写（避免冲掉 reviewed/failed 等工作流状态）
                        let gh_state = if meta["merged_at"].is_null() {
                            meta["state"].as_str().unwrap_or("open").to_string()
                        } else {
                            "merged".to_string()
                        };
                        {
                            let a = app.lock_recover();
                            let _ = a.db.fill_pr_meta_if_empty(&repo, number, &author, &url, &sha, &title);
                            if gh_state == "merged" || gh_state == "closed" {
                                let _ = a.db.conn.execute(
                                    "UPDATE prs SET status=?1 WHERE repo=?2 AND number=?3 AND status NOT IN ('merged','closed')",
                                    rusqlite::params![gh_state, repo, number],
                                );
                            }
                            match &checks {
                                Some((ci, _)) => {
                                    let _ = a.db.conn.execute(
                                        "UPDATE prs SET ci_status=?1, updated_at=?2 WHERE repo=?3 AND number=?4",
                                        rusqlite::params![ci, meta_updated, repo, number],
                                    );
                                    ci_out = Some(ci.clone());
                                }
                                None => {
                                    // Checks 拉取失败：不覆盖 ci_status，面板回退库内现值（不清空指示）
                                    if !meta_updated.is_empty() {
                                        let _ = a.db.conn.execute(
                                            "UPDATE prs SET updated_at=?1 WHERE repo=?2 AND number=?3",
                                            rusqlite::params![meta_updated, repo, number],
                                        );
                                    }
                                    ci_out = a.db.conn.query_row(
                                        "SELECT ci_status FROM prs WHERE repo=?1 AND number=?2",
                                        rusqlite::params![repo, number],
                                        |r| r.get::<_, Option<String>>(0),
                                    ).ok().flatten();
                                }
                            }
                        }
                        state_out = Some(gh_state.clone());
                        updated_out = Some(crate::state::rel_time(&meta_updated));
                        checks_json_out = checks.map(|(_, items)| {
                            serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
                        });
                        dirty = true;
                    }
                    if let Some((convo, reviews)) = convo_pair {
                        if let Some(rd) = derive_review_decision(&reviews) {
                            let a = app.lock_recover();
                            let _ = a.db.conn.execute(
                                "UPDATE prs SET review_decision=?1 WHERE repo=?2 AND number=?3",
                                rusqlite::params![rd, repo, number],
                            );
                        }
                        // 会话 = 评论 + 评审（按时间升序）
                        let mut convo_items: Vec<Value> = convo
                            .into_iter()
                            .map(|mut c| {
                                c["kind"] = serde_json::json!("comment");
                                c
                            })
                            .collect();
                        convo_items.extend(reviews.into_iter().map(|mut r| {
                            r["kind"] = serde_json::json!("review");
                            r
                        }));
                        convo_items.sort_by(|x, y| {
                            x["time"].as_str().unwrap_or("~").cmp(y["time"].as_str().unwrap_or("~"))
                        });
                        convo_out = Some(plain_convo_entries(&convo_items));
                        // 会话作者收集（含评论与评审）：头像预取在 PanelLive 回送后做，
                        // 装盒时缓存 miss 的字母降级由随后的 ConvoAvatars 原地回填
                        convo_authors = convo_out
                            .as_ref()
                            .map(|es| es.iter().map(|e| e.author.clone()).collect())
                            .unwrap_or_default();
                        dirty = true;
                    }
                    if let Some(files) = &files {
                        // 增删行数落库：列表 +/- 列之前只来自评审记录，没评审过的 PR 永远 +0/-0
                        let (adds, dels) = files.iter().fold((0i64, 0i64), |(a, d), f| {
                            (
                                a + f["adds"].as_i64().unwrap_or(0),
                                d + f["dels"].as_i64().unwrap_or(0),
                            )
                        });
                        {
                            let a = app.lock_recover();
                            let _ = a.db.conn.execute(
                                "UPDATE prs SET additions=?1, deletions=?2 WHERE repo=?3 AND number=?4",
                                rusqlite::params![adds, dels, repo, number],
                            );
                        }
                        files_json_out =
                            Some(serde_json::to_string(files).unwrap_or_else(|_| "[]".into()));
                        dirty = true;
                    }
                    if dirty {
                        reload_rows(&app, &msg_tx);
                        send(&msg_tx, Msg::PanelLive {
                            repo: repo.clone(),
                            number,
                            ci: ci_out,
                            state: state_out,
                            updated: updated_out,
                            convo: convo_out,
                            checks_json: checks_json_out,
                            files_json: files_json_out,
                        });
                        send(&msg_tx, Msg::StatusBarRefresh);
                    }
                    // 会话作者头像预取回填（与 open_drawer / NotifDetail 同款）：
                    // 首屏会话可能来自面板缓存（extras_detail 滞后），新条目由本路径带进来——
                    // 不在这里预取的话这些作者永远停在字母降级。跟 FetchPrLive 的 120s
                    // 节流同频，且 prefetch 内部按 TTL 过滤新鲜缓存，不放大请求量。
                    if !convo_authors.is_empty() {
                        convo_authors.sort();
                        convo_authors.dedup();
                        if avatars::prefetch(&g, convo_authors).await > 0 {
                            send(&msg_tx, Msg::ConvoAvatars { repo, number });
                        }
                    }
                    });
                }
            }
            Cmd::SwitchProfile(name) => {
                // 校验：档案必须存在且名字合法（防路径穿越）；非法则回填纠正 UI 选中
                let valid = {
                    let a = app.lock_recover();
                    a.cfg.has_profile(&name)
                        && (name == "local" || pr_desk_rs::config::valid_profile_name(&name))
                };
                if !valid {
                    send(&msg_tx, Msg::Status(trf(&lang, "profile_unknown", &[&name])));
                    let (names, active) = {
                        let a = app.lock_recover();
                        (a.cfg.profile_names(), a.cfg.active_profile.clone())
                    };
                    send(&msg_tx, Msg::Profiles { names, active });
                    continue;
                }
                if app.lock_recover().cfg.active_profile == name {
                    continue; // 同档重选：无操作
                }
                // 换库：local → pr-desk.db；其它档 → profiles/<name>.db
                //（sync 游标/outbox 在库文件内，换库天然隔离；远程新库游标为空 = 首次全量拉）
                let db_path = profile_db_path(&pr_desk_rs::data_dir(), &name);
                if let Some(parent) = db_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let new_db = match Db::open(&db_path) {
                    Ok(d) => d,
                    Err(e) => {
                        send(&msg_tx, Msg::Status(trf(
                            &lang,
                            "profile_db_failed",
                            &[&name, &e.to_string()],
                        )));
                        // 换库失败：回填纠正 UI 选中（顶栏是乐观切换）
                        let (names, active) = {
                            let a = app.lock_recover();
                            (a.cfg.profile_names(), a.cfg.active_profile.clone())
                        };
                        send(&msg_tx, Msg::Profiles { names, active });
                        continue;
                    }
                };
                {
                    let mut a = app.lock_recover();
                    a.cfg.active_profile = name.clone();
                    if let Err(e) = a.cfg.save(&cfg_path) {
                        // 落盘失败不阻断切换（内存态已切），仅提示
                        send(&msg_tx, Msg::Status(trf(&lang, "cfg_save_failed", &[&e.to_string()])));
                    }
                    a.db = new_db; // 旧 Db 随 drop 关闭连接
                    a.selected = None;
                    a.selection.clear();
                    a.anchor = None;
                    a.inbox_detail = None;
                    a.rows.clear();
                    a.flat.clear();
                    a.view_visible.clear();
                    a.total = 0;
                    *a.last_sync.lock_recover() = None; // 「已同步 X 前」不跨档案沿用
                }
                // SyncClient 按新档案重建（local → enabled()=false）
                sync_client = {
                    let a = app.lock_recover();
                    sync::SyncClient::new(&a.cfg)
                };
                let names = app.lock_recover().cfg.profile_names();
                send(&msg_tx, Msg::Profiles { names, active: name.clone() });
                send(&msg_tx, Msg::ClosePanel);
                send(&msg_tx, Msg::Status(trf(&lang, "profile_switched", &[&name])));
                // 走现有刷新路径：远程档 do_sync 全量拉；本地档 GitHub 状态刷新。
                // 但先让 UI 立即落地：换库后马上重载行 + 回放上一条通知列表，
                // 网络刷新 spawn 到后台，切换档案不再卡 worker 队列
                reload_rows(&app, &msg_tx);
                {
                    let cached = app.lock_recover().inbox.clone();
                    send(&msg_tx, Msg::InboxRows(cached));
                }
                if let Some(g) = &gh {
                    let g = g.clone();
                    let sc = sync_client.clone();
                    let app2 = app.clone();
                    let msg_tx2 = msg_tx.clone();
                    let prof = name.clone(); // 任务归属档案：在途结果只许写回该档案的库
                    tokio::spawn(async move {
                        if sc.enabled() {
                            fetch_inbox(&g, &app2, &msg_tx2).await;
                            do_sync(&app2, &sc, &msg_tx2, &prof).await;
                        } else {
                            refresh_from_github(&g, &app2, &msg_tx2, &prof).await;
                            fetch_inbox(&g, &app2, &msg_tx2).await;
                        }
                    });
                }
            }
            Cmd::ReloadConfig => {
                // 设置保存后热生效：按新配置重建客户端，随后立即刷新 + 同步
                let cfg = app.lock_recover().cfg.clone();
                match github::GitHub::new(cfg.token(), &cfg.proxy) {
                    Ok(g) => {
                        // 重建客户端后替换配额共享句柄
                        app.lock_recover().rate_limit = g.rate_limit_handle();
                        gh = Some(g);
                    }
                    Err(e) => {
                        send(&msg_tx, Msg::Status(trf(&lang, "gh_client_rebuild_failed", &[&e.to_string()])));
                    }
                }
                sync_client = sync::SyncClient::new(&cfg);
                send(&msg_tx, Msg::Status(tr(&lang, "cfg_reloaded").into()));
                // 复用现有流程：sync 模式同步服务器列表；本地模式刷新在库行状态
                let for_profile = cfg.active_profile.clone();
                if let Some(g) = &gh {
                    if sync_client.enabled() {
                        fetch_inbox(g, &app, &msg_tx).await;
                        do_sync(&app, &sync_client, &msg_tx, &for_profile).await;
                    } else {
                        refresh_from_github(g, &app, &msg_tx, &for_profile).await;
                        fetch_inbox(g, &app, &msg_tx).await;
                    }
                }
            }
        }
    }
    tracing::info!("worker 退出（Shutdown / 命令通道断开）");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 微型 sync 服务器：收到一次请求后延迟 delay 再返回 body（模拟网络在途）。
    fn fake_sync_server(delay: std::time::Duration, body: String) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let mut got: Vec<u8> = Vec::new();
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            got.extend_from_slice(&buf[..n]);
                            if got.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                std::thread::sleep(delay);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn inflight_sync_must_not_write_into_switched_profile_db() {
        // do_sync 以 default 档案发起；拉取在途（假服务器 sleep 中）时模拟
        // SwitchProfile 切到 local（换库）。断言切后的 local 库不出现
        // server 行与同步游标——用户实测 local 库被灌入 108 个服务器 PR。
        let body = serde_json::json!({
            "prs": [{"repo": "o/r", "number": 999, "title": "leaked"}],
            "reviews": [],
            "todos": [],
            "server_time": "2026-11-03T16:00:00",
        })
        .to_string();
        let url = fake_sync_server(std::time::Duration::from_millis(400), body);

        let app = Arc::new(Mutex::new(App::new(Config::default(), Db::open_memory().unwrap())));
        {
            let mut a = app.lock_recover();
            a.cfg.server_url = url;
            a.cfg.sync_token = "tok".into();
            a.cfg.active_profile = "default".into();
        }
        let sc = sync::SyncClient::new(&app.lock_recover().cfg);
        assert!(sc.enabled());

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app2 = app.clone();
            let (msg_tx, _rx) = mpsc::channel::<Msg>();
            let h = tokio::spawn(async move { do_sync(&app2, &sc, &msg_tx, "default").await });
            // 拉取在途：用户切回 local —— 换库
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            {
                let mut a = app.lock_recover();
                a.cfg.active_profile = "local".into();
                a.db = Db::open_memory().unwrap();
            }
            h.await.unwrap();
        });
        let a = app.lock_recover();
        assert!(
            a.db.list_pr_keys().unwrap().is_empty(),
            "在途 sync 把服务器数据写进了切换后的 local 库（串库）"
        );
        assert!(a.db.get_cursor().unwrap().is_none(), "sync 游标泄漏进了 local 库");
    }
}
