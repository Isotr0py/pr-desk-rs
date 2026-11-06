//! PR Desk 桌面端（第一阶段）：本地 SQLite + GitHub 拉取 + 插件同步 + Slint UI。
//! 纯逻辑在 lib（pr_desk_rs）里，`cargo test --lib` 不需要编译 Slint。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use slint::{Model as _, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

slint::include_modules!();

mod avatars;
mod model;

use pr_desk_rs::config::{Config, Profile};
use pr_desk_rs::db::{self, Db};
use pr_desk_rs::i18n::{tr, trf};
use pr_desk_rs::reviewer::LlmSettings;
use pr_desk_rs::selection;
use pr_desk_rs::{diffview, github, ocr, reviewer, sync};

// ── 后台命令 / 回送消息 ──

/// 互斥锁加锁：锁中毒时恢复内层值继续用，不让一次锁内 panic 拖死整个 app。
/// 恢复语义安全的前提：锁内路径已把可避免的 unwrap/panic 改成 graceful 降级
///（模型行/远程数据不是不变量），中毒不等于数据损坏。
trait LockRecover<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for Mutex<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T> {
        Mutex::lock(self).unwrap_or_else(|e| e.into_inner())
    }
}

enum Cmd {
    RefreshGithub,
    Sync,
    FetchDiff { repo: String, number: i64 },
    FetchInbox,                 // 拉取 GitHub 通知（RefreshGithub/Sync 也会顺带触发）
    FetchInboxMore,             // Inbox「加载更多」：拉下一页追加
    MarkNotifRead(String),      // 标记通知线程已读，完成后重新拉取
    MarkAllNotifsRead,          // PUT /notifications 全部已读，完成后重新拉取
    AddNotifReview { repo: String, number: i64, title: String, url: String },  // Inbox「+ 预评审」入库
    FetchNotifDetail { repo: String, number: i64, kind: String },  // Inbox 预览面板：拉会话/Files/Diff
    // 详情抽屉装配（原 UI 线程 open_drawer）：DB 读写 + diff 解析全在 worker，
    // UI 线程只把 Msg::Panel/DiffParsed 装盒。refetch=false 用于 ReopenPanel（防 FetchPrMeta 死循环）
    OpenDrawer { repo: String, number: i64, refetch: bool },
    CopyReview,        // 详情面板「复制评审」：get_review SQL + 剪贴板写入都在 worker
    CopyLinks(String), // 多选「复制链接」：换行拼接的 URL 文本，剪贴板写入在 worker
    LoadReviewPrompt,  // 设置页打开时重读 exe 旁 review_prompt.md（可能被外部编辑过）
    ReloadConfig,   // 设置保存后热生效：重建客户端并立即刷新+同步
    SwitchProfile(String),  // 切换数据源档案（"local"/"default"/自定义名）：换库 + 重建同步客户端
    RunReview { repo: String, number: i64 },  // 详情面板「AI 评审」：LLM 评审并入库
    FetchModels { base: String, key: String },  // 设置页「获取」：拉 OpenAI 兼容端点的模型列表
    FetchPrMeta { repo: String, number: i64 },  // 补齐残缺行的 author/url/head_sha（Inbox 入库的行缺这些）
    FetchPrLive { repo: String, number: i64, parts: u8 },  // 面板实时拉取（parts 位标志：1=会话+评审 2=Files 4=Checks+meta；按 tab 惰性拉取）
}

/// FetchPrLive 的 parts 位标志：按 tab 惰性拉取（打开面板只带 CHECKS，会话/Files 切到才拉）。
const PART_CONVO: u8 = 1;   // 会话 tab：评论 + 评审意见
const PART_FILES: u8 = 2;   // Files tab
const PART_CHECKS: u8 = 4;  // Checks tab / 打开面板（含 meta：状态徽章/更新时间）

/// 状态栏配置状态文本（语言取 cfg.language）。
fn config_status(cfg: &Config) -> String {
    let lang = cfg.language.as_str();
    if cfg.is_unconfigured() {
        return tr(lang, "cfg_unconfigured").into();
    }
    let mut parts = vec![trf(lang, "cfg_repos", &[&cfg.repos.len().to_string()])];
    parts.push(if cfg.active_sync().is_none() {
        tr(lang, "cfg_sync_off").into()
    } else {
        tr(lang, "cfg_sync_on").into()
    });
    parts.join(" · ")
}

/// 当前界面语言（"zh"/"en"），从配置读取。
fn lang_of(app: &Arc<Mutex<App>>) -> String {
    app.lock_recover().cfg.language.clone()
}

enum Msg {
    Status(String),
    // Slint 模型类型（SharedString/Image 基于 Rc）不能跨线程：
    // 行数据以 serde_json（Send）过界，PrListItem（摊平模型）在 UI 线程装配。
    Rows(Vec<serde_json::Value>),
    // 搜索框过滤词变化（UI 回调 send_msg 进来，apply_msg 里重建行模型）
    FilterChanged(String),
    ViewPrefsChanged,  // 排序方式 / 高分快筛变化（重算过滤视图并重建行模型）
    // 只重组状态栏（配额 / 同步时间变化，不改临时消息）
    StatusBarRefresh,
    // 详情面板：文本/JSON 字符串过界；CheckRun/FileChange 模型在 UI 线程装配；
    // 会话条目已在 worker 完成 markdown 解析（PlainConvoEntry 纯数据可跨线程）
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
        convo: Vec<PlainConvoEntry>,
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
    // diff 的结构化解析结果（worker 解析完成，大 PR 几万行不占 UI 线程；UI 只装盒 VecModel）。
    // 携带身份：面板已切走则不上屏（diff 缓存已在 worker 落库，下次打开能用）
    DiffParsed { repo: String, number: i64, files: Vec<DiffFileData> },
    // Inbox 通知行：serde_json 过界，NotifRow 模型在 UI 线程装配
    InboxRows(Vec<serde_json::Value>),
    // Inbox 预览面板：会话条目已在 worker 完成 markdown 解析；Files 保持 JSON 过界；
    // diff 已解析（标题/类型在点击时已由 on_inbox_activated 设置，这里只回送内容数据）。
    // 携带身份：apply 前与 app.inbox_detail 比对，快速连点两条通知时旧结果丢弃
    NotifDetail {
        repo: String,
        number: i64,
        convo: Vec<PlainConvoEntry>,
        files_json: String,
        diff_files: Vec<DiffFileData>,
    },
    // Inbox 预览拉取失败：身份匹配时关加载态并给状态栏错误（面板不再永远停在「加载中」）
    NotifDetailFailed { repo: String, number: i64, err: String },
    // GitHub 刷新完成：停顶栏转圈 + 复位连点去重标志 + 状态栏结果（open PR 数）
    RefreshDone(usize),
    // 同步流程结束（无论成败/跳过）：停顶栏转圈 + 复位去重标志；结果文案由 do_sync 自己的 Status 负责
    SyncFinished,
    // 设置弹窗内发起操作的进度/失败：写到弹窗内联错误行（状态栏被弹窗遮罩盖住看不见）
    DialogMsg(String),
    // Inbox 首次加载态（发 fetch_inbox 置 true；回 rows / 失败 / 无 token 时置 false）
    InboxLoading(bool),
    // 设置页模型列表拉取结果（id 已排序去重）
    ModelsLoaded(Vec<String>),
    // 数据源档案回填：名单 + 当前选中（启动/切换/增删档案后）
    Profiles { names: Vec<String>, active: String },
    // 设置页档案名校正：新增档案非法时把 settings_profile 写回之前的有效值
    //（Slint 侧在 profile_add 返回后才写 settings_profile，回调内直接 set 会被覆盖，故走消息延迟纠正）
    SettingsProfile(String),
    // 行元数据补齐完成：若面板正开着该 PR，重新装载面板（worker 只在成功补齐后发）
    ReopenPanel,
    // 设置页评审模板重读结果（worker 读盘，避免 UI 线程阻塞 IO）
    ReviewPromptLoaded(String),
    // 实时拉取完成：面板正开着该 PR 时覆盖对应区块（None = 该区块本次未拉，不动）；
    // 列表行已随 worker 落库后的 reload_rows 刷新
    PanelLive {
        repo: String,
        number: i64,
        ci: Option<String>,
        state: Option<String>,
        updated: Option<String>,
        convo: Option<Vec<PlainConvoEntry>>,
        checks_json: Option<String>,
        files_json: Option<String>,
    },
}

/// 会话条目展示数据（纯 Rust 数据可跨线程；markdown 在 worker 侧已解析成 blocks，
/// UI 线程只做装盒——大会话 PR 几十条评论的 parse_markdown 不再占 UI 线程）。
/// text 保留原文（blocks 为空时 UI 回退显示它）。
struct PlainConvoEntry {
    kind: String,
    author: String,
    time: String,
    text: String,
    blocks: Vec<pr_desk_rs::markdown::MdBlockOut>,
}

/// OCR 行级评论展示数据（纯 Rust 字符串可跨线程；Slint OcrComment 在 UI 线程装配）。
struct OcrCommentRow {
    severity: String,
    category: String,
    location: String,   // 如 "src/foo.rs:42-58"
    content: String,
}

/// diff 文件的结构化解析结果（纯 Rust 数据可跨线程；Slint DiffRow 在 UI 线程装盒）。
/// lines 的 i32 是真实行号（0 = 无行号，见 diffview::numbered_lines）。
/// Clone：UI 侧 diff 模型只装「摊平后的可见行」，折叠状态翻转要由缓存重建。
#[derive(Clone)]
struct DiffFileData {
    path: String,
    lines: Vec<(diffview::LineKind, String, i32)>,
    expanded: bool,
}

/// 单文件渲染行数上限（超出出截断提示行；替代旧 DiffView 的 max_render_lines 属性）
const DIFF_MAX_RENDER_LINES: usize = 2000;

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

/// 评审分项键名 → 界面语言名（未知键原样展示）。
fn dim_label<'a>(key: &'a str, lang: &str) -> std::borrow::Cow<'a, str> {
    match key {
        "readability" => tr(lang, "dim_readability").into(),
        "correctness_risk" => tr(lang, "dim_correctness_risk").into(),
        "test_coverage" => tr(lang, "dim_test_coverage").into(),
        "security" => tr(lang, "dim_security").into(),
        other => other.into(),
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

/// 评审 JSON → 可粘贴到 GitHub 评论的 Markdown 全文（评分/维度/摘要/风险文件/行级评论）。
fn build_review_markdown(repo: &str, number: i64, rv: &serde_json::Value, lang: &str) -> String {
    let mut s = format!("## Review: {repo}#{number}\n\n");
    if let Some(score) = rv["score"].as_f64() {
        s.push_str(&format!("**Score: {score:.1}/10**"));
        if let Some(sz) = rv["size_label"].as_str().filter(|x| !x.is_empty()) {
            s.push_str(&format!(" · {sz}"));
            if let (Some(a), Some(d), Some(f)) =
                (rv["additions"].as_i64(), rv["deletions"].as_i64(), rv["files_changed"].as_i64())
            {
                s.push_str(&format!(" (+{a}/-{d}, {f} files)"));
            }
        }
        s.push_str("\n\n");
    }
    // dimensions 在库里是 JSON 字符串（尺寸小，解析失败就跳过整节）
    if let Some(dims) = rv["dimensions"]
        .as_str()
        .and_then(|d| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(d).ok())
        .filter(|d| !d.is_empty())
    {
        for (k, v) in &dims {
            let val = v.as_f64().map(|x| format!("{x}")).unwrap_or_default();
            s.push_str(&format!("- {}: {val}\n", dim_label(k, lang)));
        }
        s.push('\n');
    }
    if let Some(sum) = rv["summary"].as_str().filter(|x| !x.is_empty()) {
        s.push_str("### Summary\n\n");
        s.push_str(sum);
        s.push_str("\n\n");
    }
    if let Some(fl) = rv["flagged_files"]
        .as_str()
        .and_then(|f| serde_json::from_str::<Vec<serde_json::Value>>(f).ok())
        .filter(|v| !v.is_empty())
    {
        s.push_str("### Flagged files\n\n");
        for f in fl {
            s.push_str(&format!(
                "- `{}` — {}\n",
                f["path"].as_str().unwrap_or("?"),
                f["issue"].as_str().unwrap_or("")
            ));
        }
        s.push('\n');
    }
    if let Some(comments) = rv["comments"].as_array().filter(|c| !c.is_empty()) {
        s.push_str("### Line comments\n\n");
        for c in comments {
            s.push_str(&format!(
                "- **[{}]** `{}`: {}\n",
                c["severity"].as_str().unwrap_or("other"),
                ocr_comment_location(c),
                c["content"].as_str().unwrap_or("")
            ));
        }
    }
    s.trim_end().to_string() + "\n"
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
/// lang："zh"/"en"（分隔符与「个文件」文案随语言切换）。
fn size_line(rv: &serde_json::Value, lang: &str) -> String {
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
        parts.push(trf(lang, "size_files", &[&f.to_string()]));
    }
    if parts.is_empty() {
        label.to_string()
    } else {
        let joined = parts.join(tr(lang, "list_sep"));
        if label.is_empty() {
            trf(lang, "size_wrap_fmt", &[&joined])
        } else {
            trf(lang, "size_full_fmt", &[label, &joined])
        }
    }
}

fn review_props(review: Option<&serde_json::Value>, lang: &str) -> ReviewProps {
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
        .map(|(k, v)| format!("{}  {}/10", dim_label(k, lang), v))
        .collect();
    let dimensions = dims
        .iter()
        .map(|(k, v)| (dim_label(k, lang).to_string(), v.as_f64().unwrap_or(0.0) as f32))
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
        size_line: size_line(rv, lang),
        flagged,
        reviewed_at: rv["reviewed_at"].as_str().unwrap_or("").to_string(),
        comments: ocr_comment_rows(rv),
    }
}

/// 详情面板当前是否正打开指定 PR（评审进度/结果只刷对应面板）。
/// 选中态存身份（repo, number），刷新后行序变化/行被移除都不影响匹配。
fn panel_matches(app: &Arc<Mutex<App>>, repo: &str, number: i64) -> bool {
    let a = app.lock_recover();
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
        self.slots.lock_recover().release(&self.repo, self.number);
    }
}

struct App {
    cfg: Config,
    db: Db,
    rows: Vec<serde_json::Value>,           // 扁平数据行（按身份查找，见 find_row）
    view: Vec<usize>,                       // 过滤后可见行 → rows 下标（UI 下标经它换算）
    view_keys: Vec<(String, i64)>,          // UI 行序列（build_items 分组后，含折叠组的行）→ 身份；点击映射以此为准
    view_visible: Vec<(String, i64)>,       // 可见行（折叠组的行除外）的展示序 → 身份；j/k 键盘导航以此为准
    collapsed: HashSet<String>,             // 折叠的组 key（置顶/看板/…）；跨 rebuild 保留
    filter: String,                         // 当前过滤词（空 = 全量）
    sort_col: String,                       // 组内排序列（默认 "upd" = 更新时间降序，即 SQL 输入序）
    sort_asc: bool,                         // 排序方向（false = 降序）
    min_score: f32,                         // 高分快筛阈值（0 = 关；8 = 只看 ≥8.0）
    selected: Option<(String, i64)>,        // 选中 PR 身份（repo, number）；刷新后行序变化也不漂移
    selection: Vec<(String, i64)>,          // 多选集合（Ctrl 点选/Shift 范围选，保持点选顺序；纯左键点击清空）
    anchor: Option<(String, i64)>,          // Shift 范围选锚点（存身份而非下标：刷新后下标漂移、身份不漂）
    status_msg: String,                     // 状态栏临时消息（最近一条 Status）
    status_gen: u64,                        // 状态栏临时消息序号（自动清除定时器的竞态守卫）
    gh_refreshing: bool,                    // GitHub 刷新在途（顶栏转圈 + 连点去重）
    sync_inflight: bool,                    // 同步在途（顶栏转圈 + 连点/定时去重）
    inbox: Vec<serde_json::Value>,          // 最近一批通知行（未过滤；inbox_repos 下拉由此去重）
    inbox_view: Vec<serde_json::Value>,     // 仓库过滤后的通知行（inbox_activated 按下标取 thread id）
    inbox_filter: String,                   // Inbox 仓库过滤（"" = 全部）
    inbox_all: bool,                        // 通知拉取范围：true = 含已读（&all=true），false = 只看未读
    inbox_page: u32,                        // 已加载到的页码（每页 50 条；「加载更多」追加下一页）
    inbox_has_more: bool,                   // 是否还有下一页可加载
    inbox_loading_more: bool,               // 「加载更多」进行中（防重入）
    inbox_detail: Option<(String, i64, String, String)>,  // 预览面板当前行（repo, number, kind, url）
    diff_data: Vec<DiffFileData>,           // 详情面板当前 diff 缓存（折叠翻转重建模型用）
    nd_diff_data: Vec<DiffFileData>,        // Inbox 预览面板当前 diff 缓存（同上）
    total: usize,                           // 全部行数（状态栏「共 N 个 PR」）
    rate_limit: Arc<Mutex<Option<u64>>>,    // GitHub X-RateLimit-Remaining 共享状态
    last_sync: sync::SharedLastSync,        // 最近同步成功时间
    review_slots: Arc<Mutex<ReviewSlots>>,  // 预评审并发槽（worker 与 UI 侧 open_drawer 共享）
}

/// 状态栏组合逻辑已内联进 update_status_bar（三段：同步/配额/总数）。
fn update_status_bar(ui: &MainWindow, app: &Arc<Mutex<App>>) {
    let a = app.lock_recover();
    let lang = a.cfg.language.as_str();
    // 状态栏三段：同步/临时消息 · API 配额 · PR 总数
    let sync_text = if !a.status_msg.is_empty() {
        a.status_msg.clone()
    } else if a.cfg.is_unconfigured() {
        // 未配置：临时消息 6s 自动清除后仍要留住「点右上角设置」的引导提示
        tr(lang, "cfg_unconfigured").into()
    } else if a.cfg.active_sync().is_none() {
        // 未配置同步：明确提示本地模式，不留空白
        tr(lang, "sb_local_mode").into()
    } else {
        sync::last_sync_text(&a.last_sync, lang).unwrap_or_else(|| tr(lang, "sb_wait_sync").into())
    };
    let quota = a.rate_limit.lock_recover()
        .map(|n| trf(lang, "sb_api_quota", &[&n.to_string()]))
        .unwrap_or_default();
    ui.set_sb_sync(sync_text.into());
    ui.set_sb_quota(quota.into());
    ui.set_sb_total(a.total as i32);
}

/// 状态栏临时消息：覆盖显示，6 秒后自动清除（恢复「已同步 X 前」等常态文本）。
/// 竞态守卫：每条消息递增 status_gen，旧定时器到期时序号不匹配则不清（防旧定时器清新消息）。
fn set_transient_status(ui: &MainWindow, app: &Arc<Mutex<App>>, s: String) {
    let seq = {
        let mut a = app.lock_recover();
        a.status_msg = s;
        a.status_gen = a.status_gen.wrapping_add(1);
        a.status_gen
    };
    update_status_bar(ui, app);
    let weak = ui.as_weak();
    let app = app.clone();
    slint::Timer::single_shot(std::time::Duration::from_secs(6), move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut a = app.lock_recover();
        if a.status_gen != seq {
            return; // 期间来了新消息：本定时器已失效
        }
        a.status_msg.clear();
        drop(a);
        update_status_bar(&ui, &app);
    });
}

/// 当前过滤词下可见行的原始下标。
fn filtered_view(rows: &[serde_json::Value], filter: &str, min_score: f32) -> Vec<usize> {
    let q = filter.trim().to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| {
            // 高分快筛：min_score > 0 时只保留达到阈值的行（未评审 score=0 被滤掉）
            if min_score > 0.0 && (r["score"].as_f64().unwrap_or(0.0) as f32) < min_score {
                return false;
            }
            q.is_empty() || model::row_matches(r, &q)
        })
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
    let a = app.lock_recover();
    // 注意：必须用「实际发给 UI 的分组后顺序」（view_keys）——build_models 会按
    // Pinned/看板/待评审/已评审重排，与 a.view（数据库排序）不是同一个序。
    a.view_keys.get(view_idx).cloned()
}

fn main() -> anyhow::Result<()> {
    // Windows 上 wgpu 默认可能挑 Vulkan；远控/抓屏（Parsec、OBS 等）对 Vulkan 呈现
    // 的支持很差（卡顿/黑屏），DX12 走 DXGI 是它们支持最好的路径。环境变量可覆盖。
    #[cfg(windows)]
    if std::env::var_os("WGPU_BACKEND").is_none() {
        unsafe { std::env::set_var("WGPU_BACKEND", "dx12") };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_e| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 配置/数据目录锚定到 exe 所在目录（便携应用惯例）：
    // 防止从不同工作目录启动时读到另一份 config.toml / 数据库（曾导致「双配置」分裂）。
    let exe_dir = pr_desk_rs::exe_dir();
    let cfg_path = exe_dir.join("config.toml");
    // 兼容迁移：exe 旁没有但旧式 cwd 里有配置时，搬过来再用
    let cwd_cfg = PathBuf::from("config.toml");
    if !cfg_path.exists() && cwd_cfg.exists() && cwd_cfg != cfg_path {
        let _ = std::fs::copy(&cwd_cfg, &cfg_path);
    }
    let cfg = Config::load(&cfg_path)?;
    let data_dir = exe_dir.join("data");
    std::fs::create_dir_all(&data_dir)?;
    // 启动档案名收口：active_profile 指向不存在/非法的档案时回退本地（内存修正，不写盘）
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

    let ui = MainWindow::new()?;
    // Cmd 走 tokio unbounded：worker 在 async 里 recv().await（std 阻塞 recv 会永久
    // 占住 tokio 一个工作线程）；发送方是同步的 UI 回调，unbounded 的 send 天然兼容。
    // Msg 保持 std mpsc：接收方是专用的转发线程（blocking recv 合理），发送是非阻塞的。
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();

    let app = Arc::new(Mutex::new(App {
        cfg: cfg.clone(),
        db,
        rows: Vec::new(),
        view: Vec::new(),
        view_keys: Vec::new(),
        view_visible: Vec::new(),
        collapsed: HashSet::new(),
        filter: String::new(),
        sort_col: "upd".into(),
        sort_asc: false,
        min_score: 0.0,
        selected: None,
        selection: Vec::new(),
        anchor: None,
        status_msg: config_status(&cfg),
        status_gen: 0,
        gh_refreshing: false,
        sync_inflight: false,
        inbox: Vec::new(),
        inbox_view: Vec::new(),
        inbox_filter: String::new(),
        inbox_all: true,
        inbox_page: 1,
        inbox_has_more: false,
        inbox_loading_more: false,
        inbox_detail: None,
        diff_data: Vec::new(),
        nd_diff_data: Vec::new(),
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
    ui.set_s_proxy(cfg.proxy.clone().into());
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
    // 启动时把配置的语言应用到 UI（Slint 侧 I18n 单例由 app.slint 的 changed language 回调处理）
    ui.set_language(cfg.language.clone().into());
    ui.set_font_scale(cfg.font_scale);
    // 自定义评审模板（exe 旁 review_prompt.md，空/缺失 = 默认模板）
    ui.set_s_review_prompt(
        std::fs::read_to_string(pr_desk_rs::exe_dir().join("review_prompt.md"))
            .unwrap_or_default()
            .into(),
    );
    // 数据源档案回填：名单（local 恒首位）+ 当前选中
    ui.set_profile_names(ModelRc::new(VecModel::from(
        cfg.profile_names().iter().map(|n| n.as_str().into()).collect::<Vec<SharedString>>(),
    )));
    ui.set_active_profile(cfg.active_profile.clone().into());

    // 字体按平台赋值（UI 侧只认 MainWindow.ui_font / Theme.mono_font，不写死字体名）：
    // Windows 雅黑 UI + Consolas；macOS PingFang SC + Menlo；
    // Linux 用 Noto CJK + Noto Sans Mono（没装就走 fontconfig 系统兜底，比 CJK 随机兜底可控）
    #[cfg(target_os = "windows")]
    let (ui_font, mono_font) = ("Microsoft YaHei UI", "Consolas");
    #[cfg(target_os = "macos")]
    let (ui_font, mono_font) = ("PingFang SC", "Menlo");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let (ui_font, mono_font) = ("Noto Sans CJK SC", "Noto Sans Mono");
    ui.set_ui_font(ui_font.into());
    ui.set_mono_font(mono_font.into());

    // 窗口/面板几何记忆：配置里有记录才应用（首启不覆盖默认值；小于最小值视为垃圾忽略）
    if let (Some(w), Some(h)) = (cfg.window_width, cfg.window_height) {
        if w >= 900.0 && h >= 480.0 {
            ui.window().set_size(slint::LogicalSize::new(w as f32, h as f32));
        }
    }
    if let Some(w) = cfg.panel_width {
        if w >= 320.0 {
            ui.set_panel_width(w as f32);
        }
    }
    // 退出时把当前窗口尺寸和面板宽度写回 config.toml（原子写在 Config::save 里）
    {
        let app = app.clone();
        let cfg_path = cfg_path.clone();
        let weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                let scale = ui.window().scale_factor() as f64;
                let size = ui.window().size();
                let (w, h) = (size.width as f64 / scale, size.height as f64 / scale);
                let pw = ui.get_panel_width() as f64;
                let mut a = app.lock_recover();
                // 小于最小值的尺寸不写（防御垃圾值；首启未配置时也不添键）
                if w >= 900.0 && h >= 480.0 {
                    a.cfg.window_width = Some(w);
                    a.cfg.window_height = Some(h);
                }
                if pw >= 320.0 {
                    a.cfg.panel_width = Some(pw);
                }
                if let Err(e) = a.cfg.save(&cfg_path) {
                    tracing::warn!("窗口尺寸写入 config.toml 失败: {e}");
                }
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    // 主题切换：即时持久化到 config.toml
    {
        let app = app.clone();
        let cfg_path = cfg_path.clone();
        ui.on_theme_toggled(move |dark| {
            let mut a = app.lock_recover();
            a.cfg.theme = if dark { "dark".into() } else { "light".into() };
            if let Err(e) = a.cfg.save(&cfg_path) {
                tracing::warn!("主题写入 config.toml 失败: {e}");
            }
        });
    }

    // 语言切换：持久化到 config.toml，并重建行模型（组标题/活动行语言跟随）
    {
        let app = app.clone();
        let cfg_path = cfg_path.clone();
        ui.on_language_changed(move |lang| {
            let lang = if lang.as_str() == "en" { "en" } else { "zh" }.to_string();
            {
                let mut a = app.lock_recover();
                if a.cfg.language == lang {
                    return; // 启动 set_language 的同值回调：不落盘不重建
                }
                a.cfg.language = lang;
                if let Err(e) = a.cfg.save(&cfg_path) {
                    tracing::warn!("语言写入 config.toml 失败: {e}");
                }
            }
            reload_rows_global(&app);
            send_msg(Msg::StatusBarRefresh);
        });
    }

    // 字号档位切换：即时生效（changed 回调已写 Theme.fs_scale），持久化到 config.toml
    {
        let app = app.clone();
        let cfg_path = cfg_path.clone();
        ui.on_font_scale_changed(move |s| {
            let snapped = if s < 0.95 { 0.9 } else if s > 1.07 { 1.15 } else { 1.0 };
            let mut a = app.lock_recover();
            if (a.cfg.font_scale - snapped).abs() < 0.001 {
                return; // 启动 set_font_scale 的同值回调：不落盘
            }
            a.cfg.font_scale = snapped;
            if let Err(e) = a.cfg.save(&cfg_path) {
                tracing::warn!("字号写入 config.toml 失败: {e}");
            }
        });
    }

    // ── 后台工作线程（tokio runtime） ──
    {
        let app = app.clone();
        let msg_tx = msg_tx.clone();
        let cmd_tx_worker = cmd_tx.clone();   // worker 内部自入队（如补齐元数据）用
        let cfg_path = cfg_path.clone();      // 切换数据源时持久化 active_profile 用
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(worker(app, cmd_rx, msg_tx, cmd_tx_worker, cfg_path));
        });
    }

    // 自动同步定时器（每轮重新读当前间隔，设置页改间隔下轮生效）
    {
        let cmd_tx = cmd_tx.clone();
        let app2 = app.clone();
        std::thread::spawn(move || loop {
            let interval = app2.lock_recover().cfg.poll_interval_min.max(1);
            std::thread::sleep(std::time::Duration::from_secs(interval * 60));
            // 在途去重：上一轮还没跑完就跳过本轮（不排队堆叠全量同步）
            {
                let mut a = app2.lock_recover();
                if a.sync_inflight {
                    continue;
                }
                a.sync_inflight = true;
            }
            if cmd_tx.send(Cmd::Sync).is_err() {
                break;
            }
        });
    }

    // ── 消息回 UI ──
    {
        let weak = ui.as_weak();
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        std::thread::spawn(move || loop {
            match msg_rx.recv() {
                Ok(msg) => {
                    let weak = weak.clone();
                    let app = app.clone();
                    let cmd_tx = cmd_tx.clone();
                    if let Err(e) = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            apply_msg(&ui, &app, &cmd_tx, msg);
                        }
                    }) {
                        // 事件循环已退出（或正在退出）：消息丢失至少要可查
                        tracing::warn!("消息回投 UI 事件循环失败，消息丢失: {e}");
                    }
                }
                Err(_) => break,
            }
        });
    }

    // ── UI 回调 ──
    {
        let cmd_tx = cmd_tx.clone();
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_refresh(move || {
            // 连点去重：一轮刷新在途时忽略后续点击（避免排队跑 N 轮全量拉取）
            {
                let mut a = app.lock_recover();
                if a.gh_refreshing {
                    return;
                }
                a.gh_refreshing = true;
            }
            if let Some(ui) = ui2.upgrade() {
                ui.set_gh_refreshing(true); // 顶栏转圈（进行中反馈）
            }
            let _ = cmd_tx.send(Cmd::RefreshGithub);
        });
    }
    {
        let cmd_tx = cmd_tx.clone();
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_sync_now(move || {
            // 连点去重：同 on_refresh
            {
                let mut a = app.lock_recover();
                if a.sync_inflight {
                    return;
                }
                a.sync_inflight = true;
            }
            if let Some(ui) = ui2.upgrade() {
                ui.set_syncing(true); // 顶栏转圈（进行中反馈）
            }
            let _ = cmd_tx.send(Cmd::Sync);
        });
    }
    {
        let cmd_tx = cmd_tx.clone();
        let ui2 = ui.as_weak();
        ui.on_fetch_inbox(move || {
            // 首次加载态：rows 回送前 UI 显示「加载中…」而非误导空态
            if let Some(ui) = ui2.upgrade() {
                ui.set_inbox_loading(true);
            }
            let _ = cmd_tx.send(Cmd::FetchInbox);
        });
    }
    {
        // 「加载更多」：按钮按下立刻给 UI 加载态（回调跑在 UI 线程），worker 拉下一页追加
        let ui2 = ui.as_weak();
        let cmd_tx = cmd_tx.clone();
        ui.on_inbox_load_more(move || {
            if let Some(ui) = ui2.upgrade() {
                ui.set_inbox_loading_more(true);
            }
            let _ = cmd_tx.send(Cmd::FetchInboxMore);
        });
    }
    {
        let cmd_tx = cmd_tx.clone();
        ui.on_inbox_mark_all_read(move || {
            let _ = cmd_tx.send(Cmd::MarkAllNotifsRead);
        });
    }
    {
        // 「只看未读」开关：状态存 App（worker 拉取时读取），切完立即重拉；分页重置回第一页
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let ui2 = ui.as_weak();
        ui.on_inbox_filter_toggled(move |unread_only| {
            let mut a = app.lock_recover();
            a.inbox_all = !unread_only;
            a.inbox_page = 1;
            a.inbox_has_more = false;
            drop(a);
            if let Some(ui) = ui2.upgrade() {
                ui.set_inbox_loading(true);
            }
            let _ = cmd_tx.send(Cmd::FetchInbox);
        });
    }
    {
        // 「+ 预评审」：按 UI 下标取 repo/number/title 交给 worker 入库（下标对过滤后的视图）
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_inbox_add_review(move |i| {
            let row = app.lock_recover().inbox_view.get(i as usize).cloned();
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
        // Inbox 仓库过滤：过滤在 Rust 侧做（模型预过滤后交给 ListView 虚拟化渲染）
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_inbox_repo_filter_changed(move |f| {
            let rows = {
                let mut a = app.lock_recover();
                a.inbox_filter = f.to_string();
                a.inbox_view = inbox_filtered(&a);
                build_inbox_rows(&a, &a.inbox_view)
            };
            if let Some(ui) = ui2.upgrade() {
                ui.set_inbox_rows(ModelRc::new(VecModel::from(rows)));
            }
        });
    }
    {
        // 列表头点击排序（Excel 式）：同列再点反向；新列按该列的默认方向
        //（数值/时间列先降序——最新/最大在前；文本列先升序）
        let app = app.clone();
        ui.on_header_clicked(move |col| {
            let mut a = app.lock_recover();
            if a.sort_col == col.as_str() {
                a.sort_asc = !a.sort_asc;
            } else {
                a.sort_col = col.to_string();
                a.sort_asc = matches!(col.as_str(), "num" | "title" | "author" | "turn" | "review" | "ci" | "size");
            }
            drop(a);
            send_msg(Msg::ViewPrefsChanged);
        });
    }
    {
        // 高分快筛开关（只看评分 ≥8.0 的 PR）
        let app = app.clone();
        ui.on_high_score_toggled(move |on| {
            app.lock_recover().min_score = if on { 8.0 } else { 0.0 };
            send_msg(Msg::ViewPrefsChanged);
        });
    }
    {
        // 组头折叠/展开：状态存 App.collapsed（跨刷新保留），翻转后直接重建行模型
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_group_toggled(move |key| {
            {
                let mut a = app.lock_recover();
                if !a.collapsed.remove(key.as_str()) {
                    a.collapsed.insert(key.to_string());
                }
            }
            if let Some(ui) = ui2.upgrade() {
                rebuild_rows_ui(&ui, &app);
            }
        });
    }
    {
        // j/k 键盘导航：在「可见行」展示序上移动（折叠组的行跳过），选中态按身份落地
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_key_move(move |delta| {
            let sel_key = {
                let mut a = app.lock_recover();
                if a.view_visible.is_empty() {
                    return;
                }
                let n = a.view_visible.len();
                let pos = a
                    .selected
                    .as_ref()
                    .and_then(|s| a.view_visible.iter().position(|k| k == s));
                let npos = match pos {
                    None => {
                        if delta > 0 { 0 } else { n - 1 }
                    }
                    Some(p) => (p as i64 + delta as i64).clamp(0, n as i64 - 1) as usize,
                };
                a.selected = Some(a.view_visible[npos].clone());
                a.selected.as_ref().map(|(r, num)| format!("{r}#{num}"))
            };
            if let (Some(ui), Some(k)) = (ui2.upgrade(), sel_key) {
                ui.set_selected_key(k.into());
            }
        });
    }
    {
        // Enter：打开当前选中 PR 的详情（身份以 App.selected 为准，与点击路径同一出口）
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_key_enter(move || {
            let sel = app.lock_recover().selected.clone();
            if let Some((repo, number)) = sel {
                let _ = cmd_tx.send(Cmd::OpenDrawer { repo, number, refetch: true });
            }
        });
    }
    {
        // Esc（详情关着时）：取消选中（UI 高亮与 App.selected 一起清）
        let app = app.clone();
        ui.on_clear_selection(move || {
            app.lock_recover().selected = None;
        });
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        // UI 传视图下标，先经 view 换算为该行的身份（repo, number）；
        // 选中态立即落地（后续 Panel/DiffParsed 的身份守卫以此为准），装配交给 worker
        ui.on_row_activated(move |i| {
            if let Some((repo, number)) = resolve_view(&app, i as usize) {
                app.lock_recover().selected = Some((repo.clone(), number));
                let _ = cmd_tx.send(Cmd::OpenDrawer { repo, number, refetch: true });
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
        // 详情面板 tab 切换 → 惰性拉取对应数据（会话/Checks/Files；各自 120s 节流）
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_panel_tab_changed(move |t| {
            let parts = match t {
                1 => PART_CONVO,
                2 => PART_CHECKS,
                3 => PART_FILES,
                _ => return, // 评审/Diff 用本地数据
            };
            let sel = app.lock_recover().selected.clone();
            if let Some((repo, number)) = sel {
                let _ = cmd_tx.send(Cmd::FetchPrLive { repo, number, parts });
            }
        });
    }
    {
        let app = app.clone();
        ui.on_row_reject(move |i| {
            let Some((repo, number)) = resolve_view(&app, i as usize) else { return };
            let cur = {
                let a = app.lock_recover();
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
            app.lock_recover().selected = None;
            send_msg(Msg::ClosePanel);
        });
    }
    {
        // 面板看板按钮：切换当前选中 PR 的 on_board
        let app = app.clone();
        ui.on_panel_board(move || {
            let sel = app.lock_recover().selected.clone();
            if let Some((repo, number)) = sel {
                let v = toggle_of(&app, &repo, number, "on_board");
                local_update(&app, &repo, number, "on_board", v);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_panel_set_priority(move |p| {
            let sel = app.lock_recover().selected.clone();
            if let Some((repo, number)) = sel {
                local_update(&app, &repo, number, "priority", p as i64);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_panel_toggle_pin(move || {
            let sel = app.lock_recover().selected.clone();
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
            let target = app.lock_recover().selected.clone();
            if let Some((repo, number)) = target {
                let _ = cmd_tx.send(Cmd::RunReview { repo, number });
            }
        });
    }
    {
        // 详情面板「复制评审」：get_review SQL + 剪贴板写入都挪进 worker
        //（Windows 剪贴板打开可能重试阻塞，不占 UI 线程）；结果经状态栏消息反馈
        let cmd_tx = cmd_tx.clone();
        ui.on_action_copy_review(move || {
            let _ = cmd_tx.send(Cmd::CopyReview);
        });
    }
    {
        // DiffView 折叠/展开：行内容在折叠时不在 UI 模型里，翻转 Rust 缓存后整体重建
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_diff_file_toggled(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let (rows, max_line) = {
                let mut a = app.lock_recover();
                if let Some(f) = a.diff_data.get_mut(i as usize) {
                    f.expanded = !f.expanded;
                }
                (diff_row_models(&a.diff_data), diff_max_line(&a.diff_data))
            };
            ui.set_diff_files(ModelRc::new(VecModel::from(rows)));
            ui.set_diff_max_text(max_line);
        });
    }
    {
        // Inbox 行点击 / Enter：选中 + 展开右侧预览面板（发 FetchNotifDetail），并标记已读
        //（下标对仓库过滤后的视图 inbox_view）
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_inbox_activated(move |i| {
            let row = app.lock_recover().inbox_view.get(i as usize).cloned();
            let Some(n) = row else { return };
            let repo = n["repo"].as_str().unwrap_or("").to_string();
            let kind = n["kind"].as_str().unwrap_or("").to_string();
            let title = n["title"].as_str().unwrap_or("").to_string();
            let url = n["url"].as_str().unwrap_or("").to_string();
            let state = n["state"].as_str().unwrap_or("").to_string();
            let Some(number) = notif_url_number(&url) else { return };
            app.lock_recover().inbox_detail = Some((repo.clone(), number, kind.clone(), url));
            // 立即开面板并进加载态（数据由 worker 回送）；
            // 查询失败是错误不是「不存在」：保守按已加入处理（防重复插入）并告警
            let added = kind == "PullRequest" && {
                let a = app.lock_recover();
                a.db.pr_exists(&repo, number).unwrap_or_else(|e| {
                    tracing::warn!("查询 PR 是否已加入失败 {repo} #{number}: {e}");
                    true
                })
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_nd_title(title.into());
                ui.set_nd_kind(kind.clone().into());
                ui.set_nd_kind_label(notif_kind_label(&lang_of(&app), &kind).into());
                ui.set_nd_state(state.into());
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
            let detail = app.lock_recover().inbox_detail.clone();
            if let Some((_, _, _, url)) = detail {
                if !url.is_empty() {
                    if let Err(e) = open::that(&url) {
                        let lang = lang_of(&app);
                        send_msg(Msg::Status(trf(&lang, "open_browser_failed", &[&e.to_string()])));
                    }
                }
            }
        });
    }
    {
        // 列表行内 hover 按钮：浏览器打开（下标 → 身份 → url）
        let app = app.clone();
        ui.on_row_open(move |i| {
            if let Some((repo, number)) = resolve_view(&app, i as usize) {
                open_pr_url(&app, &repo, &number);
            }
        });
    }
    {
        // 快捷键 o：浏览器打开；多选存在时批量打开（上限 5 个防误炸浏览器）
        let app = app.clone();
        ui.on_key_open(move || {
            batch_open_browser(&app);
        });
    }
    {
        // 预览面板关闭（× 按钮 / Esc）
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_inbox_close_detail(move || {
            app.lock_recover().inbox_detail = None;
            if let Some(ui) = weak.upgrade() {
                ui.set_inbox_detail_open(false);
            }
        });
    }
    {
        // 预览面板 Diff 标签页：同主面板（翻转 Rust 缓存后整体重建）
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_inbox_diff_toggled(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let (rows, max_line) = {
                let mut a = app.lock_recover();
                if let Some(f) = a.nd_diff_data.get_mut(i as usize) {
                    f.expanded = !f.expanded;
                }
                (diff_row_models(&a.nd_diff_data), diff_max_line(&a.nd_diff_data))
            };
            ui.set_nd_diff_files(ModelRc::new(VecModel::from(rows)));
            ui.set_nd_diff_max_text(max_line);
        });
    }
    {
        // 快捷键 p：多选存在时批量 toggle，否则单行
        let app = app.clone();
        ui.on_key_pin(move || {
            if !app.lock_recover().selection.is_empty() {
                batch_toggle(&app, "pinned");
            } else if let Some((repo, number)) = app.lock_recover().selected.clone() {
                let v = toggle_of(&app, &repo, number, "pinned");
                local_update(&app, &repo, number, "pinned", v);
            }
        });
    }
    {
        // 快捷键 1/2/3：多选存在时批量设优先级，否则单行
        let app = app.clone();
        ui.on_key_priority(move |p| {
            let keys = effective_targets(&app);
            if keys.len() > 1 {
                batch_local_update(&app, &keys, "priority", p as i64);
            } else if let Some((repo, number)) = keys.first().cloned() {
                local_update(&app, &repo, number, "priority", p as i64);
            }
        });
    }
    // ── 设置页 ──
    {
        let app = app.clone();
        let ui2 = ui.as_weak();
        let cmd_tx = cmd_tx.clone();
        ui.on_open_settings(move || {
            if let Some(ui) = ui2.upgrade() {
                // 每次打开回填当前配置（LLM 字段可能在别处被改动）
                let cfg = app.lock_recover().cfg.clone();
                // 首启兜底：default 档未配置且无自定义档案时，编辑目标停在 local 会把
                // 服务器三字段锁死、永远填不了服务器地址 → 自动切到 default 编辑
                if ui.get_settings_profile() == "local"
                    && !cfg.has_default_profile()
                    && cfg.profiles.is_empty()
                {
                    ui.set_settings_profile("default".into()); // changed → focus 回调回填（空值）
                }
                // 服务器三字段按「正在编辑的档案」回填（local 档为空且字段锁定）
                let (server, token, ak) = profile_fields(&cfg, &ui.get_settings_profile());
                ui.set_s_server(server.into());
                ui.set_s_sync_token(token.into());
                ui.set_s_ak(ak.into());
                ui.set_s_llm_base(cfg.llm_base_url.into());
                ui.set_s_llm_key(cfg.llm_api_key.unwrap_or_default().into());
                ui.set_s_llm_model(cfg.llm_model.into());
                ui.set_s_ocr_path(cfg.ocr_path.into());
                ui.set_s_review_concurrency(cfg.review_concurrency as i32);
                ui.set_s_proxy(cfg.proxy.into());
                // 弹窗内联错误行清空（上一次失败的残留不带进本次会话）
                ui.set_s_dialog_error("".into());
                // 自定义评审模板每次打开重读（可能被外部编辑过）：
                // 读盘挪进 worker（UI 线程不做阻塞 IO），回来经 Msg::ReviewPromptLoaded 回填。
                // 回填竞态防护：先清空 + loading——回填完成前编辑框锁定、保存不回写模板，
                // 防止用户瞬间点保存用旧文本覆盖磁盘新模板
                ui.set_s_review_prompt("".into());
                ui.set_s_prompt_loading(true);
                let _ = cmd_tx.send(Cmd::LoadReviewPrompt);
                ui.set_settings_open(true);
            }
        });
    }
    // ── 多选 / 批量操作 ──
    {
        // 行左键点击（带修饰键语义；行的 pointer-event 统一派发）：
        // 无修饰 = 清多选 + 单选 + 打开详情（与 row_activated 同出口）；
        // Ctrl = toggle 该行进/出多选（集合为空时以当前选中行为起点扩充；
        // 移动主选中，但不开详情——多选场景下面板频闪）；
        // Shift = 从锚点到本行（view_visible 序）范围入多选（不开详情）
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let ui2 = ui.as_weak();
        ui.on_row_clicked(move |i, ctrl, shift| {
            let Some((repo, number)) = resolve_view(&app, i as usize) else { return };
            let key = (repo.clone(), number);
            let open = !ctrl && !shift;
            {
                let mut a = app.lock_recover();
                if shift {
                    let anchor = a.anchor.clone().unwrap_or_else(|| key.clone());
                    let view = a.view_visible.clone();
                    selection::extend_range(&mut a.selection, &view, &anchor, &key);
                    if a.anchor.is_none() {
                        a.anchor = Some(key.clone());
                    }
                } else if ctrl {
                    // 标准多选手感（Explorer/Finder）：集合为空时当前选中行是多选起点，
                    // Ctrl 加选在其基础上扩充，而不是从空集合开始丢掉它
                    if a.selection.is_empty()
                        && let Some(cur) = a.selected.clone()
                        && cur != key
                    {
                        a.selection.push(cur);
                    }
                    selection::toggle(&mut a.selection, &key);
                    a.anchor = Some(key.clone());
                } else {
                    a.selection.clear();
                    a.anchor = Some(key.clone());
                }
                a.selected = Some(key);
            }
            if let Some(ui) = ui2.upgrade() {
                push_selection_ui(&ui, &app);
            }
            if open {
                let _ = cmd_tx.send(Cmd::OpenDrawer { repo, number, refetch: true });
            }
        });
    }
    {
        // 行右键：该行不在多选内则先单选它（多选内含它则不动多选），菜单由 UI 侧打开
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_row_context(move |i| {
            let Some((repo, number)) = resolve_view(&app, i as usize) else { return };
            let key = (repo.clone(), number);
            {
                let mut a = app.lock_recover();
                if !selection::is_selected(&a.selection, &key) {
                    selection::select_single(&mut a.selection, &key);
                    a.anchor = Some(key.clone());
                }
                a.selected = Some(key);
            }
            if let Some(ui) = ui2.upgrade() {
                // 右键转移主选中行：同步高亮
                let k = format!("{repo}#{number}");
                ui.set_selected_key(k.into());
                push_selection_ui(&ui, &app);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_batch_set_priority(move |p| {
            let keys = effective_targets(&app);
            if !keys.is_empty() {
                batch_local_update(&app, &keys, "priority", p as i64);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_batch_toggle_pin(move || batch_toggle(&app, "pinned"));
    }
    {
        let app = app.clone();
        ui.on_batch_toggle_board(move || batch_toggle(&app, "on_board"));
    }
    {
        // 批量状态："rejected"（跳过）/ "pending"（恢复）
        let app = app.clone();
        ui.on_batch_set_status(move |s| {
            let status = match s.as_str() {
                "rejected" | "pending" => s.to_string(),
                _ => return, // 白名单外的值不进库
            };
            let keys = effective_targets(&app);
            if !keys.is_empty() {
                batch_local_update(&app, &keys, "status", status);
            }
        });
    }
    {
        // 批量预评审：逐行发 Cmd::RunReview（review_slots 并发槽排队/去重由 worker 侧保证）
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_batch_run_review(move || {
            let keys = effective_targets(&app);
            if keys.is_empty() {
                return;
            }
            let n = keys.len();
            for (repo, number) in keys {
                let _ = cmd_tx.send(Cmd::RunReview { repo, number });
            }
            let lang = lang_of(&app);
            send_msg(Msg::Status(trf(&lang, "review_queued", &[&n.to_string()])));
        });
    }
    {
        // 批量复制链接：换行拼接 URL，剪贴板写入挪进 worker（Windows 剪贴板打开可能阻塞）
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        ui.on_batch_copy_links(move || {
            let keys = effective_targets(&app);
            if keys.is_empty() {
                return;
            }
            let text = {
                let a = app.lock_recover();
                keys.iter()
                    .map(|(repo, number)| {
                        find_row(&a, repo, *number)
                            .and_then(|i| a.rows[i]["url"].as_str().map(|s| s.to_string()))
                            .filter(|u| !u.is_empty())
                            .unwrap_or_else(|| format!("https://github.com/{repo}/pull/{number}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let _ = cmd_tx.send(Cmd::CopyLinks(text));
        });
    }
    {
        let app = app.clone();
        ui.on_batch_open_browser(move || batch_open_browser(&app));
    }
    {
        // 清空多选（Esc 第一级 / 操作条 ✕）；主选中行保留。
        // 高亮由 push_selection_ui 原地翻转（不做全量重建——重建会重置滚动位置）
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_clear_multi_selection(move || {
            {
                let mut a = app.lock_recover();
                a.selection.clear();
                a.anchor = None;
            }
            if let Some(ui) = ui2.upgrade() {
                push_selection_ui(&ui, &app);
            }
        });
    }
    {
        let app = app.clone();
        let ui2 = ui.as_weak();
        ui.on_settings_profile_focus(move |name| {
            let (server, token, ak) = {
                let a = app.lock_recover();
                profile_fields(&a.cfg, &name)
            };
            if let Some(ui) = ui2.upgrade() {
                ui.set_s_server(server.into());
                ui.set_s_sync_token(token.into());
                ui.set_s_ak(ak.into());
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
        ui.on_save_settings(move |server, sync_tok, gh_tok, repos_text, poll_s, llm_base, llm_key, llm_model, ocr_path, review_conc_s, ak, proxy| {
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
            let proxy = proxy.trim().to_string();
            // 代理地址保存前校验：非法直接拒绝保存（弹窗内联错误 + 状态栏各发一份）
            if let Err(e) = pr_desk_rs::http_client(&proxy, std::time::Duration::from_secs(30)) {
                let lang = lang_of(&app);
                let msg = trf(&lang, "proxy_invalid", &[&e.to_string()]);
                if let Some(ui) = ui2.upgrade() {
                    ui.set_s_dialog_error(msg.clone().into());
                }
                send_msg(Msg::Status(msg));
                return;
            }
            // 服务器三字段按「正在编辑的档案」写回（local 档字段锁定，不读表单值）
            let edit_profile = ui2
                .upgrade()
                .map(|u| u.get_settings_profile().to_string())
                .unwrap_or_else(|| "default".into());
            #[allow(clippy::type_complexity)]
            let (theme, language, mut profiles, active_profile, mut server_url, mut sync_token, mut ak_s,
                 font_scale, window_width, window_height, panel_width) = {
                let a = app.lock_recover();
                // 主题/语言/数据源档案/活跃档不在设置页表单里，保留当前值。
                // 字号与窗口/面板几何同样是保留项——必须在同一个锁块里一次取完：
                // 曾把它们放在下面 Config 字面量里逐字段 lock_recover()，临时 guard
                // 活到整条 let 语句结束，同线程第二次 lock 即自死锁（一保存设置就卡死）
                (
                    a.cfg.theme.clone(),
                    a.cfg.language.clone(),
                    a.cfg.profiles.clone(),
                    a.cfg.active_profile.clone(),
                    a.cfg.server_url.clone(),
                    a.cfg.sync_token.clone(),
                    a.cfg.astrbot_api_key.clone(),
                    a.cfg.font_scale,
                    a.cfg.window_width,
                    a.cfg.window_height,
                    a.cfg.panel_width,
                )
            };
            match edit_profile.as_str() {
                "local" => {} // 本地档无服务器字段，顶层键（= default 档）保持不动
                "default" => {
                    server_url = server.trim().to_string();
                    sync_token = sync_tok.trim().to_string();
                    ak_s = ak.trim().to_string();
                }
                name => {
                    let vals = (server.trim().to_string(), sync_tok.trim().to_string(), ak.trim().to_string());
                    match profiles.iter_mut().find(|p| p.name == name) {
                        Some(p) => {
                            p.server_url = vals.0;
                            p.sync_token = vals.1;
                            p.api_key = vals.2;
                        }
                        // 档案在编辑期间被别处删了：按当前表单值重建，不丢用户输入
                        None => profiles.push(Profile {
                            name: name.into(),
                            server_url: vals.0,
                            sync_token: vals.1,
                            api_key: vals.2,
                        }),
                    }
                }
            }
            let new_cfg = Config {
                server_url,
                sync_token,
                astrbot_api_key: ak_s,
                github_token: Some(gh_tok.trim().to_string()).filter(|t| !t.is_empty()),
                proxy,
                repos,
                poll_interval_min: poll,
                theme,
                language,
                // 字号档位是即时生效项（不走设置表单），保存设置时保留现值
                font_scale,
                llm_base_url: llm_base.trim().to_string(),
                llm_api_key: Some(llm_key.trim().to_string()).filter(|t| !t.is_empty()),
                llm_model: llm_model.trim().to_string(),
                ocr_path: if ocr_path.is_empty() { "ocr".into() } else { ocr_path.to_string() },
                review_concurrency,
                profiles,
                active_profile,
                // 窗口/面板几何记忆不在设置表单里，保留现值
                window_width,
                window_height,
                panel_width,
            };
            if let Err(e) = new_cfg.save(&cfg_path) {
                let lang = lang_of(&app);
                let msg = trf(&lang, "cfg_save_failed", &[&e.to_string()]);
                // 弹窗内联错误（同步回填：Slint 侧保存成功后才会关弹窗，靠它决定不关闭）；
                // 状态栏仍发一份（弹窗若被 Esc 关掉后还能看到）
                if let Some(ui) = ui2.upgrade() {
                    ui.set_s_dialog_error(msg.clone().into());
                }
                send_msg(Msg::Status(msg));
                return;
            }
            {
                let mut a = app.lock_recover();
                a.cfg = new_cfg.clone();
                a.status_msg = config_status(&new_cfg);
                // 配置状态是常驻消息：推序号让在途的自动清除定时器失效
                a.status_gen = a.status_gen.wrapping_add(1);
                // 并发上限即时生效（不影响已占用的槽位）
                a.review_slots.lock_recover().set_capacity(review_concurrency as usize);
            }
            if let Some(ui) = ui2.upgrade() {
                // 自定义评审模板落盘（exe 旁 review_prompt.md；清空 = 删除文件回到默认模板）。
                // 回填竞态：异步回填未完成时跳过（此时表单里是空占位，写盘会覆盖磁盘新模板）
                if !ui.get_s_prompt_loading() {
                    let prompt = ui.get_s_review_prompt().to_string();
                    let prompt_path = pr_desk_rs::exe_dir().join("review_prompt.md");
                    if prompt.trim().is_empty() {
                        let _ = std::fs::remove_file(&prompt_path);
                    } else if let Err(e) = std::fs::write(&prompt_path, prompt) {
                        tracing::warn!("写入 review_prompt.md 失败: {e}");
                    }
                }
                ui.set_onboarding(new_cfg.is_unconfigured());
                ui.set_inbox_needs_token(new_cfg.token().is_none());
                // 设置页改的是顶层键（= "default" 档）：token 从空变非空时名单要多出 default
                ui.set_profile_names(ModelRc::new(VecModel::from(
                    new_cfg.profile_names().iter().map(|n| n.as_str().into()).collect::<Vec<SharedString>>(),
                )));
                update_status_bar(&ui, &app);
            }
            send_msg(Msg::Status(tr(&lang_of(&app), "cfg_saved").into()));
            let _ = cmd_tx.send(Cmd::ReloadConfig); // 热生效，不用重启
        });
    }

    // ── 数据源档案（多数据源切换）──
    {
        // 切换：worker 校验档案存在 → 换库 + 重建同步客户端 + 刷新（结果经 Msg::Profiles 回填）
        let cmd_tx = cmd_tx.clone();
        ui.on_switch_profile(move |name| {
            let _ = cmd_tx.send(Cmd::SwitchProfile(name.to_string()));
        });
    }
    {
        // 新增自定义档案：校验名字（非空/字符合法/非保留/不重名）→ 复制 default 档配置入库 + save + 回填名单
        let app = app.clone();
        let cfg_path = cfg_path.clone();
        let weak = ui.as_weak();
        ui.on_profile_add(move |name| {
            let name = name.trim().to_string();
            let lang = lang_of(&app);
            // 弹窗内发起的操作：失败除了状态栏还要写弹窗内联错误行（状态栏被遮罩盖住）
            let dialog_err = |msg: String| {
                send_msg(Msg::Status(msg.clone()));
                if let Some(ui) = weak.upgrade() {
                    ui.set_s_dialog_error(msg.into());
                }
            };
            // Slint 侧在回调返回后才写 settings_profile = 新名；回调内拿不到之后的值，
            // 这里的 prev 仍是旧值，失败时经 Msg::SettingsProfile 延迟纠正回去
            let prev = weak.upgrade().map(|u| u.get_settings_profile().to_string());
            let names = {
                let mut a = app.lock_recover();
                if !pr_desk_rs::config::valid_profile_name(&name) {
                    dialog_err(tr(&lang, "profile_invalid_name").into());
                    if let Some(p) = prev { send_msg(Msg::SettingsProfile(p)); }
                    return;
                }
                if pr_desk_rs::config::is_reserved_profile_name(&name) {
                    dialog_err(trf(&lang, "profile_reserved", &[&name]));
                    if let Some(p) = prev { send_msg(Msg::SettingsProfile(p)); }
                    return;
                }
                if a.cfg.profiles.iter().any(|p| p.name == name) {
                    dialog_err(trf(&lang, "profile_exists", &[&name]));
                    if let Some(p) = prev { send_msg(Msg::SettingsProfile(p)); }
                    return;
                }
                // 以 default 档（顶层键）服务器配置为起点：用户通常只需改主机名
                let (d_url, d_tok, d_key) = (
                    a.cfg.server_url.clone(),
                    a.cfg.sync_token.clone(),
                    a.cfg.astrbot_api_key.clone(),
                );
                a.cfg.profiles.push(Profile {
                    name: name.clone(),
                    server_url: d_url,
                    sync_token: d_tok,
                    api_key: d_key,
                });
                if let Err(e) = a.cfg.save(&cfg_path) {
                    a.cfg.profiles.retain(|p| p.name != name);
                    dialog_err(trf(&lang, "cfg_save_failed", &[&e.to_string()]));
                    if let Some(p) = prev { send_msg(Msg::SettingsProfile(p)); }
                    return;
                }
                a.cfg.profile_names()
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_profile_names(ModelRc::new(VecModel::from(
                    names.iter().map(|n| n.as_str().into()).collect::<Vec<SharedString>>(),
                )));
                ui.set_s_dialog_error("".into()); // 成功：清掉之前的内联错误
            }
            send_msg(Msg::Status(trf(&lang, "profile_added", &[&name])));
        });
    }
    {
        // 删除自定义档案：只允许删非保留且非当前 active 的（db 文件保留，不删数据）
        let app = app.clone();
        let cfg_path = cfg_path.clone();
        let weak = ui.as_weak();
        ui.on_profile_remove(move |name| {
            let name = name.trim().to_string();
            let lang = lang_of(&app);
            // 弹窗内发起的操作：失败除了状态栏还要写弹窗内联错误行（状态栏被遮罩盖住）
            let dialog_err = |msg: String| {
                send_msg(Msg::Status(msg.clone()));
                if let Some(ui) = weak.upgrade() {
                    ui.set_s_dialog_error(msg.into());
                }
            };
            let names = {
                let mut a = app.lock_recover();
                let removable = !pr_desk_rs::config::is_reserved_profile_name(&name)
                    && a.cfg.profiles.iter().any(|p| p.name == name)
                    && a.cfg.active_profile != name;
                if !removable {
                    dialog_err(tr(&lang, "profile_remove_denied").into());
                    return;
                }
                let backup = a.cfg.profiles.clone();
                a.cfg.profiles.retain(|p| p.name != name);
                if let Err(e) = a.cfg.save(&cfg_path) {
                    a.cfg.profiles = backup;
                    dialog_err(trf(&lang, "cfg_save_failed", &[&e.to_string()]));
                    return;
                }
                a.cfg.profile_names()
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_profile_names(ModelRc::new(VecModel::from(
                    names.iter().map(|n| n.as_str().into()).collect::<Vec<SharedString>>(),
                )));
                // 被删的正是设置页在编辑的档案：编辑目标回到 local（changed 触发三字段回填）
                ui.set_settings_profile("local".into());
                ui.set_s_dialog_error("".into()); // 成功：清掉之前的内联错误
            }
            send_msg(Msg::Status(trf(&lang, "profile_removed", &[&name])));
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
    match MSG_TX.get() {
        Some(tx) => {
            if tx.send(msg).is_err() {
                tracing::warn!("消息通道已关闭（事件循环退出中），消息丢失");
            }
        }
        // 启动竞态窗口（worker 尚未注册 MSG_TX）：消息丢失至少要可查
        None => tracing::warn!("MSG_TX 未注册，消息丢失（启动竞态窗口）"),
    }
}

fn toggle_of(app: &Arc<Mutex<App>>, repo: &str, number: i64, field: &str) -> i64 {
    let a = app.lock_recover();
    find_row(&a, repo, number)
        .and_then(|idx| a.rows[idx][field].as_i64())
        .map(|v| if v == 0 { 1 } else { 0 })
        .unwrap_or(1)
}

/// 本地字段更新（pinned/on_board/priority/status）：按身份直改 DB，
/// 不依赖行是否还在当前列表里（面板开着但行被刷新滤掉时操作仍生效）。
fn local_update<V: Into<serde_json::Value>>(app: &Arc<Mutex<App>>, repo: &str, number: i64, field: &str, value: V) {
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

/// 批量操作的目标集合：多选非空用多选，否则回退主选中行（单选语义）。
fn effective_targets(app: &Arc<Mutex<App>>) -> Vec<(String, i64)> {
    let a = app.lock_recover();
    if !a.selection.is_empty() {
        a.selection.clone()
    } else {
        a.selected.clone().into_iter().collect()
    }
}

/// 多选状态摘要（喂给 UI 的右键菜单/操作条）：数量 + 三个「全部已置位」标志。
/// 标志从当前行数据现读；行已被滤掉的身份按「未置位」计（偏向显示 Pin/Add/Skip）。
fn selection_summary(app: &Arc<Mutex<App>>) -> (usize, bool, bool, bool) {
    let a = app.lock_recover();
    let n = a.selection.len();
    if n == 0 {
        return (0, false, false, false);
    }
    let field_all = |field: &str| {
        a.selection.iter().all(|(r, num)| {
            find_row(&a, r, *num)
                .and_then(|i| a.rows[i][field].as_i64())
                .unwrap_or(0)
                != 0
        })
    };
    let all_rejected = a.selection.iter().all(|(r, num)| {
        find_row(&a, r, *num)
            .and_then(|i| a.rows[i]["status"].as_str())
            == Some("rejected")
    });
    (n, field_all("pinned"), field_all("on_board"), all_rejected)
}

/// 把多选状态回写 UI（计数 + 菜单动态项标志）。selection 每次变动后都要调。
/// 同时原地翻转行模型的 multi_selected：全量重建（rebuild_rows_ui）会重置滚动位置，
/// 点击响应必须即时——逐行比对 row.key，只写回有变化的行（O(行数)，组头跳过）。
fn push_selection_ui(ui: &MainWindow, app: &Arc<Mutex<App>>) {
    let (n, all_pinned, all_on_board, all_rejected) = selection_summary(app);
    ui.set_sel_count(n as i32);
    ui.set_sel_all_pinned(all_pinned);
    ui.set_sel_all_on_board(all_on_board);
    ui.set_sel_all_rejected(all_rejected);
    let keys: HashSet<String> = {
        let a = app.lock_recover();
        a.selection.iter().map(|(r, num)| format!("{r}#{num}")).collect()
    };
    // ModelRc 的 Model 实现直接委托给内部 VecModel，无需 downcast
    let model = ui.get_items();
    for i in 0..model.row_count() {
        let Some(mut it) = model.row_data(i) else { continue };
        if it.is_header {
            continue;
        }
        let want = keys.contains(it.row.key.as_str());
        if it.multi_selected != want {
            it.multi_selected = want;
            model.set_row_data(i, it);
        }
    }
}

/// 批量字段更新：循环走 local_pr_update（字段白名单 + UPDATE/outbox 同事务不变），
/// 全部写完后只重建一次行模型、只发一条状态栏汇总（逐条调 local_update 会
/// 每条都重建 + 刷一条消息，N 大时闪屏且消息互相覆盖）。
fn batch_local_update<V: Into<serde_json::Value> + Clone>(
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
fn batch_toggle(app: &Arc<Mutex<App>>, field: &str) {
    let keys = effective_targets(app);
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
        selection::batch_toggle_target(vals.iter())
    };
    batch_local_update(app, &keys, field, target);
}

/// 批量「浏览器打开」：上限 5 个（防快捷键误触一次弹几十个标签页），超出部分状态栏提示。
fn batch_open_browser(app: &Arc<Mutex<App>>) {
    const MAX_OPEN: usize = 5;
    let keys = effective_targets(app);
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

fn reload_rows_global(app: &Arc<Mutex<App>>) {
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
    app.lock_recover().rows = rows.clone();
    // serde_json 行可跨线程；Slint 模型在 UI 线程（apply_msg）装配
    send_msg(Msg::Rows(rows));
}

// ── 详情抽屉 ──

/// 打开详情抽屉（worker 侧，Cmd::OpenDrawer 的处理体）：
/// 全部 DB 读写 + diff 解析在这里完成，UI 线程只负责把回送的 Msg 装盒。
/// 选中态（a.selected）由调用方先行设置（UI 回调点行时立即落地，
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

    // tab0 评审文本：评分 + 分项 + 摘要 + flagged
    let mut body = String::new();
    if let Some(rv) = &review {
        if let Some(s) = rv["score"].as_f64() {
            body.push_str(&trf(&lang, "body_score", &[&format!("{s:.1}")]));
            body.push('\n');
        } else {
            body.push_str(tr(&lang, "body_score_failed"));
            body.push('\n');
        }
        if let Some(dims) = rv["dimensions"].as_str() {
            if let Ok(d) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(dims) {
                let parts: Vec<String> = d.iter().map(|(k, v)| format!("{k} {}", v)).collect();
                body.push_str(&trf(&lang, "body_dimensions", &[&parts.join(" | ")]));
                body.push('\n');
            }
        }
        if let Some(sz) = rv["size_label"].as_str() {
            body.push_str(&trf(&lang, "body_size", &[sz]));
            body.push('\n');
        }
        if let Some(s) = rv["summary"].as_str() {
            body.push_str(&format!("\n{s}"));
        }
        if let Some(fl) = rv["flagged_files"].as_str() {
            if let Ok(files) = serde_json::from_str::<Vec<serde_json::Value>>(fl) {
                if !files.is_empty() {
                    body.push_str(&format!("\n\n{}", tr(&lang, "body_flagged")));
                    for f in files {
                        body.push_str(&trf(
                            &lang,
                            "flagged_item_fmt",
                            &[
                                f["path"].as_str().unwrap_or("?"),
                                f["issue"].as_str().unwrap_or(""),
                            ],
                        ));
                    }
                }
            }
        }
    }

    // 会话条目在 worker 完成 markdown 解析（纯数据过界）；Checks/Files 保持 JSON，UI 线程装盒
    let (convo_json, checks_json) = row
        .as_ref()
        .map(|r| drawer_extras_json(r))
        .unwrap_or_default();
    let convo = plain_convo_entries(
        &serde_json::from_str::<Vec<serde_json::Value>>(&convo_json).unwrap_or_default(),
    );
    let files_json = match &diff_cached {
        Some(d) => {
            let j = files_json_from_diff(d);
            // diff 缓存的增删行数顺手落库（列表 +/- 列的兜底数据源，GitHub files API 的
            // 真值拉到时会覆盖，见 FetchPrLive 的文件分支）
            if let Ok(files) = serde_json::from_str::<Vec<serde_json::Value>>(&j) {
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
    let age = row.as_ref().map(|r| model::age_text(r)).unwrap_or_default();
    let pinned = row.as_ref().map(|r| r["pinned"].as_i64() == Some(1)).unwrap_or(false);
    let on_board = row.as_ref().map(|r| r["on_board"].as_i64() == Some(1)).unwrap_or(false);

    let _ = msg_tx.send(Msg::Panel {
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
        convo,
        checks_json,
        files_json,
        review: review_props(review.as_ref(), &lang),
    });

    // 评审进行中打开该 PR 面板：紧随 Panel 之后把面板置 running
    //（apply_msg 顺序处理，ReviewProgress 在 Panel 重置状态后生效）
    let running = app.lock_recover().review_slots.lock_recover().is_running(&repo, number);
    if running {
        let _ = msg_tx.send(Msg::ReviewProgress { repo: repo.clone(), number, text: tr(&lang, "review_running").into() });
    }

    // diff 解析在 worker 完成（大 PR 几万行逐行解析不占 UI 线程），
    // DiffParsed 带身份：面板已切走时 apply 丢弃上屏（缓存可用）
    match diff_cached {
        Some(d) => {
            let files = parse_diff(&d);
            let _ = msg_tx.send(Msg::DiffParsed { repo: repo.clone(), number, files });
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

/// 评审结论推导：取 time（submitted_at）最新一条 approved/changes_requested 为准；
/// 只有 commented → "commented"；无评审 → None。dismissed 不计。
fn derive_review_decision(reviews: &[serde_json::Value]) -> Option<&'static str> {
    let mut latest: Option<(&str, &'static str)> = None; // (time, decision)
    for r in reviews {
        let decision = match r["state"].as_str().unwrap_or("") {
            "approved" => "approved",
            "changes_requested" => "changes_requested",
            _ => continue,
        };
        let time = r["time"].as_str().unwrap_or("");
        if latest.is_none_or(|(t, _)| time >= t) {
            latest = Some((time, decision));
        }
    }
    if let Some((_, d)) = latest {
        return Some(d);
    }
    if reviews.iter().any(|r| r["state"].as_str() == Some("commented")) {
        Some("commented")
    } else {
        None
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

/// diff 文本 → 结构化数据（前 3 个文件展开）。
/// 在 worker 调用：大 PR 几万行逐行解析是重活，UI 线程只做 VecModel 装盒。
fn parse_diff(diff: &str) -> Vec<DiffFileData> {
    diffview::split_files(diff)
        .into_iter()
        .enumerate()
        .map(|(i, (path, text))| DiffFileData {
            path,
            lines: diffview::numbered_lines(&text),
            expanded: i < 3,
        })
        .collect()
}

/// 最长内容行（按显示宽度：ASCII 计 1、其余计 2），供 UI 侧测量其像素宽，
/// 以决定 diff 横向滚动区域（Slint 无法对虚拟化/未实例化的行求最大文本宽）。
fn diff_max_line(files: &[DiffFileData]) -> slint::SharedString {
    files
        .iter()
        .flat_map(|f| f.lines.iter().map(|(_, t, _)| t.as_str()))
        .max_by_key(|t| t.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum::<usize>())
        .unwrap_or("")
        .into()
}

/// 结构化 diff → 摊平的 Slint DiffRow 模型（仅装盒，O(n)）。
/// 每个文件一个 header 行；展开的文件跟内容行（超 DIFF_MAX_RENDER_LINES 出截断提示行）。
/// 折叠的文件只有 header 行——展开重建依赖 Rust 侧缓存（App::diff_data / nd_diff_data）。
/// 增删行数在这里统计（替代旧 UI 侧的隐藏计数器 trick）。
fn diff_row_models(files: &[DiffFileData]) -> Vec<DiffRow> {
    let mut out = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        let adds = f.lines.iter().filter(|(k, _, _)| *k == diffview::LineKind::Add).count() as i32;
        let dels = f.lines.iter().filter(|(k, _, _)| *k == diffview::LineKind::Del).count() as i32;
        out.push(DiffRow {
            is_header: true,
            file_idx: fi as i32,
            path: f.path.as_str().into(),
            adds,
            dels,
            expanded: f.expanded,
            ..Default::default()
        });
        if f.expanded {
            for (kind, text, lineno) in f.lines.iter().take(DIFF_MAX_RENDER_LINES) {
                out.push(DiffRow {
                    is_header: false,
                    file_idx: fi as i32,
                    kind: match kind {
                        diffview::LineKind::Add => "add",
                        diffview::LineKind::Del => "del",
                        diffview::LineKind::Hunk => "hunk",
                        diffview::LineKind::Meta => "meta",
                        diffview::LineKind::Ctx => "ctx",
                    }
                    .into(),
                    text: text.as_str().into(),
                    lineno: *lineno,
                    ..Default::default()
                });
            }
            if f.lines.len() > DIFF_MAX_RENDER_LINES {
                out.push(DiffRow {
                    is_header: false,
                    is_trunc: true,
                    file_idx: fi as i32,
                    shown: DIFF_MAX_RENDER_LINES as i32,
                    total: f.lines.len() as i32,
                    ..Default::default()
                });
            }
        }
    }
    out
}

// ── 后台 worker ──

/// 会话 JSON 行 → 纯数据条目（markdown 在这里解析；worker 侧调用，产物可跨线程）。
/// kind 缺失（通知预览的评论条目无 kind 字段）时按 "comment" 处理。
fn plain_convo_entries(entries: &[serde_json::Value]) -> Vec<PlainConvoEntry> {
    entries
        .iter()
        .map(|e| {
            let text = e["text"].as_str().unwrap_or("").to_string();
            PlainConvoEntry {
                kind: e["kind"].as_str().unwrap_or("comment").to_string(),
                author: e["author"].as_str().unwrap_or("").to_string(),
                time: model::rel_time(e["time"].as_str().unwrap_or("")),
                blocks: pr_desk_rs::markdown::parse_markdown(&text),
                text,
            }
        })
        .collect()
}

/// PlainConvoEntry → Slint ConvoEntry 模型（纯装盒，UI 线程；Msg::Panel / PanelLive / NotifDetail 共用）。
fn convo_models(entries: Vec<PlainConvoEntry>) -> Vec<ConvoEntry> {
    entries
        .into_iter()
        .map(|e| ConvoEntry {
            kind: e.kind.into(),
            author: e.author.into(),
            time: e.time.into(),
            text: e.text.into(),
            blocks: md_blocks_of(e.blocks),
        })
        .collect()
}

/// Checks JSON → CheckRun 模型（Msg::Panel / PanelLive 共用装配）。
fn check_run_models(json: &str) -> Vec<CheckRun> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .map(|c| CheckRun {
            name: c["name"].as_str().unwrap_or("").into(),
            conclusion: c["conclusion"].as_str().unwrap_or("").into(),
        })
        .collect()
}

/// Files JSON → FileChange 模型（Msg::Panel / PanelLive / NotifDetail 共用装配）。
fn file_change_models(json: &str) -> Vec<FileChange> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .map(|f| FileChange {
            path: f["path"].as_str().unwrap_or("").into(),
            adds: f["adds"].as_i64().unwrap_or(0) as i32,
            dels: f["dels"].as_i64().unwrap_or(0) as i32,
        })
        .collect()
}

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

/// 批量补 PR/Issue 的 open/closed/merged 状态（GraphQL 一次请求）。
/// 失败降级为空表——状态只是行装饰，不拖垮通知拉取本身。
async fn fetch_notif_states(
    gh: &github::GitHub,
    ns: &[github::Notification],
) -> std::collections::HashMap<String, String> {
    let mut seen = std::collections::HashSet::new();
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
fn notif_row_json(n: &github::Notification, states: &std::collections::HashMap<String, String>) -> serde_json::Value {
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

/// GitHub 通知 subject.type（PullRequest/CheckSuite/…）→ 界面文案；未知值回退原串。
/// 注意：行的逻辑判断（是否 PR、徽章配色）仍用原始 kind 字段，本函数只产出显示文本。
fn notif_kind_label(lang: &str, kind: &str) -> String {
    let key = match kind {
        "PullRequest" => "kind_pull_request",
        "Issue" => "kind_issue",
        "Release" => "kind_release",
        "Discussion" => "kind_discussion",
        "CheckSuite" => "kind_check_suite",
        "Commit" => "kind_commit",
        _ => return kind.to_string(),
    };
    tr(lang, key).to_string()
}

/// GitHub 通知 reason（review_requested/mention/…）→ 界面文案；未知值回退原串。
fn notif_reason_label(lang: &str, reason: &str) -> String {
    let key = match reason {
        "review_requested" => "reason_review_requested",
        "assign" => "reason_assign",
        "mention" => "reason_mention",
        "comment" => "reason_comment",
        "ci_activity" => "reason_ci_activity",
        "author" => "reason_author",
        "subscribed" => "reason_subscribed",
        "team_mention" => "reason_team_mention",
        "security_alert" => "reason_security_alert",
        "state_change" => "reason_state_change",
        "invitation" => "reason_invitation",
        "manual" => "reason_manual",
        _ => return reason.to_string(),
    };
    tr(lang, key).to_string()
}

/// 当前仓库过滤词下的通知行（"" = 全部）。
fn inbox_filtered(a: &App) -> Vec<serde_json::Value> {
    a.inbox
        .iter()
        .filter(|n| a.inbox_filter.is_empty() || n["repo"].as_str() == Some(a.inbox_filter.as_str()))
        .cloned()
        .collect()
}

/// Inbox JSON 行 → NotifRow 模型：updated_at 换算相对时间、kind/reason 本地化、
/// PullRequest 行的「已加入」标记批量一次查完（替代逐行 pr_exists 的 N+1）；
/// 查询失败是错误不是「不存在」：保守按全部已加入处理（防重复插入）。
fn build_inbox_rows(a: &App, jsons: &[serde_json::Value]) -> Vec<NotifRow> {
    let lang = a.cfg.language.as_str();
    let keys: Vec<(String, i64)> = jsons
        .iter()
        .filter(|n| n["kind"].as_str() == Some("PullRequest"))
        .filter_map(|n| {
            notif_url_number(n["url"].as_str().unwrap_or(""))
                .map(|num| (n["repo"].as_str().unwrap_or("").to_string(), num))
        })
        .collect();
    let existing = match a.db.existing_pr_keys(&keys) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("批量查询 PR 是否已加入失败: {e}");
            None
        }
    };
    jsons
        .iter()
        .map(|n| {
            let url = n["url"].as_str().unwrap_or("");
            let number = notif_url_number(url);
            let kind = n["kind"].as_str().unwrap_or("");
            let added = kind == "PullRequest"
                && number.is_some()
                && match &existing {
                    Some(s) => s.contains(&(
                        n["repo"].as_str().unwrap_or("").to_string(),
                        number.unwrap_or(0),
                    )),
                    None => true,
                };
            NotifRow {
                id: n["id"].as_str().unwrap_or("").into(),
                repo: n["repo"].as_str().unwrap_or("").into(),
                title: n["title"].as_str().unwrap_or("").into(),
                kind: kind.into(),
                kind_label: notif_kind_label(lang, kind).into(),
                reason: notif_reason_label(lang, n["reason"].as_str().unwrap_or("")).into(),
                updated: model::rel_time(n["updated_at"].as_str().unwrap_or("")).into(),
                unread: n["unread"].as_bool().unwrap_or(false),
                url: url.into(),
                added,
                state: n["state"].as_str().unwrap_or("").into(),
            }
        })
        .collect()
}

/// 拉取 GitHub 通知并回送 UI（无 token 时静默跳过：通知接口必须鉴权）。
/// 刷新语义：重拉已加载的全部页（1..=inbox_page），保序拼接 + 按 thread id 去重，
/// 这样用户「加载更多」后定时刷新不会把多页数据塌回第一页。
async fn fetch_inbox(gh: &github::GitHub, app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let (has_token, all, pages) = {
        let a = app.lock_recover();
        (a.cfg.token().is_some(), a.inbox_all, a.inbox_page.max(1))
    };
    tracing::info!("fetch_inbox: has_token={} all={} pages={}", has_token, all, pages);
    if !has_token {
        // 无 token：关掉可能存在的 UI 加载态（首次进 Inbox 时置过 true）
        let _ = msg_tx.send(Msg::InboxLoading(false));
        return;
    }
    let mut all_ns: Vec<github::Notification> = Vec::new();
    let mut seen = std::collections::HashSet::new();
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
                let _ = msg_tx.send(Msg::Status(trf(&lang, "inbox_fetch_failed", &[&e.to_string()])));
                // 失败也要关首次加载态（否则 Inbox 永远停在「加载中」）
                let _ = msg_tx.send(Msg::InboxLoading(false));
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
    let rows: Vec<serde_json::Value> = all_ns.iter().map(|n| notif_row_json(n, &states)).collect();
    let _ = msg_tx.send(Msg::InboxRows(rows));
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
    let merged: Vec<serde_json::Value>;
    {
        let mut a = app.lock_recover();
        a.inbox_loading_more = false;
        match result {
            Ok((ns, more)) => {
                let mut seen: std::collections::HashSet<String> = a
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
                let _ = msg_tx.send(Msg::Status(trf(&lang, "inbox_fetch_failed", &[&e.to_string()])));
                return;
            }
        }
    }
    tracing::info!("fetch_inbox_more: page {} loaded, total {} rows", next_page, merged.len());
    let _ = msg_tx.send(Msg::InboxRows(merged));
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
    let lang = cfg.language.as_str();
    let _ = msg_tx.send(Msg::ReviewProgress {
        repo: repo.to_string(),
        number,
        text: trf(lang, "review_start", &[repo, &number.to_string()]),
    });
    // 进度回调 → channel（状态栏 + 面板进度，见 Msg::ReviewProgress）
    let tx = msg_tx.clone();
    let (r2, n2) = (repo.to_string(), number);
    let mut progress = move |text: String| {
        let _ = tx.send(Msg::ReviewProgress { repo: r2.clone(), number: n2, text });
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
                            let _ = msg_tx.send(Msg::ReviewProgress {
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
                            v["comments"] = serde_json::Value::Array(outcome.comments);
                        }
                        Err(e) => {
                            // 状态栏一闪而过很难捉到，同步写日志
                            tracing::warn!("OCR 明细获取失败 {repo} #{number}: {e}");
                            let _ = msg_tx.send(Msg::Status(trf(lang, "ocr_detail_failed", &[&e.to_string()])));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("OCR 前置 meta 拉取失败 {repo} #{number}: {e}");
                    let _ = msg_tx.send(Msg::Status(trf(lang, "ocr_detail_failed", &[&e.to_string()])));
                }
            }
            let ok = { app.lock_recover().db.upsert_review(&v).is_ok() };
            if ok {
                let _ = msg_tx.send(Msg::ReviewDone { repo: repo.to_string(), number });
            } else {
                let _ = msg_tx.send(Msg::ReviewFailed {
                    repo: repo.to_string(),
                    number,
                    err: tr(lang, "review_save_failed").into(),
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
    if cfg.active_sync().is_none() {
        &[]
    } else {
        &["server", "inbox"]
    }
}

/// 档案名 → db 文件路径：local → pr-desk.db；其它档 → profiles/<name>.db。
/// 名字先过字符合性校验（防路径穿越），非法名防御性回退本地库。
/// 每个档案独立 db 文件：sync 游标（sync_state 表）与 outbox 随库天然隔离。
fn profile_db_path(data_dir: &std::path::Path, name: &str) -> PathBuf {
    if name == "local" || !pr_desk_rs::config::valid_profile_name(name) {
        data_dir.join("pr-desk.db")
    } else {
        data_dir.join("profiles").join(format!("{name}.db"))
    }
}

/// 设置页档案聚焦回填：local → 空（表单字段锁定）；default → 顶层键；自定义 → 档案值。
fn profile_fields(cfg: &Config, name: &str) -> (String, String, String) {
    match name {
        "local" => (String::new(), String::new(), String::new()),
        "default" => (
            cfg.server_url.clone(),
            cfg.sync_token.clone(),
            cfg.astrbot_api_key.clone(),
        ),
        n => cfg
            .profiles
            .iter()
            .find(|p| p.name == n)
            .map(|p| (p.server_url.clone(), p.sync_token.clone(), p.api_key.clone()))
            .unwrap_or_default(),
    }
}

/// 本地模式刷新：本地库是手动策展的（只有 Inbox「+ 预评审」加入的 PR），
/// 不从 GitHub 自动拉任何 PR 列表——这里只刷新通知收件箱。
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
async fn do_sync(app: &Arc<Mutex<App>>, sync_client: &sync::SyncClient, msg_tx: &mpsc::Sender<Msg>, for_profile: &str) {
    if !sync_client.enabled() {
        // 本地模式：静默跳过，状态栏由 update_status_bar 显示「本地模式」
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
            let _ = msg_tx.send(Msg::Status(trf(
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
            let _ = msg_tx.send(Msg::Status(trf(&lang, "sync_failed", &[&e.to_string()])));
        }
    }
}

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
            let _ = msg_tx.send(Msg::Status(trf(&lang, "gh_client_init_failed", &[&e.to_string()])));
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

    // unbounded channel + recv().await：不在 tokio 工作线程上做 std 阻塞 recv
    while let Some(cmd) = rx.recv().await {
        // 每条命令开始时取一次界面语言（设置页切换语言后下条命令即生效）
        let lang = lang_of(&app);
        match cmd {
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
                    let _ = tx.send(Msg::RefreshDone(open_count));
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
                    let _ = tx.send(Msg::SyncFinished);
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
                                let _ = tx.send(Msg::DiffParsed { repo, number, files });
                                // diff 响应也带配额头，状态栏重组刷新配额
                                let _ = tx.send(Msg::StatusBarRefresh);
                            }
                            Err(e) => {
                                let lang = lang_of(&app2);
                                let _ = tx.send(Msg::Status(trf(&lang, "diff_fetch_failed", &[&e.to_string()])));
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
                        let _ = tx.send(Msg::InboxLoading(false));
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
                                let _ = tx.send(Msg::Status(trf(&lang, "mark_read_failed", &[&e.to_string()])));
                            }
                        }
                        let _ = tx.send(Msg::StatusBarRefresh);
                    });
                }
            }
            Cmd::FetchNotifDetail { repo, number, kind } => {
                // spawn：3 个串行网络请求是会阻塞命令循环的重头。消息带身份守卫，
                // 快速连点两条通知时旧结果不上屏（NotifDetail/NotifDetailFailed apply 比对）。
                // 会话条目的 markdown 解析也在 task 内完成（不占 UI 线程）。
                let g = gh.clone();
                let app2 = app.clone();
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    // 每个出口都必须回送消息：失败也要把预览面板的 nd_loading 置 false，
                    // 否则面板永远停在「加载中」。
                    let Some(g) = &g else {
                        let lang = lang_of(&app2);
                        let _ = tx.send(Msg::NotifDetailFailed {
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
                            let _ = tx.send(Msg::Status(trf(&lang, "convo_fetch_failed", &[&e.to_string()])));
                            Vec::new()
                        }
                    };
                    let (files, diff) = if kind == "PullRequest" {
                        let files = match g.fetch_pr_files(&repo, number).await {
                            Ok(f) => f,
                            Err(e) => {
                                let lang = lang_of(&app2);
                                let _ = tx.send(Msg::Status(trf(&lang, "files_fetch_failed", &[&e.to_string()])));
                                Vec::new()
                            }
                        };
                        let diff = match g.fetch_diff(&repo, number).await {
                            Ok(d) => d,
                            Err(e) => {
                                let lang = lang_of(&app2);
                                let _ = tx.send(Msg::Status(trf(&lang, "diff_fetch_failed", &[&e.to_string()])));
                                String::new()
                            }
                        };
                        (files, diff)
                    } else {
                        (Vec::new(), String::new())
                    };
                    let _ = tx.send(Msg::NotifDetail {
                        repo,
                        number,
                        convo: plain_convo_entries(&convo),
                        files_json: serde_json::to_string(&files).unwrap_or_else(|_| "[]".into()),
                        diff_files: parse_diff(&diff),
                    });
                    let _ = tx.send(Msg::StatusBarRefresh);
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
                            let _ = msg_tx.send(Msg::Status(tr(&lang, "review_copied").into()));
                        }
                        Err(e) => {
                            let _ = msg_tx.send(Msg::Status(trf(&lang, "review_copy_failed", &[&e.to_string()])));
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
                        let _ = msg_tx.send(Msg::Status(trf(&lang, "links_copied", &[&n.to_string()])));
                    }
                    Err(e) => {
                        let _ = msg_tx.send(Msg::Status(trf(&lang, "review_copy_failed", &[&e.to_string()])));
                    }
                }
            }
            Cmd::LoadReviewPrompt => {
                // 设置页打开时重读自定义评审模板（可能被外部编辑过）；空/缺失 = 默认模板
                let text = std::fs::read_to_string(pr_desk_rs::exe_dir().join("review_prompt.md"))
                    .unwrap_or_default();
                let _ = msg_tx.send(Msg::ReviewPromptLoaded(text));
            }
            Cmd::MarkAllNotifsRead => {
                if let Some(g) = gh.clone() {
                    let app2 = app.clone();
                    let tx = msg_tx.clone();
                    tokio::spawn(async move {
                        match g.mark_all_notifications_read().await {
                            Ok(()) => {
                                let lang = lang_of(&app2);
                                let _ = tx.send(Msg::Status(tr(&lang, "all_marked_read").into()));
                                fetch_inbox(&g, &app2, &tx).await;
                            }
                            Err(e) => {
                                let lang = lang_of(&app2);
                                let _ = tx.send(Msg::Status(trf(&lang, "mark_all_read_failed", &[&e.to_string()])));
                            }
                        }
                        let _ = tx.send(Msg::StatusBarRefresh);
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
                    let _ = msg_tx.send(Msg::Status(trf(&lang, "already_in_list", &[&repo, &number.to_string()])));
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
                    let _ = msg_tx.send(Msg::Status(if ok {
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
                // 重发当前通知行：apply_msg 重新计算「已加入」标记
                let rows = app.lock_recover().inbox.clone();
                let _ = msg_tx.send(Msg::InboxRows(rows));
            }
            Cmd::RunReview { repo, number } => {
                let (cfg, slots) = {
                    let a = app.lock_recover();
                    (a.cfg.clone(), a.review_slots.clone())
                };
                // LLM 配置缺失：立即提示，不占并发槽
                let Some(llm) = LlmSettings::from_config(&cfg) else {
                    let _ = msg_tx.send(Msg::Status(tr(&lang, "llm_not_configured").into()));
                    continue;
                };
                // 并发槽：同一 PR 重入 / 超额都立即拒绝（槽位由任务内 SlotGuard 释放）
                // 注意：try_acquire 的锁守卫随 let 语句结束即释放，match 臂内可再次加锁
                let acquired = slots.lock_recover().try_acquire(&repo, number);
                match acquired {
                    Ok(()) => {}
                    Err(SlotError::AlreadyRunning) => {
                        let _ = msg_tx.send(Msg::Status(tr(&lang, "review_already_running").into()));
                        continue;
                    }
                    Err(SlotError::AtCapacity) => {
                        let cap = slots.lock_recover().capacity.to_string();
                        let _ = msg_tx.send(Msg::Status(trf(&lang, "review_slots_full", &[&cap])));
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
                            let _ = tx.send(Msg::ReviewFailed {
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
                    let _ = tx.send(Msg::Status(tr(&lang2, "models_fetching").into()));
                    // 弹窗内联反馈：状态栏被设置弹窗遮罩盖住，拉取进度/失败要落到弹窗里
                    let _ = tx.send(Msg::DialogMsg(tr(&lang2, "models_fetching").into()));
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
                            let _ = tx.send(Msg::Status(trf(&lang2, "models_fetched", &[&n.to_string()])));
                        }
                        Err(e) => {
                            let msg = trf(&lang2, "models_fetch_failed", &[&e.to_string()]);
                            let _ = tx.send(Msg::Status(msg.clone()));
                            let _ = tx.send(Msg::DialogMsg(msg));
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
                                        let _ = tx.send(Msg::ReopenPanel);
                                    }
                                }
                            }
                            Err(e) => {
                                // 失败：移除节流标记（下次可立即重试）+ 状态栏报错
                                healed.lock_recover().remove(&(repo.clone(), number));
                                let lang = lang_of(&app2);
                                let _ = tx.send(Msg::Status(trf(
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
                        let _ = msg_tx.send(Msg::Status(tr(&lang, "quota_low_skip").into()));
                    }
                    continue;
                }
                if let Some(g) = gh.clone() {
                    // 乐观节流记账：spawn 前即刻标记，在飞期间的重复命令直接被节流拒掉；
                    // task 内拉取失败的部分由 task 撤标（失败不吃满 120s 惩罚窗口）。
                    // 克隆遮蔽同名局部变量，task 主体代码引用不变。
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
                                let _ = msg_tx.send(Msg::Status(trf(
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
                        updated_out = Some(model::rel_time(&meta_updated));
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
                        // 会话 = 评论 + 评审（按时间升序）；markdown 在 task 内解析
                        let mut convo_items: Vec<serde_json::Value> = convo
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
                        let _ = msg_tx.send(Msg::PanelLive {
                            repo,
                            number,
                            ci: ci_out,
                            state: state_out,
                            updated: updated_out,
                            convo: convo_out,
                            checks_json: checks_json_out,
                            files_json: files_json_out,
                        });
                        let _ = msg_tx.send(Msg::StatusBarRefresh);
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
                    let _ = msg_tx.send(Msg::Status(trf(&lang, "profile_unknown", &[&name])));
                    let (names, active) = {
                        let a = app.lock_recover();
                        (a.cfg.profile_names(), a.cfg.active_profile.clone())
                    };
                    let _ = msg_tx.send(Msg::Profiles { names, active });
                    continue;
                }
                if app.lock_recover().cfg.active_profile == name {
                    continue; // 同档重选：无操作
                }
                // 换库：local → pr-desk.db；其它档 → profiles/<name>.db
                //（sync 游标/outbox 在库文件内，换库天然隔离；远程新库游标为空 = 首次全量拉）
                let db_path = profile_db_path(&pr_desk_rs::exe_data_dir(), &name);
                if let Some(parent) = db_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let new_db = match Db::open(&db_path) {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = msg_tx.send(Msg::Status(trf(
                            &lang,
                            "profile_db_failed",
                            &[&name, &e.to_string()],
                        )));
                        // 换库失败：回填纠正 UI 选中（顶栏是乐观切换）
                        let (names, active) = {
                            let a = app.lock_recover();
                            (a.cfg.profile_names(), a.cfg.active_profile.clone())
                        };
                        let _ = msg_tx.send(Msg::Profiles { names, active });
                        continue;
                    }
                };
                {
                    let mut a = app.lock_recover();
                    a.cfg.active_profile = name.clone();
                    if let Err(e) = a.cfg.save(&cfg_path) {
                        // 落盘失败不阻断切换（内存态已切），仅提示
                        let _ = msg_tx.send(Msg::Status(trf(&lang, "cfg_save_failed", &[&e.to_string()])));
                    }
                    a.db = new_db; // 旧 Db 随 drop 关闭连接
                    a.selected = None;
                    a.selection.clear();
                    a.anchor = None;
                    a.inbox_detail = None;
                    a.rows.clear();
                    a.view.clear();
                    a.view_keys.clear();
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
                let _ = msg_tx.send(Msg::Profiles { names, active: name.clone() });
                let _ = msg_tx.send(Msg::ClosePanel);
                let _ = msg_tx.send(Msg::Status(trf(&lang, "profile_switched", &[&name])));
                // 走现有刷新路径：远程档 do_sync 全量拉；本地档 GitHub 火hose。
                // 但先让 UI 立即落地：换库后马上重排行 + 回放上一条通知列表，
                // 网络刷新 spawn 到后台，切换档案不再卡 worker 队列
                reload_rows(&app, &msg_tx);
                {
                    let cached = app.lock_recover().inbox.clone();
                    let _ = msg_tx.send(Msg::InboxRows(cached));
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
                        let _ = msg_tx.send(Msg::Status(trf(&lang, "gh_client_rebuild_failed", &[&e.to_string()])));
                    }
                }
                sync_client = sync::SyncClient::new(&cfg);
                let _ = msg_tx.send(Msg::Status(tr(&lang, "cfg_reloaded").into()));
                // 复用现有流程：sync 模式同步服务器列表；本地模式拉全部 open PR
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
}

/// 重建行模型并预取头像/组织图标（有新拉取时再次重建让 UI 拿到图片）。
/// 同步函数（DB 操作均为同步），不进入 async —— Slint 模型类型（SharedString）
/// 不是 Send，async fn 会在 await 点持有它们导致 future 非 Send。
fn reload_rows(app: &Arc<Mutex<App>>, msg_tx: &mpsc::Sender<Msg>) {
    let rows = {
        let a = app.lock_recover();
        match a.db.list_prs(list_sources(&a.cfg)) {
            Ok(rows) => rows,
            Err(e) => {
                // 失败时保留旧行并发状态栏错误——清空成空列表会让用户以为数据全没了
                tracing::warn!("列表加载失败: {e}");
                let lang = a.cfg.language.clone();
                drop(a);
                let _ = msg_tx.send(Msg::Status(trf(&lang, "list_load_failed", &[&e.to_string()])));
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
        orgs.extend(model::author_orgs(r));
    }
    app.lock_recover().rows = rows.clone();
    // serde_json 行可跨线程；Slint 模型在 UI 线程（apply_msg）装配
    let _ = msg_tx.send(Msg::Rows(rows));
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
            // 新头像落盘：重建一次让 UI 拿到图（缓存已新鲜，不会递归预取）
            reload_rows(&app2, &msg_tx2);
        }
    });
}

// ── UI 消息应用 ──

/// 用当前 filter 过滤 app.rows 并重建摊平 items 模型（折叠状态存 App.collapsed，跨重建保留）。
fn rebuild_rows_ui(ui: &MainWindow, app: &Arc<Mutex<App>>) {
    // 在 UI 线程装配 Slint 模型（SharedString/Image 是 Rc，不跨线程）
    let (rows_json, lang, sort_col, sort_asc, collapsed, selection) = {
        let a = app.lock_recover();
        (
            a.view.iter().map(|&i| a.rows[i].clone()).collect::<Vec<serde_json::Value>>(),
            a.cfg.language.clone(),
            a.sort_col.clone(),
            a.sort_asc,
            a.collapsed.clone(),
            a.selection.clone(),
        )
    };
    // view_keys：全部行（含折叠组）的展示序身份——行委托的 ridx 点击映射以此为准；
    // view_visible：可见行的展示序身份——j/k 键盘导航以它为准（与扁平化后的视觉顺序一致）。
    // 都不能用 a.view（数据库排序）代替：分组重排后不是一个序。
    let (items, keys, visible) = model::build_items(&rows_json, &lang, &sort_col, sort_asc, &collapsed, &selection);
    {
        let mut a = app.lock_recover();
        // 多选按身份修剪：被过滤/刷新掉的行退出多选（折叠组的行仍在 view_keys 里，保留）
        selection::retain_existing(&mut a.selection, &keys);
        if a.anchor.as_ref().is_some_and(|k| !keys.contains(k)) {
            a.anchor = None;
        }
        a.view_keys = keys;
        a.view_visible = visible;
    }
    ui.set_items(ModelRc::new(VecModel::from(items)));
    // 多选计数/菜单标志可能因修剪变化，与行模型一起回写
    push_selection_ui(ui, app);
    // 重建后高亮跟随选中 PR 的身份（下标已漂移，UI 侧以 selected_key 反查高亮）
    let sel_key = app
        .lock_recover()
        .selected
        .as_ref()
        .map(|(r, n)| format!("{r}#{n}"))
        .unwrap_or_default();
    ui.set_selected_key(sel_key.into());
}

/// 浏览器打开 PR：优先用行数据里的 url，缺失时按 repo/number 拼 GitHub 链接兜底。
fn open_pr_url(app: &Arc<Mutex<App>>, repo: &str, number: &i64) {
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

fn apply_msg(ui: &MainWindow, app: &Arc<Mutex<App>>, cmd_tx: &UnboundedSender<Cmd>, msg: Msg) {
    match msg {
        Msg::Status(s) => set_transient_status(ui, app, s),
        Msg::RefreshDone(open_count) => {
            // 刷新完成：停顶栏转圈（去重标志已在 worker 复位）+ 状态栏结果（走 6s 自动清）
            ui.set_gh_refreshing(false);
            let lang = lang_of(app);
            set_transient_status(ui, app, trf(&lang, "gh_refresh_done", &[&open_count.to_string()]));
        }
        Msg::SyncFinished => {
            // 同步结束（成败与否）：只负责停转圈；结果文案由 do_sync 自己的 Status 负责
            ui.set_syncing(false);
        }
        Msg::DialogMsg(s) => {
            // 设置弹窗内联反馈（模型拉取进度/失败等弹窗内发起的操作）
            ui.set_s_dialog_error(s.into());
        }
        Msg::InboxLoading(v) => ui.set_inbox_loading(v),
        Msg::StatusBarRefresh => update_status_bar(ui, app),
        Msg::ModelsLoaded(models) => {
            let v: Vec<SharedString> = models.iter().map(|m| m.as_str().into()).collect();
            ui.set_model_list(ModelRc::new(slint::VecModel::from(v)));
            // 拉取成功：清掉「正在拉取模型列表…」内联提示
            ui.set_s_dialog_error("".into());
        }
        Msg::Profiles { names, active } => {
            ui.set_profile_names(ModelRc::new(VecModel::from(
                names.iter().map(|n| n.as_str().into()).collect::<Vec<SharedString>>(),
            )));
            ui.set_active_profile(active.into());
        }
        Msg::SettingsProfile(name) => {
            // 写回 settings_profile 会触发 changed → settings_profile_focus 回填三字段
            ui.set_settings_profile(name.into());
        }
        Msg::ReopenPanel => {
            // 元数据补齐后重开面板（内容已变，身份不变）；
            // refetch=false：这是刷新重载，不许再派发拉取命令（防 FetchPrMeta 死循环）。
            // 装配在 worker（Cmd::OpenDrawer），UI 线程不做 DB/解析
            let sel = app.lock_recover().selected.clone();
            if let Some((repo, number)) = sel {
                let _ = cmd_tx.send(Cmd::OpenDrawer { repo, number, refetch: false });
            }
        }
        Msg::FilterChanged(q) => {
            {
                let mut a = app.lock_recover();
                a.filter = q;
                a.view = filtered_view(&a.rows, &a.filter, a.min_score);
            }
            rebuild_rows_ui(ui, app);
        }
        Msg::ViewPrefsChanged => {
            // 排序 / 高分快筛变化：重算过滤视图并重建行模型
            {
                let mut a = app.lock_recover();
                a.view = filtered_view(&a.rows, &a.filter, a.min_score);
            }
            rebuild_rows_ui(ui, app);
        }
        Msg::Rows(rows_json) => {
            {
                let mut a = app.lock_recover();
                a.total = rows_json.len();
                a.view = filtered_view(&a.rows, &a.filter, a.min_score);
            }
            rebuild_rows_ui(ui, app);
            update_status_bar(ui, app);
        }
        Msg::Panel { title, author, state, ci, opened, updated, body, priority, pinned, on_board, convo, checks_json, files_json, review } => {
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
            // 装盒（会话 markdown 已在 worker 解析；Checks/Files 是 JSON → 模型）
            ui.set_d_convo(ModelRc::new(VecModel::from(convo_models(convo))));
            ui.set_d_checks(ModelRc::new(VecModel::from(check_run_models(&checks_json))));
            ui.set_d_files(ModelRc::new(VecModel::from(file_change_models(&files_json))));
            // 评审 tab：装载已有评审；状态重置（避免带出上一个 PR 的 running/failed）
            apply_review_props(ui, &review);
            ui.set_d_review_status("".into());
            ui.set_d_review_progress("".into());
            ui.set_d_open(true);
        }
        Msg::ClosePanel => ui.set_d_open(false),
        Msg::ReviewProgress { repo, number, text } => {
            {
                // 直写 status_msg 也要推序号，否则在途的 6s 自动清除定时器会误清这条
                let mut a = app.lock_recover();
                a.status_msg = text.clone();
                a.status_gen = a.status_gen.wrapping_add(1);
            }
            update_status_bar(ui, app);
            if panel_matches(app, &repo, number) {
                ui.set_d_review_status("running".into());
                ui.set_d_review_progress(text.into());
            }
        }
        Msg::ReviewDone { repo, number } => {
            let lang = lang_of(app);
            {
                let mut a = app.lock_recover();
                a.status_msg = trf(&lang, "review_done", &[&repo, &number.to_string()]);
                a.status_gen = a.status_gen.wrapping_add(1);
            }
            update_status_bar(ui, app);
            // 评审分数进列表 Rev 列：重建行模型
            reload_rows_global(app);
            if panel_matches(app, &repo, number) {
                let rv = { app.lock_recover().db.get_review(&repo, number).ok().flatten() };
                apply_review_props(ui, &review_props(rv.as_ref(), &lang));
                ui.set_d_review_status("done".into());
                ui.set_d_review_progress("".into());
            }
        }
        Msg::ReviewFailed { repo, number, err } => {
            let lang = lang_of(app);
            {
                let mut a = app.lock_recover();
                a.status_msg = trf(&lang, "review_failed", &[&repo, &number.to_string(), &err]);
                a.status_gen = a.status_gen.wrapping_add(1);
            }
            update_status_bar(ui, app);
            if panel_matches(app, &repo, number) {
                ui.set_d_review_status("failed".into());
            }
        }
        Msg::InboxRows(rows_json) => {
            // 首次加载完成（成功路径）：关「加载中…」
            ui.set_inbox_loading(false);
            // 存原始 JSON + 仓库过滤视图（inbox_activated 按下标取 thread id，以过滤视图为准）
            let (rows, repos) = {
                let mut a = app.lock_recover();
                a.inbox = rows_json;
                a.inbox_view = inbox_filtered(&a);
                let rows = build_inbox_rows(&a, &a.inbox_view);
                // 仓库过滤下拉的去重列表（保序去重：按出现顺序；取未过滤全量）
                let mut seen = std::collections::HashSet::new();
                let repos: Vec<SharedString> = a
                    .inbox
                    .iter()
                    .filter_map(|n| {
                        let s = n["repo"].as_str().unwrap_or("").to_string();
                        if !s.is_empty() && seen.insert(s.clone()) {
                            Some(s.into())
                        } else {
                            None
                        }
                    })
                    .collect();
                (rows, repos)
            };
            tracing::info!("InboxRows: setting {} rows to UI", rows.len());
            ui.set_inbox_repos(ModelRc::new(VecModel::from(repos)));
            ui.set_inbox_rows(ModelRc::new(VecModel::from(rows)));
            // 分页状态：供「加载更多」按钮显隐与加载态
            {
                let a = app.lock_recover();
                ui.set_inbox_has_more(a.inbox_has_more);
                ui.set_inbox_loading_more(a.inbox_loading_more);
            }
            // 预览面板开着时同步刷新「已加入」状态（+ 预评审后重发 InboxRows 会走到这里）；
            // 查询失败保守按已加入处理（防重复插入）并告警
            let detail_added = {
                let a = app.lock_recover();
                a.inbox_detail.as_ref().map(|(repo, number, kind, _)| {
                    kind == "PullRequest"
                        && a.db.pr_exists(repo, *number).unwrap_or_else(|e| {
                            tracing::warn!("查询 PR 是否已加入失败 {repo} #{number}: {e}");
                            true
                        })
                })
            };
            if let Some(added) = detail_added {
                ui.set_nd_added(added);
            }
        }
        Msg::NotifDetail { repo, number, convo, files_json, diff_files } => {
            // 身份守卫：快速连点两条通知时，旧通知的内容不得覆盖正在看的那条
            let current = app
                .lock_recover()
                .inbox_detail
                .as_ref()
                .map(|(r, n, _, _)| (r.clone(), *n));
            if current.as_ref() != Some(&(repo, number)) {
                return;
            }
            // 装盒（会话 markdown 已在 worker 解析；Files 是 JSON → 模型）；diff 已在 worker 解析
            ui.set_nd_convo(ModelRc::new(VecModel::from(convo_models(convo))));
            ui.set_nd_files(ModelRc::new(VecModel::from(file_change_models(&files_json))));
            app.lock_recover().nd_diff_data = diff_files.clone();
            ui.set_nd_diff_files(ModelRc::new(VecModel::from(diff_row_models(&diff_files))));
            ui.set_nd_diff_max_text(diff_max_line(&diff_files));
            ui.set_nd_loading(false);
        }
        Msg::NotifDetailFailed { repo, number, err } => {
            // 拉取失败：身份匹配时关加载态并给状态栏错误（面板不再永远停在「加载中」）
            let current = app
                .lock_recover()
                .inbox_detail
                .as_ref()
                .map(|(r, n, _, _)| (r.clone(), *n));
            if current.as_ref() != Some(&(repo, number)) {
                return;
            }
            ui.set_nd_loading(false);
            {
                let mut a = app.lock_recover();
                a.status_msg = err;
                a.status_gen = a.status_gen.wrapping_add(1);
            }
            update_status_bar(ui, app);
        }
        Msg::ReviewPromptLoaded(text) => {
            // 回填完成：解除编辑锁定（打开设置页时置的 loading 到此结束）
            ui.set_s_review_prompt(text.into());
            ui.set_s_prompt_loading(false);
        }
        Msg::PanelLive { repo, number, ci, state, updated, convo, checks_json, files_json } => {
            // 实时数据只刷当前打开的面板；None 的区块本次未拉取，保持不动。
            // 面板已切走/关闭则丢弃（列表行已随落库 reload 更新）
            if panel_matches(app, &repo, number) {
                if let Some(ci) = ci {
                    ui.set_d_ci(ci.into());
                }
                if let Some(state) = state {
                    ui.set_d_state(state.into());
                }
                if let Some(updated) = updated {
                    ui.set_d_updated(updated.into());
                }
                if let Some(convo) = convo {
                    ui.set_d_convo(ModelRc::new(VecModel::from(convo_models(convo))));
                }
                if let Some(checks_json) = checks_json {
                    ui.set_d_checks(ModelRc::new(VecModel::from(check_run_models(&checks_json))));
                }
                if let Some(files_json) = files_json {
                    ui.set_d_files(ModelRc::new(VecModel::from(file_change_models(&files_json))));
                }
            }
        }
        Msg::DiffParsed { repo, number, files } => {
            // 身份守卫：快速切换 PR 时，A 的 diff 不得覆盖正在看的 B；
            // 不匹配时不上屏（diff 缓存已在 worker 落库，下次打开能用）。
            // UI 线程只做 VecModel 装盒（解析在 worker 完成）
            if panel_matches(app, &repo, number) {
                app.lock_recover().diff_data = files.clone();
                ui.set_diff_files(ModelRc::new(VecModel::from(diff_row_models(&files))));
                ui.set_diff_max_text(diff_max_line(&files));
            }
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
        slots.lock_recover().try_acquire("o/r", 1).unwrap();
        {
            let _g = SlotGuard { slots: slots.clone(), repo: "o/r".into(), number: 1 };
        }
        assert!(!slots.lock_recover().is_running("o/r", 1));
        assert!(slots.lock_recover().try_acquire("o/r", 1).is_ok());
    }

    // ── 选中态按身份（repo, number）查找 ──

    fn test_app(rows: Vec<serde_json::Value>) -> Arc<Mutex<App>> {
        let view = (0..rows.len()).collect();
        Arc::new(Mutex::new(App {
            cfg: Config::default(),
            db: Db::open_memory().unwrap(),
            rows,
            view,
            view_keys: Vec::new(),
            view_visible: Vec::new(),
            collapsed: HashSet::new(),
            filter: String::new(),
        sort_col: "upd".into(),
        sort_asc: false,
        min_score: 0.0,
            selected: None,
            selection: Vec::new(),
            anchor: None,
            status_msg: String::new(),
            status_gen: 0,
            gh_refreshing: false,
            sync_inflight: false,
            inbox: Vec::new(),
            inbox_view: Vec::new(),
            inbox_filter: String::new(),
            inbox_all: true,
            inbox_page: 1,
            inbox_has_more: false,
            inbox_loading_more: false,
            inbox_detail: None,
            diff_data: Vec::new(),
            nd_diff_data: Vec::new(),
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
            let a = app.lock_recover();
            assert_eq!(find_row(&a, "o/r", 2), Some(1));
            assert_eq!(find_row(&a, "o/r", 99), None);
            assert_eq!(find_row(&a, "x/y", 1), None);
        }
        // 模拟刷新后行序颠倒：身份查找仍命中同一个 PR，不会货不对板
        app.lock_recover().rows = vec![
            serde_json::json!({"repo": "o/r", "number": 2}),
            serde_json::json!({"repo": "o/r", "number": 1}),
        ];
        let a = app.lock_recover();
        assert_eq!(find_row(&a, "o/r", 2), Some(0));
        assert_eq!(find_row(&a, "o/r", 1), Some(1));
    }

    #[test]
    fn panel_matches_identity_surviving_row_removal() {
        let app = test_app(vec![serde_json::json!({"repo": "o/r", "number": 1})]);
        app.lock_recover().selected = Some(("o/r".into(), 1));
        assert!(panel_matches(&app, "o/r", 1));
        assert!(!panel_matches(&app, "o/r", 2));
        assert!(!panel_matches(&app, "x/y", 1));
        // 刷新后该行不在列表里：面板保持打开，身份保留，匹配继续工作
        app.lock_recover().rows = Vec::new();
        assert!(panel_matches(&app, "o/r", 1));
        // 关闭面板后不再匹配
        app.lock_recover().selected = None;
        assert!(!panel_matches(&app, "o/r", 1));
    }

    #[test]
    fn resolve_view_maps_view_index_to_identity() {
        let app = test_app(vec![
            serde_json::json!({"repo": "o/r", "number": 1, "title": "aaa"}),
            serde_json::json!({"repo": "o/r", "number": 2, "title": "bbb"}),
        ]);
        // UI 实际展示顺序（分组重排后）与数据库序不同：点击映射必须以 view_keys 为准
        app.lock_recover().view_keys = vec![("o/r".into(), 2), ("o/r".into(), 1)];
        assert_eq!(resolve_view(&app, 0), Some(("o/r".to_string(), 2)));
        assert_eq!(resolve_view(&app, 1), Some(("o/r".to_string(), 1)));
        assert_eq!(resolve_view(&app, 2), None);
    }

    // ── 评审展示数据扩展 ──

    #[test]
    fn size_line_formats_and_handles_missing_fields() {
        let rv = serde_json::json!({
            "size_label": "XL", "additions": 2122, "deletions": 23, "files_changed": 24,
        });
        assert_eq!(size_line(&rv, "zh"), "XL（+2122/-23，24 个文件）");
        assert_eq!(size_line(&rv, "en"), "XL (+2122/-23, 24 files)");
        assert_eq!(size_line(&serde_json::json!({}), "zh"), "");
        assert_eq!(size_line(&serde_json::json!({"size_label": "S"}), "zh"), "S");
        assert_eq!(
            size_line(&serde_json::json!({"additions": 10, "deletions": 2, "files_changed": 3}), "zh"),
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
        let p = review_props(Some(&rv), "zh");
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
        // 英文：分项标签与体量行切换
        let pe = review_props(Some(&rv), "en");
        assert_eq!(pe.size_line, "M (+100/-5, 4 files)");
        assert!(pe.dimensions.contains(&("Readability".to_string(), 8.0)));
        assert!(pe.dimensions.contains(&("Security".to_string(), 6.0)));
        // 无评审：空态各字段为空
        let p0 = review_props(None, "zh");
        assert!(!p0.has);
        assert!(p0.dimensions.is_empty());
        assert!(p0.summary_blocks.is_empty());
        assert_eq!(p0.size_line, "");
    }

    // ── 实时拉取的评审结论推导 ──

    fn review(state: &str, time: &str) -> serde_json::Value {
        serde_json::json!({"author": "a", "state": state, "time": time, "text": ""})
    }

    #[test]
    fn derive_review_decision_latest_decisive_wins() {
        // 最新一条 approved 胜过更早的 changes_requested
        let rvs = vec![
            review("changes_requested", "2026-01-01T00:00:00Z"),
            review("approved", "2026-01-02T00:00:00Z"),
        ];
        assert_eq!(derive_review_decision(&rvs), Some("approved"));
        // 反过来：最新的 changes_requested 胜过更早的 approved
        let rvs = vec![
            review("approved", "2026-01-01T00:00:00Z"),
            review("changes_requested", "2026-01-02T00:00:00Z"),
        ];
        assert_eq!(derive_review_decision(&rvs), Some("changes_requested"));
    }

    #[test]
    fn derive_review_decision_ignores_commented_and_dismissed() {
        // commented/dismissed 不参与最新结论；更早的 approved 仍生效
        let rvs = vec![
            review("approved", "2026-01-01T00:00:00Z"),
            review("commented", "2026-01-03T00:00:00Z"),
            review("dismissed", "2026-01-04T00:00:00Z"),
        ];
        assert_eq!(derive_review_decision(&rvs), Some("approved"));
        // 只有 commented
        assert_eq!(
            derive_review_decision(&[review("commented", "2026-01-01T00:00:00Z")]),
            Some("commented")
        );
        // 空 / 只有 dismissed → None
        assert_eq!(derive_review_decision(&[]), None);
        assert_eq!(
            derive_review_decision(&[review("dismissed", "2026-01-01T00:00:00Z")]),
            None
        );
    }

    // ── 串库竞态回归：在途 sync 不得写进切换后的档案库 ──

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

        let app = test_app(Vec::new());
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
