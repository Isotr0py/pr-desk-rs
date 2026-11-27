//! 共享状态与纯逻辑：App 结构、消息类型、行/评审/通知的展示数据装配。
//! 全部从旧 Slint 版 main.rs / model.rs 逐语义移植（竞态守卫、身份匹配、
//! 锁恢复、节流都保留注释出处）。UI 装配与 egui 绘制在其它模块。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use pr_desk_rs::config::Config;
use pr_desk_rs::db::Db;
use pr_desk_rs::i18n::{tr, trf};
use pr_desk_rs::{diffview, selection, sync};

/// 互斥锁加锁：锁中毒时恢复内层值继续用，不让一次锁内 panic 拖死整个 app。
/// 恢复语义安全的前提：锁内路径已把可避免的 unwrap/panic 改成 graceful 降级
///（模型行/远程数据不是不变量），中毒不等于数据损坏。
pub trait LockRecover<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for Mutex<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T> {
        Mutex::lock(self).unwrap_or_else(|e| e.into_inner())
    }
}

// ── 后台命令 / 回送消息 ──

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AutoReviewJob {
    pub profile: String,
    pub repo: String,
    pub number: i64,
}

pub enum Cmd {
    RefreshGithub,
    Sync,
    FetchDiff {
        repo: String,
        number: i64,
    },
    FetchInbox,            // 拉取 GitHub 通知（RefreshGithub/Sync 也会顺带触发）
    FetchInboxMore,        // Inbox「加载更多」：拉下一页追加
    MarkNotifRead(String), // 标记通知线程已读，完成后重新拉取
    MarkAllNotifsRead,     // PUT /notifications 全部已读，完成后重新拉取
    AddNotifReview {
        repo: String,
        number: i64,
        title: String,
        url: String,
    }, // Inbox「+ 预评审」入库
    FetchNotifDetail {
        repo: String,
        number: i64,
        kind: String,
    }, // Inbox 预览面板：拉会话/Files/Diff
    // 详情面板装配（原 UI 线程 open_drawer）：DB 读写 + diff 解析全在 worker，
    // UI 线程只把 Msg::Panel/DiffParsed 落地。refetch=false 用于 ReopenPanel（防 FetchPrMeta 死循环）
    OpenDrawer {
        repo: String,
        number: i64,
        refetch: bool,
    },
    CopyReview,        // 详情面板「复制评审」：get_review SQL + 剪贴板写入都在 worker
    CopyLinks(String), // 多选「复制链接」：换行拼接的 URL 文本，剪贴板写入在 worker
    // 右键/操作条「复制到档案…」：当前选择集（多选优先，否则当前行）+ 全部评审复制到目标档案
    CopyToProfile {
        target: String,
        keys: Vec<(String, i64)>,
    },
    ExportDatabase {
        destination: PathBuf,
    },
    ImportDatabase {
        source: PathBuf,
        name: String,
    },
    LoadReviewPrompt, // 设置页打开时重读 exe 旁 review_prompt.md（可能被外部编辑过）
    ReloadConfig,     // 设置保存后热生效：重建客户端并立即刷新+同步
    SwitchProfile(String), // 切换数据源档案（"local"/"default"/自定义名）：换库 + 重建同步客户端
    RunReview {
        repo: String,
        number: i64,
    }, // 详情面板「AI 评审」：LLM 评审并入库
    QueueAutoReviews {
        profile: String,
        keys: Vec<(String, i64)>,
    },
    AutoReviewSettled {
        job: AutoReviewJob,
        success: bool,
    },
    ReviewSlotReleased,
    PrepareKimiContext {
        key: crate::kimi_terminal::KimiKey,
    },
    FetchModels {
        base: String,
        key: String,
    }, // 设置页「获取」：拉 OpenAI 兼容端点的模型列表
    FetchPrMeta {
        repo: String,
        number: i64,
    }, // 补齐残缺行的 author/url/head_sha（Inbox 入库的行缺这些）
    FetchPrLive {
        repo: String,
        number: i64,
        parts: u8,
    }, // 面板实时拉取（parts 位标志：1=会话+评审 2=Files 4=Checks+meta；按 tab 惰性拉取）
    Shutdown, // 真正退出：worker 跳出命令循环（修「窗口关了进程不死」的僵尸问题）
}

/// FetchPrLive 的 parts 位标志：按 tab 惰性拉取（打开面板只带 CHECKS，会话/Files 切到才拉）。
pub const PART_CONVO: u8 = 1; // 会话 tab：评论 + 评审意见
pub const PART_FILES: u8 = 2; // Files tab
pub const PART_CHECKS: u8 = 4; // Checks tab / 打开面板（含 meta：状态徽章/更新时间）

/// 状态栏配置状态文本（语言取 cfg.language）。
pub fn config_status(cfg: &Config) -> String {
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
pub fn lang_of(app: &Arc<Mutex<App>>) -> String {
    app.lock_recover().cfg.language.clone()
}

pub enum Msg {
    Status(String),
    Rows(Vec<Value>),
    FilterChanged(String),
    ViewPrefsChanged, // 排序方式 / 高分快筛变化（重算过滤视图）
    StatusBarRefresh, // 只重组状态栏（配额 / 同步时间变化，不改临时消息）
    Panel(Box<PanelData>),
    ClosePanel,
    // AI 评审进度：状态栏显示 text；若详情面板正打开该 PR，面板置 running 并显示进度
    ReviewProgress {
        repo: String,
        number: i64,
        text: String,
    },
    ReviewDone {
        repo: String,
        number: i64,
    },
    ReviewFailed {
        repo: String,
        number: i64,
        err: String,
    },
    KimiContextReady {
        key: crate::kimi_terminal::KimiKey,
        context: Box<crate::kimi_terminal::PrContext>,
    },
    KimiContextFailed {
        key: crate::kimi_terminal::KimiKey,
        error: String,
    },
    // diff 的结构化解析结果（worker 解析完成，大 PR 几万行不占 UI 线程）。
    // 携带身份：面板已切走则不上屏（diff 缓存已在 worker 落库，下次打开能用）
    DiffParsed {
        repo: String,
        number: i64,
        files: Vec<DiffFileData>,
    },
    InboxRows(Vec<Value>),
    // Inbox 预览面板：会话条目已在 worker 完成 markdown 解析。携带身份：
    // apply 前与 app.inbox_detail 比对，快速连点两条通知时旧结果丢弃
    NotifDetail {
        repo: String,
        number: i64,
        convo: Vec<PlainConvoEntry>,
        files_json: String,
        diff_files: Vec<DiffFileData>,
    },
    NotifDetailFailed {
        repo: String,
        number: i64,
        err: String,
    },
    RefreshDone(usize),
    SyncFinished,
    DialogMsg(String),  // 设置弹窗内联错误行
    InboxLoading(bool), // Inbox 首次加载态
    ModelsLoaded(Vec<String>),
    Profiles {
        names: Vec<String>,
        active: String,
    },
    DatabaseExportFinished(Result<PathBuf, String>),
    DatabaseImportFinished(Result<String, String>),
    // 设置页档案名校正：新增档案非法时把 settings_profile 写回之前的有效值
    SettingsProfile(String),
    // 行元数据补齐完成：若面板正开着该 PR，重新装载面板（worker 只在成功补齐后发）
    ReopenPanel,
    ReviewPromptLoaded(String),
    // 实时拉取完成：面板正开着该 PR 时覆盖对应区块（None = 该区块本次未拉，不动）
    PanelLive {
        repo: String,
        number: i64,
        ci: Option<String>,
        state: Option<String>,
        updated: Option<String>,
        head_sha: Option<String>,
        convo: Option<Vec<PlainConvoEntry>>,
        checks_json: Option<String>,
        files_json: Option<String>,
    },
    // 会话头像预取落盘完成（有实际新拉取才发）：apply 侧按身份守卫后原地更新
    ConvoAvatars {
        repo: String,
        number: i64,
    },
    // 托盘：左键还原窗口 / 右键菜单「退出」
    TrayShow,
    TrayQuit,
    // 自动化验证指令（仅 PR_DESK_TEST=1 时由指令文件监听线程投递；
    // 经消息泵走 logic()，与 UI 回调同线程同路径调用处理器）
    TestCmd(String),
}

/// 会话条目展示数据（markdown 由 UI 侧 egui_commonmark 渲染；text 是原文）。
#[derive(Clone)]
pub struct PlainConvoEntry {
    pub kind: String,
    pub author: String,
    pub time: String,
    pub text: String,
}

/// OCR 行级评论展示数据。
#[derive(Clone)]
pub struct OcrCommentRow {
    pub severity: String,
    pub category: String,
    pub location: String, // 如 "src/foo.rs:42-58"
    pub content: String,
}

/// diff 文件的结构化解析结果（纯 Rust 数据可跨线程）。
/// lines 的 i32 是真实行号（0 = 无行号，见 diffview::numbered_lines）。
/// expanded：折叠状态，翻转后由 UI 缓存重建（前 3 个文件默认展开）。
#[derive(Clone)]
pub struct DiffFileData {
    pub path: String,
    pub lines: Vec<(diffview::LineKind, String, i32)>,
    pub expanded: bool,
}

/// 详情面板「评审」tab 的展示数据（从 db.get_review 的 JSON 格式化而来）。
#[derive(Clone, Default)]
pub struct ReviewProps {
    pub has: bool,
    pub score: f32,
    // 结构化分项（中文标签 + 分数）
    pub dimensions: Vec<(String, f32)>,
    pub summary: String,
    // 体量行，如 "XL（+2122/-23，24 个文件）"；缺字段时给空串
    pub size_line: String,
    pub flagged: Vec<String>,
    pub reviewed_at: String,
    pub comments: Vec<OcrCommentRow>,
}

#[derive(Clone, Default)]
pub struct ImportanceFactorProps {
    pub group: String,
    pub label: String,
    pub points: i32,
    pub max_points: u32,
    pub evidence: String,
}

#[derive(Clone, Default)]
pub struct ImportanceProps {
    pub score: Option<f32>,
    pub factors: Vec<ImportanceFactorProps>,
    pub method: String,
    pub version: String,
    pub updated_at: String,
}

/// 详情面板一次装配的全部数据（Msg::Panel 负载）。
pub struct PanelData {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub author: String,
    pub state: String,
    pub ci: String,
    pub updated: String,
    pub priority: i32,
    pub pinned: bool,
    pub on_board: bool,
    pub convo: Vec<PlainConvoEntry>,
    pub checks_json: String,
    pub files_json: String,
    pub importance: ImportanceProps,
    pub review: ReviewProps,
    // 评审三态："" = 无活动；"running" / "failed" / "done"
    pub review_status: String,
    pub review_progress: String,
    pub head_sha: String,
}

/// 评审分项键名 → 界面语言名（未知键原样展示）。
pub fn dim_label<'a>(key: &'a str, lang: &str) -> std::borrow::Cow<'a, str> {
    match key {
        "readability" => tr(lang, "dim_readability").into(),
        "correctness_risk" => tr(lang, "dim_correctness_risk").into(),
        "test_coverage" => tr(lang, "dim_test_coverage").into(),
        "security" => tr(lang, "dim_security").into(),
        other => other.into(),
    }
}

/// 评审字段可能是 JSON 字符串（旧数据）或内联对象/数组，两种都解析。
pub fn json_field(rv: &Value, key: &str) -> Option<Value> {
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
pub fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

/// 评审 JSON → 可粘贴到 GitHub 评论的 Markdown 全文（评分/维度/摘要/风险文件/行级评论）。
pub fn build_review_markdown(repo: &str, number: i64, rv: &Value, lang: &str) -> String {
    let mut s = format!("## Review: {repo}#{number}\n\n");
    if let Some(score) = rv["score"].as_f64() {
        s.push_str(&format!("**Score: {score:.1}/10**"));
        if let Some(sz) = rv["size_label"].as_str().filter(|x| !x.is_empty()) {
            s.push_str(&format!(" · {sz}"));
            if let (Some(a), Some(d), Some(f)) = (
                rv["additions"].as_i64(),
                rv["deletions"].as_i64(),
                rv["files_changed"].as_i64(),
            ) {
                s.push_str(&format!(" (+{a}/-{d}, {f} files)"));
            }
        }
        s.push_str("\n\n");
    }
    // dimensions 在库里是 JSON 字符串（尺寸小，解析失败就跳过整节）
    if let Some(dims) = rv["dimensions"]
        .as_str()
        .and_then(|d| serde_json::from_str::<serde_json::Map<String, Value>>(d).ok())
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
        .and_then(|f| serde_json::from_str::<Vec<Value>>(f).ok())
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
pub fn ocr_comment_location(c: &Value) -> String {
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
pub fn ocr_comment_rows(rv: &Value) -> Vec<OcrCommentRow> {
    let mut rows: Vec<OcrCommentRow> = json_field(rv, "comments")
        .and_then(|c| c.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|c| OcrCommentRow {
            severity: c["severity"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("other")
                .to_string(),
            category: c["category"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("other")
                .to_string(),
            location: ocr_comment_location(c),
            content: c["content"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    rows.sort_by_key(|c| severity_rank(&c.severity));
    rows
}

/// 体量行组装："XL（+2122/-23，24 个文件）"；label/增删行/文件数全缺时给空串。
pub fn size_line(rv: &Value, lang: &str) -> String {
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

pub fn review_props(review: Option<&Value>, lang: &str) -> ReviewProps {
    let Some(rv) = review else {
        return ReviewProps::default();
    };
    let dims = json_field(rv, "dimensions")
        .and_then(|d| d.as_object().cloned())
        .unwrap_or_default();
    let dimensions = dims
        .iter()
        .map(|(k, v)| {
            (
                dim_label(k, lang).to_string(),
                v.as_f64().unwrap_or(0.0) as f32,
            )
        })
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
        dimensions,
        summary,
        size_line: size_line(rv, lang),
        flagged,
        reviewed_at: rv["reviewed_at"].as_str().unwrap_or("").to_string(),
        comments: ocr_comment_rows(rv),
    }
}

pub fn importance_props(row: Option<&Value>, lang: &str) -> ImportanceProps {
    let Some(row) = row else {
        return ImportanceProps::default();
    };
    let Some(score) = row["importance_score"].as_f64() else {
        return ImportanceProps::default();
    };
    let factors = json_field(row, "importance_factors")
        .and_then(|factors| factors.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|factor| {
            let key = factor["key"].as_str().unwrap_or("");
            let group = factor["group"].as_str().unwrap_or(key);
            let group = match group {
                "severity" => "how_bad",
                "reach" => "community",
                other => other,
            };
            let label_key = match group {
                "how_bad" => "importance_group_how_bad",
                "who_it_hits" => "importance_group_who_it_hits",
                "release" => "importance_group_release",
                "evidence" => "importance_group_evidence",
                "community" => "importance_group_community",
                "pr_signals" => "importance_group_pr_signals",
                "penalty" => "importance_group_penalty",
                _ => "importance_factor_unknown",
            };
            ImportanceFactorProps {
                group: group.to_string(),
                label: tr(lang, label_key).to_string(),
                points: factor["points"].as_i64().unwrap_or(0) as i32,
                max_points: factor["max_points"].as_u64().unwrap_or(0) as u32,
                evidence: factor["evidence"].as_str().unwrap_or("").to_string(),
            }
        })
        .collect();
    ImportanceProps {
        score: Some(score as f32),
        factors,
        method: row["importance_method"].as_str().unwrap_or("").to_string(),
        version: row["importance_version"].as_str().unwrap_or("").to_string(),
        updated_at: row["importance_updated_at"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

// ── 预评审并发槽 ──

/// 普通预评审并发槽位：同一 PR 禁止重入，总并发受容量限制。
/// 容量来自配置 review_concurrency（设置页可改，保存即时生效）。
pub struct ReviewSlots {
    pub capacity: usize,
    standard_inflight: HashSet<(String, i64)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SlotError {
    AlreadyRunning, // 同一 PR 已有评审在跑
    AtCapacity,     // 槽位已满
}

impl ReviewSlots {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            standard_inflight: HashSet::new(),
        }
    }

    fn at_capacity(&self) -> bool {
        self.standard_inflight.len() >= self.capacity
    }

    pub fn try_acquire(&mut self, repo: &str, number: i64) -> Result<(), SlotError> {
        if self.standard_inflight.contains(&(repo.to_string(), number)) {
            return Err(SlotError::AlreadyRunning);
        }
        if self.at_capacity() {
            return Err(SlotError::AtCapacity);
        }
        self.standard_inflight.insert((repo.to_string(), number));
        Ok(())
    }

    pub fn release(&mut self, repo: &str, number: i64) {
        self.standard_inflight.remove(&(repo.to_string(), number));
    }

    /// 面板打开时查询：该 PR 是否有评审任务。
    pub fn is_running(&self, repo: &str, number: i64) -> bool {
        self.standard_inflight.contains(&(repo.to_string(), number))
    }

    pub fn is_standard_review_running(&self, repo: &str, number: i64) -> bool {
        self.standard_inflight.contains(&(repo.to_string(), number))
    }

    /// 设置页调整并发数（不影响已占用的槽位）。
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.clamp(1, 16);
    }
}

/// 槽位 RAII 守卫：任务结束时释放普通预评审槽位。
pub struct SlotGuard {
    pub slots: Arc<Mutex<ReviewSlots>>,
    pub repo: String,
    pub number: i64,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.slots.lock_recover().release(&self.repo, self.number);
    }
}

// ── App 状态 ──

/// 列表摊平后的展示项（组头与数据行混排；折叠组的行不进 flat）。
#[derive(Clone)]
pub enum FlatItem {
    Header {
        key: String,
        title: String,
        count: usize,
    },
    Row {
        rows_idx: usize,
    },
}

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub rows: Vec<Value>,    // 扁平数据行（按身份查找，见 find_row）
    pub flat: Vec<FlatItem>, // 展示序摊平（过滤+排序+分组+折叠后），渲染与点击映射以此为准
    pub view_visible: Vec<(String, i64)>, // 可见行的展示序身份；键盘导航以此为准
    pub collapsed: HashSet<String>, // 折叠的组 key；跨刷新保留
    pub filter: String,      // 当前过滤词（空 = 全量）
    pub sort_col: String,    // 组内排序列（默认 "upd" = 更新时间降序，即 SQL 输入序）
    pub sort_asc: bool,      // 排序方向（false = 降序）
    pub min_score: f32,      // 高分快筛阈值（0 = 关；8 = 只看 ≥8.0）
    pub selected: Option<(String, i64)>, // 选中 PR 身份（repo, number）；刷新后行序变化也不漂移
    pub selection: Vec<(String, i64)>, // 多选集合（Ctrl 点选/Shift 范围选，保持点选顺序；纯左键点击清空）
    pub anchor: Option<(String, i64)>, // Shift 范围选锚点（存身份而非下标：刷新后下标漂移、身份不漂）
    pub status_msg: String,            // 状态栏临时消息（最近一条 Status）
    pub status_gen: u64,               // 状态栏临时消息序号（自动清除定时器的竞态守卫）
    pub status_msg_at: Option<std::time::Instant>, // 临时消息写入时刻（egui 侧按 6s 到期清除，替代 Slint Timer）
    pub gh_refreshing: bool,                       // GitHub 刷新在途（顶栏转圈 + 连点去重）
    pub sync_inflight: bool,                       // 同步在途（顶栏转圈 + 连点/定时去重）
    pub inbox: Vec<Value>,                         // 最近一批通知行（未过滤；repo 下拉由此去重）
    pub inbox_view: Vec<Value>, // 仓库过滤后的通知行（inbox_activated 按下标取 thread id）
    pub inbox_filter: String,   // Inbox 仓库过滤（"" = 全部）
    pub inbox_all: bool,        // 通知拉取范围：true = 含已读（&all=true），false = 只看未读
    pub inbox_page: u32,        // 已加载到的页码（每页 50 条；「加载更多」追加下一页）
    pub inbox_has_more: bool,   // 是否还有下一页可加载
    pub inbox_loading_more: bool, // 「加载更多」进行中（防重入）
    pub inbox_fetching: bool,   // 通知拉取在飞（fetch_inbox 在飞去重：并发触发合并为一次+补跑）
    pub inbox_fetch_again: bool, // 在飞期间又来拉取需求：结束后补跑一次
    pub inbox_detail: Option<(String, i64, String, String)>, // 预览面板当前行（repo, number, kind, url）
    pub inbox_added: bool, // 预览面板当前行「已加入」标记（InboxRows 重发时重算）
    pub total: usize,      // 全部行数（状态栏「共 N 个 PR」）
    pub rate_limit: Arc<Mutex<Option<u64>>>, // GitHub X-RateLimit-Remaining 共享状态
    pub last_sync: sync::SharedLastSync, // 最近同步成功时间
    pub review_slots: Arc<Mutex<ReviewSlots>>, // 预评审并发槽（worker 与 UI 侧共享）
}

impl App {
    pub fn new(cfg: Config, db: Db) -> Self {
        Self {
            review_slots: Arc::new(Mutex::new(ReviewSlots::new(
                cfg.review_concurrency as usize,
            ))),
            cfg,
            db,
            rows: Vec::new(),
            flat: Vec::new(),
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
            status_msg_at: None,
            gh_refreshing: false,
            sync_inflight: false,
            inbox: Vec::new(),
            inbox_view: Vec::new(),
            inbox_filter: String::new(),
            inbox_all: true,
            inbox_page: 1,
            inbox_has_more: false,
            inbox_loading_more: false,
            inbox_fetching: false,
            inbox_fetch_again: false,
            inbox_detail: None,
            inbox_added: false,
            total: 0,
            rate_limit: Arc::new(Mutex::new(None)),
            last_sync: sync::shared_last_sync(),
        }
    }
}

/// 按身份（repo, number）在 rows 里现找下标。
pub fn find_row(a: &App, repo: &str, number: i64) -> Option<usize> {
    a.rows
        .iter()
        .position(|r| r["repo"].as_str() == Some(repo) && r["number"].as_i64() == Some(number))
}

/// 详情面板当前是否正打开指定 PR（评审进度/结果只刷对应面板）。
/// 选中态存身份（repo, number），刷新后行序变化/行被移除都不影响匹配。
pub fn panel_matches(app: &Arc<Mutex<App>>, repo: &str, number: i64) -> bool {
    let a = app.lock_recover();
    a.selected
        .as_ref()
        .map(|(r, n)| r == repo && *n == number)
        .unwrap_or(false)
}

/// 当前过滤词 + 高分快筛下可见行的原始下标。
pub fn filtered_view(rows: &[Value], filter: &str, min_score: f32) -> Vec<usize> {
    let q = filter.trim().to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| {
            // 高分快筛：min_score > 0 时只保留达到阈值的行（未评审 score=0 被滤掉）
            if min_score > 0.0 && (r["score"].as_f64().unwrap_or(0.0) as f32) < min_score {
                return false;
            }
            q.is_empty() || row_matches(r, &q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 重算展示序摊平（过滤 → 分组排序 → 折叠）：
/// - flat：渲染与点击映射的数据源；
/// - view_visible：可见行身份序列（键盘导航）；
/// - 多选按身份修剪：被过滤/刷新掉的行退出多选；锚点行消失则清锚。
pub fn recompute_flat(a: &mut App) {
    let view = filtered_view(&a.rows, &a.filter, a.min_score);
    let sub: Vec<Value> = view.iter().map(|&i| a.rows[i].clone()).collect();
    let mut flat = Vec::new();
    let mut visible = Vec::new();
    for sec in pr_desk_rs::build_sections(&sub, &a.cfg.language, &a.sort_col, a.sort_asc) {
        let is_collapsed = a.collapsed.contains(sec.key);
        flat.push(FlatItem::Header {
            key: sec.key.into(),
            title: sec.title.clone(),
            count: sec.idxs.len(),
        });
        if is_collapsed {
            continue;
        }
        for si in &sec.idxs {
            let r = &sub[*si];
            let id = (
                r["repo"].as_str().unwrap_or("").to_string(),
                r["number"].as_i64().unwrap_or(0),
            );
            visible.push(id);
            flat.push(FlatItem::Row {
                rows_idx: view[*si],
            });
        }
    }
    // 多选/锚点按身份修剪（fold 组的行不在 visible 里，但数据行修剪看全部 keys——
    // 折叠只是不显示，行还在列表数据里，不应被修剪掉，所以用 rows 全量身份而非 visible）
    let all_keys: Vec<(String, i64)> = a
        .rows
        .iter()
        .map(|r| {
            (
                r["repo"].as_str().unwrap_or("").to_string(),
                r["number"].as_i64().unwrap_or(0),
            )
        })
        .collect();
    selection::retain_existing(&mut a.selection, &all_keys);
    if a.anchor.as_ref().is_some_and(|k| !all_keys.contains(k)) {
        a.anchor = None;
    }
    a.flat = flat;
    a.view_visible = visible;
}

/// 批量操作的目标集合：多选非空用多选，否则回退主选中行（单选语义）。
pub fn effective_targets(a: &App) -> Vec<(String, i64)> {
    if !a.selection.is_empty() {
        a.selection.clone()
    } else {
        a.selected.clone().into_iter().collect()
    }
}

/// 多选状态摘要（喂给右键菜单/操作条）：数量 + 三个「全部已置位」标志。
/// 标志从当前行数据现读；行已被滤掉的身份按「未置位」计（偏向显示 Pin/Add/Skip）。
pub fn selection_summary(a: &App) -> (usize, bool, bool, bool) {
    let n = a.selection.len();
    if n == 0 {
        return (0, false, false, false);
    }
    let field_all = |field: &str| {
        a.selection.iter().all(|(r, num)| {
            find_row(a, r, *num)
                .and_then(|i| a.rows[i][field].as_i64())
                .unwrap_or(0)
                != 0
        })
    };
    let all_rejected = a.selection.iter().all(|(r, num)| {
        find_row(a, r, *num).and_then(|i| a.rows[i]["status"].as_str()) == Some("rejected")
    });
    (n, field_all("pinned"), field_all("on_board"), all_rejected)
}

// ── 行展示辅助（移植自已删除的 model.rs 的纯逻辑部分）──

/// 解析 updated_at：兼容 db::now() 的本地 "YYYY-MM-DDTHH:MM:SS"、带小数秒、
/// 空格分隔以及 RFC3339（带时区）格式。
pub fn parse_ts(s: &str) -> Option<chrono::NaiveDateTime> {
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

/// 相对时间格式："27m" / "2h" / "3d" / "2mo" / "1y"；未来时间归零为 "0m"，无时间戳返回 ""。
pub fn rel_time_at(ts: Option<chrono::NaiveDateTime>, now: chrono::NaiveDateTime) -> String {
    let Some(ts) = ts else { return String::new() };
    let mins = (now - ts).num_minutes().max(0);
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

pub fn rel_time(updated_at: &str) -> String {
    rel_time_at(parse_ts(updated_at), chrono::Local::now().naive_local())
}

/// 行内评审条数：优先 extras_detail.reviews 数组长度，其次 reviews 数字字段。
fn review_count(r: &Value) -> i64 {
    if let Some(n) = r["reviews"].as_i64() {
        return n;
    }
    if let Some(detail) = r["extras_detail"].as_str() {
        if let Ok(d) = serde_json::from_str::<Value>(detail) {
            if let Some(rvs) = d["reviews"].as_array() {
                return rvs.len() as i64;
            }
        }
    }
    0
}

/// 次级活动行（dim 文本）：「3 条评审」；无评审时返回 ""。
pub fn activity_text(r: &Value, lang: &str) -> String {
    let reviews = review_count(r);
    if reviews > 0 {
        trf(lang, "reviews_count", &[&reviews.to_string()])
    } else {
        String::new()
    }
}

/// 大小写不敏感子串匹配：title / number（query 可带 # 前缀）/ author / repo。
/// 内部自行转小写（调用方无需预处理）。
pub fn row_matches(r: &Value, query: &str) -> bool {
    let q = query.trim().trim_start_matches('#').to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hit = |s: &str| s.to_lowercase().contains(&q);
    hit(r["title"].as_str().unwrap_or(""))
        || hit(r["author"].as_str().unwrap_or(""))
        || hit(r["repo"].as_str().unwrap_or(""))
        || r["number"]
            .as_i64()
            .map(|n| n.to_string())
            .is_some_and(|n| n.contains(&q))
}

/// 作者所属组织列表。GitHub/sync 侧在行数据上提供 `author_orgs` 字段时生效，
/// 兼容 JSON 数组与 JSON 字符串两种形态；没有该字段时返回空 Vec。
pub fn author_orgs(r: &Value) -> Vec<String> {
    match &r["author_orgs"] {
        Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) => serde_json::from_str::<Vec<String>>(s)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// 头像字母降级的单字母：作者名首字符大写（空 → "?"）。
pub fn avatar_initial(author: &str) -> String {
    author
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

// ── Inbox / 通知 ──

/// subject.url（API URL）→ 浏览器可打开的 HTML URL：
/// `api.github.com/repos/` → `github.com/`，`/pulls/` → `/pull/`（issues 路径不变）。
pub fn notif_html_url(api_url: &str) -> String {
    api_url
        .replace("api.github.com/repos/", "github.com/")
        .replace("/pulls/", "/pull/")
}

/// 从 HTML URL 末段解析 PR/Issue 编号（…/pull/123 → 123）；非数字末段返回 None。
pub fn notif_url_number(html_url: &str) -> Option<i64> {
    html_url.rsplit('/').next()?.parse().ok()
}

/// GitHub 通知 subject.type（PullRequest/CheckSuite/…）→ 界面文案；未知值回退原串。
pub fn notif_kind_label(lang: &str, kind: &str) -> String {
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
pub fn notif_reason_label(lang: &str, reason: &str) -> String {
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
pub fn inbox_filtered(a: &App) -> Vec<Value> {
    a.inbox
        .iter()
        .filter(|n| {
            a.inbox_filter.is_empty() || n["repo"].as_str() == Some(a.inbox_filter.as_str())
        })
        .cloned()
        .collect()
}

// ── 详情面板装配的纯逻辑 ──

/// 评审结论推导：取 time（submitted_at）最新一条 approved/changes_requested 为准；
/// 只有 commented → "commented"；无评审 → None。dismissed 不计。
pub fn derive_review_decision(reviews: &[Value]) -> Option<&'static str> {
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
    if reviews
        .iter()
        .any(|r| r["state"].as_str() == Some("commented"))
    {
        Some("commented")
    } else {
        None
    }
}

/// extras_detail / recent_commits → (会话条目 JSON, Checks JSON)。
pub fn drawer_extras_json(r: &Value) -> (String, String) {
    let mut convo: Vec<Value> = Vec::new();
    let mut checks: Vec<Value> = Vec::new();
    if let Some(detail_str) = r["extras_detail"].as_str() {
        if let Ok(detail) = serde_json::from_str::<Value>(detail_str) {
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
                    convo.push(
                        serde_json::json!({"kind": "commit", "author": "", "time": "", "text": c}),
                    );
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
pub fn files_json_from_diff(diff: &str) -> String {
    let files: Vec<Value> = diffview::split_files(diff)
        .into_iter()
        .map(|(path, text)| {
            let adds = text.lines().filter(|l| l.starts_with('+')).count();
            let dels = text.lines().filter(|l| l.starts_with('-')).count();
            serde_json::json!({"path": path, "adds": adds, "dels": dels})
        })
        .collect();
    serde_json::to_string(&files).unwrap_or_else(|_| "[]".into())
}

/// diff 文本 → 结构化数据（前 3 个文件展开）。
/// 在 worker 调用：大 PR 几万行逐行解析是重活，不占 UI 线程。
pub fn parse_diff(diff: &str) -> Vec<DiffFileData> {
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

/// 会话 JSON 行 → 纯数据条目（markdown 渲染挪到 UI 侧 egui_commonmark）。
/// kind 缺失（通知预览的评论条目无 kind 字段）时按 "comment" 处理。
pub fn plain_convo_entries(entries: &[Value]) -> Vec<PlainConvoEntry> {
    entries
        .iter()
        .map(|e| PlainConvoEntry {
            kind: e["kind"].as_str().unwrap_or("comment").to_string(),
            author: e["author"].as_str().unwrap_or("").to_string(),
            time: rel_time(e["time"].as_str().unwrap_or("")),
            text: e["text"].as_str().unwrap_or("").to_string(),
        })
        .collect()
}

// ── 数据源档案 ──

/// 列表来源过滤：sync 启用时只显示服务器插件数据库跟踪的 PR + 手动预评审加的；
/// 本地模式显示全部。
pub fn list_sources(cfg: &Config) -> &'static [&'static str] {
    if cfg.active_sync().is_none() {
        &[]
    } else {
        &["server", "inbox"]
    }
}

/// 档案名 → db 文件路径：local → pr-desk.db；其它档 → profiles/<name>.db。
/// 名字先过字符合性校验（防路径穿越），非法名防御性回退本地库。
/// 每个档案独立 db 文件：sync 游标（sync_state 表）与 outbox 随库天然隔离。
pub fn profile_db_path(data_dir: &std::path::Path, name: &str) -> PathBuf {
    if name == "local" || !pr_desk_rs::config::valid_profile_name(name) {
        data_dir.join("pr-desk.db")
    } else {
        data_dir.join("profiles").join(format!("{name}.db"))
    }
}

/// 设置页档案聚焦回填：local → 空（表单字段锁定）；default → 顶层键；自定义 → 档案值。
pub fn profile_fields(cfg: &Config, name: &str) -> (String, String, String) {
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
            .map(|p| {
                (
                    p.server_url.clone(),
                    p.sync_token.clone(),
                    p.api_key.clone(),
                )
            })
            .unwrap_or_default(),
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
        assert_eq!(
            notif_url_number("https://github.com/o/r/pull/123"),
            Some(123)
        );
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
        assert!(s.try_acquire("o/r", 1).is_ok());
    }

    #[test]
    fn review_slots_track_standard_running_status() {
        let mut s = ReviewSlots::new(2);
        assert!(!s.is_running("o/r", 1));
        assert!(!s.is_standard_review_running("o/r", 1));

        s.try_acquire("o/r", 1).unwrap();
        assert!(s.is_running("o/r", 1));
        assert!(s.is_standard_review_running("o/r", 1));
        assert!(!s.is_running("o/r", 2));

        s.release("o/r", 1);
        assert!(!s.is_running("o/r", 1));
        assert!(!s.is_standard_review_running("o/r", 1));
    }

    #[test]
    fn review_slots_reject_duplicate_acquire() {
        let mut s = ReviewSlots::new(3);
        s.try_acquire("o/r", 1).unwrap();
        assert_eq!(s.try_acquire("o/r", 1), Err(SlotError::AlreadyRunning));
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
        s.release("o/r", 2);
        assert!(s.try_acquire("o/r", 4).is_ok());
        assert_eq!(s.try_acquire("o/r", 5), Err(SlotError::AtCapacity));
    }

    #[test]
    fn review_slots_release_unknown_is_noop() {
        let mut s = ReviewSlots::new(1);
        s.release("o/r", 99);
        assert!(s.try_acquire("o/r", 1).is_ok());
    }

    #[test]
    fn review_slots_duplicate_check_beats_capacity() {
        let mut s = ReviewSlots::new(1);
        s.try_acquire("o/r", 1).unwrap();
        assert_eq!(s.try_acquire("o/r", 1), Err(SlotError::AlreadyRunning));
    }

    #[test]
    fn slot_guard_releases_on_drop() {
        let slots = Arc::new(Mutex::new(ReviewSlots::new(1)));
        slots.lock_recover().try_acquire("o/r", 1).unwrap();
        {
            let _g = SlotGuard {
                slots: slots.clone(),
                repo: "o/r".into(),
                number: 1,
            };
        }
        assert!(!slots.lock_recover().is_running("o/r", 1));
        assert!(slots.lock_recover().try_acquire("o/r", 1).is_ok());
    }

    fn test_app(rows: Vec<Value>) -> Arc<Mutex<App>> {
        Arc::new(Mutex::new(App::new(
            Config::default(),
            Db::open_memory().unwrap(),
        )))
        .tap_rows(rows)
    }

    // 小工具：替换 rows 并重建 flat
    trait TapRows {
        fn tap_rows(self, rows: Vec<Value>) -> Self;
    }
    impl TapRows for Arc<Mutex<App>> {
        fn tap_rows(self, rows: Vec<Value>) -> Self {
            {
                let mut a = self.lock_recover();
                a.rows = rows;
                a.total = a.rows.len();
                recompute_flat(&mut a);
            }
            self
        }
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
        app.lock_recover().rows = Vec::new();
        assert!(panel_matches(&app, "o/r", 1));
        app.lock_recover().selected = None;
        assert!(!panel_matches(&app, "o/r", 1));
    }

    #[test]
    fn recompute_flat_groups_and_trims_selection() {
        let app = test_app(vec![
            serde_json::json!({"repo": "o/r", "number": 1, "status": "pending"}),
            serde_json::json!({"repo": "o/r", "number": 2, "status": "pending"}),
        ]);
        {
            let a = app.lock_recover();
            // 两行都在「待评审」组：header + 2 行
            assert_eq!(a.flat.len(), 3);
            assert!(matches!(a.flat[0], FlatItem::Header { .. }));
            assert_eq!(a.view_visible.len(), 2);
        }
        // 折叠后只剩 header，键盘可见行为空
        {
            let mut a = app.lock_recover();
            a.collapsed.insert("needs-review".into());
            recompute_flat(&mut a);
            assert_eq!(a.flat.len(), 1);
            assert!(a.view_visible.is_empty());
        }
        // 多选修剪：行被过滤掉后退出多选
        {
            let mut a = app.lock_recover();
            a.collapsed.clear();
            a.selection = vec![("o/r".into(), 1), ("o/r".into(), 2)];
            a.filter = "no-such".into();
            recompute_flat(&mut a);
            assert!(a.flat.is_empty());
            // 过滤掉但数据还在 rows 里：不修剪（折叠/过滤只是不显示）
            assert_eq!(a.selection.len(), 2);
            a.rows.clear();
            recompute_flat(&mut a);
            assert!(a.selection.is_empty());
        }
    }

    #[test]
    fn size_line_formats_and_handles_missing_fields() {
        let rv = serde_json::json!({
            "size_label": "XL", "additions": 2122, "deletions": 23, "files_changed": 24,
        });
        assert_eq!(size_line(&rv, "zh"), "XL（+2122/-23，24 个文件）");
        assert_eq!(size_line(&rv, "en"), "XL (+2122/-23, 24 files)");
        assert_eq!(size_line(&serde_json::json!({}), "zh"), "");
        assert_eq!(
            size_line(&serde_json::json!({"size_label": "S"}), "zh"),
            "S"
        );
        assert_eq!(
            size_line(
                &serde_json::json!({"additions": 10, "deletions": 2, "files_changed": 3}),
                "zh"
            ),
            "（+10/-2，3 个文件）"
        );
    }

    #[test]
    fn review_props_exposes_structured_dimensions_and_size() {
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
        assert!(p.summary.contains("结论"));
        let pe = review_props(Some(&rv), "en");
        assert_eq!(pe.size_line, "M (+100/-5, 4 files)");
        assert!(pe.dimensions.contains(&("Readability".to_string(), 8.0)));
        assert!(pe.dimensions.contains(&("Security".to_string(), 6.0)));
        let p0 = review_props(None, "zh");
        assert!(!p0.has);
        assert!(p0.dimensions.is_empty());
        assert_eq!(p0.size_line, "");
    }

    #[test]
    fn importance_props_distinguishes_missing_and_zero_scores() {
        assert!(importance_props(None, "zh").score.is_none());
        assert!(
            importance_props(Some(&serde_json::json!({})), "zh")
                .score
                .is_none()
        );
        let row = serde_json::json!({
            "importance_score": 0.0,
            "importance_factors": [
                {"key": "severity", "points": 0, "max_points": 50, "evidence": "No signal"},
                {"key": "reach", "points": 0, "max_points": 30, "evidence": "No issue"},
                {"group": "penalty", "key": "change_penalties", "points": -5, "max_points": 15, "evidence": "Large change"}
            ],
            "importance_version": "v1",
            "importance_updated_at": "2026-11-27T00:00:00Z"
        });
        let props = importance_props(Some(&row), "zh");
        assert_eq!(props.score, Some(0.0));
        assert_eq!(props.factors.len(), 3);
        assert_eq!(props.factors[0].label, "缺陷严重度");
        assert_eq!(props.factors[1].label, "社区关注");
        assert_eq!(props.factors[2].label, "惩罚");
        assert_eq!(props.factors[2].points, -5);
        assert_eq!(props.version, "v1");
        let english = importance_props(Some(&row), "en");
        assert_eq!(english.factors[0].label, "How bad");
        assert_eq!(english.factors[1].label, "Community");
    }

    fn review(state: &str, time: &str) -> Value {
        serde_json::json!({"author": "a", "state": state, "time": time, "text": ""})
    }

    #[test]
    fn derive_review_decision_latest_decisive_wins() {
        let rvs = vec![
            review("changes_requested", "2026-01-01T00:00:00Z"),
            review("approved", "2026-01-02T00:00:00Z"),
        ];
        assert_eq!(derive_review_decision(&rvs), Some("approved"));
        let rvs = vec![
            review("approved", "2026-01-01T00:00:00Z"),
            review("changes_requested", "2026-01-02T00:00:00Z"),
        ];
        assert_eq!(derive_review_decision(&rvs), Some("changes_requested"));
    }

    #[test]
    fn derive_review_decision_ignores_commented_and_dismissed() {
        let rvs = vec![
            review("approved", "2026-01-01T00:00:00Z"),
            review("commented", "2026-01-03T00:00:00Z"),
            review("dismissed", "2026-01-04T00:00:00Z"),
        ];
        assert_eq!(derive_review_decision(&rvs), Some("approved"));
        assert_eq!(
            derive_review_decision(&[review("commented", "2026-01-01T00:00:00Z")]),
            Some("commented")
        );
        assert_eq!(derive_review_decision(&[]), None);
        assert_eq!(
            derive_review_decision(&[review("dismissed", "2026-01-01T00:00:00")]),
            None
        );
    }

    #[test]
    fn rel_time_formats() {
        use chrono::Duration;
        let n = chrono::NaiveDateTime::parse_from_str("2026-02-01T12:00:00", "%Y-%m-%dT%H:%M:%S")
            .unwrap();
        let at = |d: Duration| rel_time_at(Some(n - d), n);
        assert_eq!(at(Duration::minutes(27)), "27m");
        assert_eq!(at(Duration::seconds(30)), "0m");
        assert_eq!(at(Duration::hours(2)), "2h");
        assert_eq!(at(Duration::days(3)), "3d");
        assert_eq!(at(Duration::days(60)), "2mo");
        assert_eq!(at(Duration::days(400)), "1y");
        assert_eq!(at(Duration::days(-1)), "0m");
        assert_eq!(rel_time_at(None, n), "");
    }

    #[test]
    fn parse_ts_formats() {
        use chrono::NaiveDateTime;
        let expect =
            NaiveDateTime::parse_from_str("2026-01-02T03:04:05", "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(parse_ts("2026-01-02T03:04:05"), Some(expect));
        assert_eq!(parse_ts("2026-01-02 03:04:05"), Some(expect));
        assert!(parse_ts("2026-01-02T03:04:05Z").is_some());
        assert_eq!(parse_ts(""), None);
        assert_eq!(parse_ts("garbage"), None);
    }

    #[test]
    fn author_orgs_parses_both_shapes() {
        let arr = serde_json::json!({"author_orgs": ["acme", "oss "]});
        assert_eq!(
            author_orgs(&arr),
            vec!["acme".to_string(), "oss".to_string()]
        );
        let s = serde_json::json!({"author_orgs": "[\"acme\"]"});
        assert_eq!(author_orgs(&s), vec!["acme".to_string()]);
        assert!(author_orgs(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn activity_text_shows_review_count_only() {
        use chrono::Duration;
        let recent = (chrono::Local::now().naive_local() - Duration::minutes(27))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let r = serde_json::json!({
            "updated_at": recent,
            "extras_detail": serde_json::to_string(&serde_json::json!({
                "reviews": [{"author": "a"}, {"author": "b"}, {"author": "c"}]
            })).unwrap(),
        });
        assert_eq!(activity_text(&r, "zh"), "3 条评审");
        assert_eq!(activity_text(&r, "en"), "3 reviews");
        let r2 = serde_json::json!({"updated_at": recent, "reviews": 2});
        assert_eq!(activity_text(&r2, "zh"), "2 条评审");
        assert_eq!(
            activity_text(&serde_json::json!({"updated_at": recent}), "zh"),
            ""
        );
        assert_eq!(activity_text(&serde_json::json!({}), "zh"), "");
    }

    #[test]
    fn avatar_initial_first_letter_or_fallback() {
        assert_eq!(avatar_initial("octocat"), "O");
        assert_eq!(avatar_initial("claude[bot]"), "C");
        assert_eq!(avatar_initial(""), "?");
        assert_eq!(avatar_initial("中文名"), "中");
    }

    #[test]
    fn row_matches_case_insensitive_substring() {
        let rows = vec![
            serde_json::json!({"repo": "acme/web", "number": 12, "title": "Fix Login Bug", "author": "Alice"}),
            serde_json::json!({"repo": "acme/api", "number": 34, "title": "Add endpoint", "author": "bob"}),
        ];
        assert!(row_matches(&rows[0], "login"));
        assert!(row_matches(&rows[0], "LOGIN"));
        assert!(row_matches(&rows[0], "ali"));
        assert!(row_matches(&rows[0], "acme"));
        assert!(row_matches(&rows[1], "34"));
        assert!(row_matches(&rows[1], "#34"));
        assert!(!row_matches(&rows[0], "zzz"));
    }
}
