//! PR Desk 桌面端：egui/eframe UI（自 Slint 版全量移植）。
//! 纯逻辑在 lib（pr_desk_rs）与本 crate 的 state.rs；worker.rs 是后台命令循环；
//! 本文件做装配：窗口/字体/主题/托盘/几何记忆/快捷键/消息落地。

mod avatars;
mod state;
mod tray;
mod ui_detail;
mod ui_inbox;
mod ui_list;
mod ui_settings;
mod worker;

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use pr_desk_rs::config::Config;
use pr_desk_rs::db::Db;
use pr_desk_rs::i18n::{tr, trf};

use state::*;

/// UI 侧即时状态（App 是业务状态；这里是面板/弹窗/表单等展示态）。
pub struct EguiApp {
    pub app: Arc<Mutex<App>>,
    pub cmd_tx: UnboundedSender<Cmd>,
    pub msg_rx: mpsc::Receiver<Msg>,
    pub worker_handle: Option<std::thread::JoinHandle<()>>,
    pub md_cache: egui_commonmark::CommonMarkCache,
    pub last_poll: Instant,
    pub cfg_path: PathBuf,
    // ── 视图 ──
    pub view_mode: usize, // 0 列表 / 1 Inbox
    pub profile_names: Vec<String>,
    pub filter_buf: String, // 顶栏搜索框文本（编辑即写回 App.filter 并重算视图）
    /// 列表列宽（表头拖拽调宽；标题列弹性不占位）。不持久化，退出重置默认。
    pub col_widths: Vec<f32>,
    /// Files tab 的 FetchDiff 去重标记（打开面板已按需拉过则不再补发）
    pub diff_requested: Option<(String, i64)>,
    /// debug：列边界覆盖层开关（colgrid 指令切换；验证列内容钉死在列框内）
    pub show_col_grid: bool,
    /// 列宽拖拽发生时刻（拖拽后松开那一下不得触发表头排序点击）
    pub header_drag_at: Option<std::time::Instant>,
    // debug：最近一帧的键盘占用/焦点（dumpstate 导出）
    pub last_wants_keyboard: bool,
    pub last_focus_id: String,
    // ── 详情面板 ──
    pub panel: Option<PanelData>,
    pub diff_files: Vec<DiffFileData>,
    pub panel_tab: usize,
    pub panel_width_cur: f32,
    // ── Inbox ──
    pub inbox_loading: bool,
    pub inbox_sel: Option<usize>, // 当前选中通知在 inbox_view 里的下标
    pub nd_open: bool,
    pub nd_title: String,
    pub nd_kind: String,
    pub nd_kind_label: String,
    pub nd_state: String,
    pub nd_loading: bool,
    pub nd_convo: Vec<PlainConvoEntry>,
    pub nd_files: Vec<(String, i64, i64)>,
    pub nd_diff: Vec<DiffFileData>,
    pub mar_confirm: Option<Instant>, // 「全部已读」两步确认（3s 超时还原）
    // ── 设置 ──
    pub settings_open: bool,
    pub settings_tab: usize,
    pub settings_profile: String,
    pub s_server: String,
    pub s_sync_token: String,
    pub s_ak: String,
    pub s_gh_token: String,
    pub s_proxy: String,
    pub s_repos: String,
    pub s_poll: i64,
    pub s_llm_base: String,
    pub s_llm_key: String,
    pub s_llm_model: String,
    pub s_ocr_path: String,
    pub s_review_conc: i64,
    pub s_review_prompt: String,
    pub s_prompt_loading: bool,
    pub dialog_error: String,
    pub model_list: Vec<String>,
    pub model_list_open: bool,
    pub adding_profile: bool,
    pub add_profile_name: String,
    pub del_confirm: Option<(String, Instant)>, // 档案删除两步确认（3s 超时还原）
    pub gh_token_from_env: bool,
    // ── 引导 ──
    pub onboarding_dismissed: bool,
    // ── 托盘 ──
    #[cfg(windows)]
    pub tray: Option<tray::Tray>,
    pub allow_close: bool,
    /// j/k 导航后请求列表滚动到该行（渲染时消费）
    pub scroll_to: Option<(String, i64)>,
}

impl EguiApp {
    pub fn lang(&self) -> String {
        self.app.lock_recover().cfg.language.clone()
    }

    /// 状态栏临时消息：覆盖显示，6 秒后自动清除（恢复「已同步 X 前」等常态文本）。
    /// 竞态守卫：每条消息递增 status_gen，到期清除时序号不匹配则不清（防旧定时器清新消息）。
    pub fn set_transient_status(&self, s: String) {
        let mut a = self.app.lock_recover();
        a.status_msg = s;
        a.status_gen = a.status_gen.wrapping_add(1);
        a.status_msg_at = Some(Instant::now());
    }

    /// 状态栏临时消息 6s 自动清除（egui 版：update 里检查时刻，替代 Slint Timer）。
    fn transient_tick(&self) {
        let mut clear = false;
        {
            let a = self.app.lock_recover();
            if let Some(at) = a.status_msg_at {
                if at.elapsed() > Duration::from_secs(6) && !a.status_msg.is_empty() {
                    clear = true;
                }
            }
        }
        if clear {
            let mut a = self.app.lock_recover();
            // 再校验：持锁期间可能有新消息进来（status_msg_at 已更新则不清）
            if let Some(at) = a.status_msg_at {
                if at.elapsed() > Duration::from_secs(6) {
                    a.status_msg.clear();
                    a.status_msg_at = None;
                }
            }
        }
    }

    /// 状态栏三段：同步状态/临时消息 · API 配额 · PR 总数。
    pub fn status_bar_sync_text(&self) -> String {
        let a = self.app.lock_recover();
        let lang = a.cfg.language.as_str();
        if !a.status_msg.is_empty() {
            a.status_msg.clone()
        } else if a.cfg.is_unconfigured() {
            // 未配置：临时消息 6s 自动清除后仍要留住「点右上角设置」的引导提示
            tr(lang, "cfg_unconfigured").into()
        } else if a.cfg.active_sync().is_none() {
            // 未配置同步：明确提示本地模式，不留空白
            tr(lang, "sb_local_mode").into()
        } else {
            pr_desk_rs::sync::last_sync_text(&a.last_sync, lang)
                .unwrap_or_else(|| tr(lang, "sb_wait_sync").into())
        }
    }

    /// 行点击（带修饰键语义）：
    /// 无修饰 = 清多选 + 单选 + 打开详情；
    /// Ctrl = toggle 进/出多选（集合为空时以当前选中行为起点扩充；不开详情——多选场景下面板频闪）；
    /// Shift = 从锚点到本行（view_visible 序）范围入多选（不开详情）。
    pub fn row_clicked(&self, key: (String, i64), ctrl: bool, shift: bool) {
        let open = !ctrl && !shift;
        {
            let mut a = self.app.lock_recover();
            if shift {
                let anchor = a.anchor.clone().unwrap_or_else(|| key.clone());
                let view = a.view_visible.clone();
                pr_desk_rs::selection::extend_range(&mut a.selection, &view, &anchor, &key);
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
                pr_desk_rs::selection::toggle(&mut a.selection, &key);
                a.anchor = Some(key.clone());
            } else {
                a.selection.clear();
                a.anchor = Some(key.clone());
            }
            a.selected = Some(key.clone());
        }
        if open {
            let _ = self.cmd_tx.send(Cmd::OpenDrawer { repo: key.0, number: key.1, refetch: true });
        }
    }

    /// 行右键：该行不在多选内则先单选它（多选内含它则不动多选），菜单由 UI 侧打开。
    pub fn row_context(&self, key: (String, i64)) {
        let mut a = self.app.lock_recover();
        if !pr_desk_rs::selection::is_selected(&a.selection, &key) {
            pr_desk_rs::selection::select_single(&mut a.selection, &key);
            a.anchor = Some(key.clone());
        }
        a.selected = Some(key);
    }

    /// 清空多选（Esc 第一级 / 操作条 ✕）；主选中行保留。
    pub fn clear_multi_selection(&self) {
        let mut a = self.app.lock_recover();
        a.selection.clear();
        a.anchor = None;
    }

    /// 组头折叠/展开：状态存 App.collapsed（跨刷新保留）。
    pub fn group_toggled(&self, key: &str) {
        let mut a = self.app.lock_recover();
        if !a.collapsed.remove(key) {
            a.collapsed.insert(key.to_string());
        }
        recompute_flat(&mut a);
    }

    /// 列表头点击排序（Excel 式）：同列再点反向；新列按该列的默认方向
    ///（数值/时间列先降序——最新/最大在前；文本列先升序）。
    pub fn header_clicked(&mut self, col: &str) {
        if self.header_drag_at.is_some_and(|t| t.elapsed().as_millis() < 300) {
            return; // 拖拽调宽刚松开：吞掉这次点击（不当成排序）
        }
        let mut a = self.app.lock_recover();
        if a.sort_col == col {
            a.sort_asc = !a.sort_asc;
        } else {
            a.sort_col = col.to_string();
            a.sort_asc = matches!(col, "num" | "title" | "author" | "turn" | "review" | "ci" | "size");
        }
        recompute_flat(&mut a);
    }

    /// 详情面板 tab 切换 → 惰性拉取对应数据（会话/Checks/Files；各自 120s 节流）。
    /// Files（合并 diff 视图）tab：diff 缓存缺失时补发 FetchDiff（每 PR 一次去重）。
    pub fn panel_tab_changed(&mut self, t: usize) {
        let parts = match t {
            1 => PART_CONVO,
            2 => PART_CHECKS,
            3 => PART_FILES,
            _ => return, // 评审用本地数据
        };
        let sel = self.app.lock_recover().selected.clone();
        if let Some((repo, number)) = sel {
            let _ = self.cmd_tx.send(Cmd::FetchPrLive { repo: repo.clone(), number, parts });
            if t == 3 && self.diff_files.is_empty() && self.diff_requested != Some((repo.clone(), number)) {
                self.diff_requested = Some((repo.clone(), number));
                let _ = self.cmd_tx.send(Cmd::FetchDiff { repo, number });
            }
        }
    }

    /// Inbox 行点击 / Enter：选中 + 展开右侧预览面板（发 FetchNotifDetail），并标记已读
    ///（下标对仓库过滤后的视图 inbox_view）。
    pub fn inbox_activated(&mut self, i: usize) {
        let row = self.app.lock_recover().inbox_view.get(i).cloned();
        let Some(n) = row else { return };
        let repo = n["repo"].as_str().unwrap_or("").to_string();
        let kind = n["kind"].as_str().unwrap_or("").to_string();
        let title = n["title"].as_str().unwrap_or("").to_string();
        let url = n["url"].as_str().unwrap_or("").to_string();
        let state = n["state"].as_str().unwrap_or("").to_string();
        let Some(number) = notif_url_number(&url) else { return };
        let lang = self.lang();
        {
            let mut a = self.app.lock_recover();
            a.inbox_detail = Some((repo.clone(), number, kind.clone(), url));
            // 查询失败是错误不是「不存在」：保守按已加入处理（防重复插入）并告警
            a.inbox_added = kind == "PullRequest" && {
                a.db.pr_exists(&repo, number).unwrap_or_else(|e| {
                    tracing::warn!("查询 PR 是否已加入失败 {repo} #{number}: {e}");
                    true
                })
            };
        }
        self.inbox_sel = Some(i);
        // 立即开面板并进加载态（数据由 worker 回送）
        self.nd_title = title;
        self.nd_kind_label = notif_kind_label(&lang, &kind);
        self.nd_kind = kind.clone();
        self.nd_state = state;
        self.nd_loading = true;
        self.nd_open = true;
        let _ = self.cmd_tx.send(Cmd::FetchNotifDetail { repo, number, kind });
        if let Some(id) = n["id"].as_str().filter(|s| !s.is_empty()) {
            let _ = self.cmd_tx.send(Cmd::MarkNotifRead(id.to_string()));
        }
    }

    /// 预览面板「在浏览器打开」/ Enter（面板已开时）。
    pub fn inbox_open_browser(&self) {
        let detail = self.app.lock_recover().inbox_detail.clone();
        if let Some((_, _, _, url)) = detail {
            if !url.is_empty() {
                if let Err(e) = open::that(&url) {
                    let lang = self.lang();
                    worker::send_msg(Msg::Status(trf(&lang, "open_browser_failed", &[&e.to_string()])));
                }
            }
        }
    }

    /// Inbox「+ 预评审」：按 UI 下标取 repo/number/title 交给 worker 入库。
    pub fn inbox_add_review(&self, i: usize) {
        let row = self.app.lock_recover().inbox_view.get(i).cloned();
        if let Some(n) = row {
            let repo = n["repo"].as_str().unwrap_or("").to_string();
            let title = n["title"].as_str().unwrap_or("").to_string();
            let url = n["url"].as_str().unwrap_or("").to_string();
            if let Some(number) = notif_url_number(&url) {
                let _ = self.cmd_tx.send(Cmd::AddNotifReview { repo, number, title, url });
            }
        }
    }

    /// 主题切换：即时持久化到 config.toml。
    pub fn theme_toggled(&self, ctx: &egui::Context) {
        let mut a = self.app.lock_recover();
        a.cfg.theme = if a.cfg.theme == "dark" { "light".into() } else { "dark".into() };
        let theme = a.cfg.theme.clone();
        if let Err(e) = a.cfg.save(&self.cfg_path) {
            tracing::warn!("主题写入 config.toml 失败: {e}");
        }
        drop(a);
        apply_theme(ctx, &theme);
    }

    /// 语言切换：持久化到 config.toml，并重算行视图（组标题语言跟随）。
    pub fn language_changed(&self, lang: &str) {
        let lang = if lang == "en" { "en" } else { "zh" }.to_string();
        {
            let mut a = self.app.lock_recover();
            if a.cfg.language == lang {
                return; // 同值：不落盘不重算
            }
            a.cfg.language = lang;
            if let Err(e) = a.cfg.save(&self.cfg_path) {
                tracing::warn!("语言写入 config.toml 失败: {e}");
            }
            recompute_flat(&mut a);
        }
    }

    /// 字号档位切换：即时生效并持久化（基准是 egui 默认字号表，不累乘漂移）。
    pub fn font_scale_changed(&self, ctx: &egui::Context, s: f32) {
        let snapped = if s < 0.95 { 0.9 } else if s > 1.07 { 1.15 } else { 1.0 };
        {
            let mut a = self.app.lock_recover();
            if (a.cfg.font_scale - snapped).abs() < 0.001 {
                return;
            }
            a.cfg.font_scale = snapped;
            if let Err(e) = a.cfg.save(&self.cfg_path) {
                tracing::warn!("字号写入 config.toml 失败: {e}");
            }
        }
        apply_font_scale(ctx, snapped);
    }

    /// 窗口/面板几何记忆：小于最小值的尺寸不写（防御垃圾值）。
    pub fn save_geometry(&self, ctx: &egui::Context) {
        let (w, h) = ctx.input(|i| {
            i.viewport().outer_rect.map(|r| (r.width() as f64, r.height() as f64)).unwrap_or((0.0, 0.0))
        });
        let pw = self.panel_width_cur as f64;
        let mut a = self.app.lock_recover();
        if w >= 900.0 && h >= 480.0 {
            a.cfg.window_width = Some(w);
            a.cfg.window_height = Some(h);
        }
        if pw >= 320.0 {
            a.cfg.panel_width = Some(pw);
        }
        if let Err(e) = a.cfg.save(&self.cfg_path) {
            tracing::warn!("窗口尺寸写入 config.toml 失败: {e}");
        }
    }

    fn apply_msg(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::Status(s) => self.set_transient_status(s),
            Msg::RefreshDone(open_count) => {
                // 刷新完成：停顶栏转圈（去重标志已在 worker 复位）+ 状态栏结果（走 6s 自动清）
                let lang = self.lang();
                self.set_transient_status(trf(&lang, "gh_refresh_done", &[&open_count.to_string()]));
            }
            Msg::SyncFinished => {
                // 同步结束（成败与否）：只负责停转圈；结果文案由 do_sync 自己的 Status 负责
            }
            Msg::DialogMsg(s) => self.dialog_error = s,
            Msg::InboxLoading(v) => self.inbox_loading = v,
            Msg::StatusBarRefresh => {} // 状态栏每帧从 App 现读，无需动作（重绘已由通道触发）
            Msg::ModelsLoaded(models) => {
                self.model_list = models;
                // 拉取成功：清掉「正在拉取模型列表…」内联提示
                self.dialog_error.clear();
            }
            Msg::Profiles { names, active: _ } => {
                self.profile_names = names;
                // active 已在 worker 写进 app.cfg.active_profile（顶栏现读）
            }
            Msg::SettingsProfile(name) => {
                // 档案名校正：新增档案非法时写回之前的有效值并回填三字段
                self.settings_profile = name;
                let (server, token, ak) = {
                    let a = self.app.lock_recover();
                    profile_fields(&a.cfg, &self.settings_profile)
                };
                self.s_server = server;
                self.s_sync_token = token;
                self.s_ak = ak;
            }
            Msg::ReopenPanel => {
                // 元数据补齐后重开面板（内容已变，身份不变）；
                // refetch=false：这是刷新重载，不许再派发拉取命令（防 FetchPrMeta 死循环）
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    let _ = self.cmd_tx.send(Cmd::OpenDrawer { repo, number, refetch: false });
                }
            }
            Msg::FilterChanged(q) => {
                let mut a = self.app.lock_recover();
                a.filter = q;
                recompute_flat(&mut a);
            }
            Msg::ViewPrefsChanged => {
                let mut a = self.app.lock_recover();
                recompute_flat(&mut a);
            }
            Msg::Rows(_) => {
                // worker 已把行写进 App 并 recompute_flat；这里无需动作
            }
            Msg::Panel(pd) => {
                self.diff_requested = None;
                self.panel = Some(*pd);
            }
            Msg::ClosePanel => self.panel = None,
            Msg::ReviewProgress { repo, number, text } => {
                {
                    // 直写 status_msg 也要推序号，否则在途的 6s 自动清除会误清这条
                    let mut a = self.app.lock_recover();
                    a.status_msg = text.clone();
                    a.status_gen = a.status_gen.wrapping_add(1);
                    a.status_msg_at = Some(Instant::now());
                }
                if panel_matches(&self.app, &repo, number) {
                    if let Some(p) = self.panel.as_mut() {
                        p.review_status = "running".into();
                        p.review_progress = text;
                    }
                }
            }
            Msg::ReviewDone { repo, number } => {
                let lang = self.lang();
                self.set_transient_status(trf(&lang, "review_done", &[&repo, &number.to_string()]));
                // 评审分数进列表 Rev 列：重载行
                worker::reload_rows_global(&self.app);
                if panel_matches(&self.app, &repo, number) {
                    let rv = { self.app.lock_recover().db.get_review(&repo, number).ok().flatten() };
                    if let Some(p) = self.panel.as_mut() {
                        p.review = review_props(rv.as_ref(), &lang);
                        p.review_status = "done".into();
                        p.review_progress.clear();
                    }
                }
            }
            Msg::ReviewFailed { repo, number, err } => {
                let lang = self.lang();
                self.set_transient_status(trf(&lang, "review_failed", &[&repo, &number.to_string(), &err]));
                if panel_matches(&self.app, &repo, number) {
                    if let Some(p) = self.panel.as_mut() {
                        p.review_status = "failed".into();
                    }
                }
            }
            Msg::InboxRows(rows_json) => {
                // 首次加载完成（成功路径）：关「加载中…」
                self.inbox_loading = false;
                // 存原始 JSON + 仓库过滤视图（inbox_activated 按下标取 thread id，以过滤视图为准）
                let mut a = self.app.lock_recover();
                a.inbox = rows_json;
                a.inbox_view = inbox_filtered(&a);
                // 预览面板开着时同步刷新「已加入」状态（+ 预评审后重发 InboxRows 会走到这里）；
                // 查询失败保守按已加入处理（防重复插入）并告警
                if let Some((repo, number, kind, _)) = a.inbox_detail.clone() {
                    a.inbox_added = kind == "PullRequest"
                        && a.db.pr_exists(&repo, number).unwrap_or_else(|e| {
                            tracing::warn!("查询 PR 是否已加入失败 {repo} #{number}: {e}");
                            true
                        });
                }
            }
            Msg::NotifDetail { repo, number, convo, files_json, diff_files } => {
                // 身份守卫：快速连点两条通知时，旧通知的内容不得覆盖正在看的那条
                let current = self
                    .app
                    .lock_recover()
                    .inbox_detail
                    .as_ref()
                    .map(|(r, n, _, _)| (r.clone(), *n));
                if current.as_ref() != Some(&(repo, number)) {
                    return;
                }
                self.nd_convo = convo;
                self.nd_files = ui_detail::parse_files_json(&files_json);
                self.nd_diff = diff_files;
                self.nd_loading = false;
            }
            Msg::NotifDetailFailed { repo, number, err } => {
                // 拉取失败：身份匹配时关加载态并给状态栏错误（面板不再永远停在「加载中」）
                let current = self
                    .app
                    .lock_recover()
                    .inbox_detail
                    .as_ref()
                    .map(|(r, n, _, _)| (r.clone(), *n));
                if current.as_ref() != Some(&(repo, number)) {
                    return;
                }
                self.nd_loading = false;
                self.set_transient_status(err);
            }
            Msg::ReviewPromptLoaded(text) => {
                // 回填完成：解除编辑锁定（打开设置页时置的 loading 到此结束）
                self.s_review_prompt = text;
                self.s_prompt_loading = false;
            }
            Msg::PanelLive { repo, number, ci, state: st, updated, convo, checks_json, files_json } => {
                // 实时数据只刷当前打开的面板；None 的区块本次未拉取，保持不动。
                // 面板已切走/关闭则丢弃（列表行已随落库 reload 更新）
                if panel_matches(&self.app, &repo, number) {
                    if let Some(p) = self.panel.as_mut() {
                        if let Some(ci) = ci {
                            p.ci = ci;
                        }
                        if let Some(st) = st {
                            p.state = st;
                        }
                        if let Some(updated) = updated {
                            p.updated = updated;
                        }
                        if let Some(convo) = convo {
                            p.convo = convo;
                        }
                        if let Some(cj) = checks_json {
                            p.checks_json = cj;
                        }
                        if let Some(fj) = files_json {
                            p.files_json = fj;
                        }
                    }
                }
            }
            Msg::DiffParsed { repo, number, files } => {
                // 身份守卫：快速切换 PR 时，A 的 diff 不得覆盖正在看的 B；
                // 不匹配时不上屏（diff 缓存已在 worker 落库，下次打开能用）。
                if panel_matches(&self.app, &repo, number) {
                    self.diff_files = files;
                }
            }
            Msg::ConvoAvatars { .. } => {
                // egui 的纹理缓存按文件 mtime 自动失效（avatars.rs load_cached），
                // 重绘已由通道触发——无需重装任何数据，天然「原地更新不闪烁」。
            }
            Msg::TrayShow => {
                tracing::info!("托盘：还原窗口");
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Msg::TrayQuit => {
                tracing::info!("托盘：退出");
                self.allow_close = true;
                self.save_geometry(ctx);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Msg::TestCmd(line) => self.handle_test_cmd(ctx, &line),
        }
    }

    /// ── 自动化验证指令（PR_DESK_TEST=1 时启用；%TEMP%/prdesk-cmd.txt 逐行指令）──
    /// 每条指令调用的就是对应 UI 控件背后的处理器（同函数同路径）。
    /// `dumpstate` 把关键状态写成 JSON（%TEMP%/prdesk-state.json）供断言。
    pub fn handle_test_cmd(&mut self, ctx: &egui::Context, line: &str) {
        let mut it = line.split_whitespace();
        let Some(cmd) = it.next() else { return };
        let arg = it.next().unwrap_or("");
        match cmd {
            "shot" => self.request_shot(ctx),
            "inbox" => {
                self.view_mode = 1;
                self.inbox_loading = true;
                let _ = self.cmd_tx.send(Cmd::FetchInbox);
            }
            "list" => self.view_mode = 0,
            "refresh" => {
                let mut a = self.app.lock_recover();
                if !a.gh_refreshing {
                    a.gh_refreshing = true;
                    drop(a);
                    let _ = self.cmd_tx.send(Cmd::RefreshGithub);
                }
            }
            "sync" => {
                let mut a = self.app.lock_recover();
                if !a.sync_inflight {
                    a.sync_inflight = true;
                    drop(a);
                    let _ = self.cmd_tx.send(Cmd::Sync);
                }
            }
            "theme" => self.theme_toggled(ctx),
            // row <n> [ctrl|shift]：点可见行（view_visible 序）
            // 按身份直接打开详情（测试指令；等价于点行 + Enter）
            "openpr" => {
                let number: i64 = it.next().unwrap_or("0").parse().unwrap_or(0);
                {
                    self.app.lock_recover().selected = Some((arg.to_string(), number));
                }
                let _ = self.cmd_tx.send(Cmd::OpenDrawer { repo: arg.to_string(), number, refetch: true });
            }
            "row" | "rowctrl" | "rowshift" => {
                let n: usize = arg.parse().unwrap_or(0);
                let key = self.app.lock_recover().view_visible.get(n).cloned();
                if let Some(key) = key {
                    self.row_clicked(key, cmd == "rowctrl", cmd == "rowshift");
                }
            }
            "enter" => {
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    let _ = self.cmd_tx.send(Cmd::OpenDrawer { repo, number, refetch: true });
                }
            }
            "esc" => {
                let (sel_count, panel_open) = {
                    let a = self.app.lock_recover();
                    (a.selection.len(), self.panel.is_some())
                };
                if self.settings_open {
                    self.settings_open = false;
                } else if self.view_mode == 1 && self.nd_open {
                    self.nd_open = false;
                    self.app.lock_recover().inbox_detail = None;
                } else if sel_count > 0 {
                    self.clear_multi_selection();
                } else if panel_open {
                    self.app.lock_recover().selected = None;
                    self.panel = None;
                } else {
                    self.app.lock_recover().selected = None;
                }
            }
            "key" => {
                // key j/k/o/p/1/2/3：与 hotkeys 同一处理器语义
                match arg {
                    "j" | "k" => {
                        let delta = if arg == "j" { 1i64 } else { -1 };
                        if self.view_mode == 0 {
                            let sel_key = {
                                let mut a = self.app.lock_recover();
                                if a.view_visible.is_empty() {
                                    None
                                } else {
                                    let n = a.view_visible.len();
                                    let pos = a
                                        .selected
                                        .as_ref()
                                        .and_then(|s| a.view_visible.iter().position(|k2| k2 == s));
                                    let npos = match pos {
                                        None => if delta > 0 { 0 } else { n - 1 },
                                        Some(p) => (p as i64 + delta).clamp(0, n as i64 - 1) as usize,
                                    };
                                    a.selected = Some(a.view_visible[npos].clone());
                        tracing::debug!("jk: pos={:?} npos={} n={}", pos, npos, n);
                                    a.selected.clone()
                                }
                            };
                            if let Some(k2) = sel_key {
                                self.scroll_to = Some(k2);
                            }
                        }
                    }
                    "o" => worker::batch_open_browser(&self.app),
                    "p" => {
                        let (has_sel, single) = {
                            let a = self.app.lock_recover();
                            (a.selection.is_empty(), a.selected.clone())
                        };
                        if !has_sel {
                            worker::batch_toggle(&self.app, "pinned");
                        } else if let Some((repo, number)) = single {
                            worker::toggle_field(&self.app, &repo, number, "pinned");
                        }
                    }
                    "1" | "2" | "3" => {
                        let p: i64 = arg.parse().unwrap();
                        let keys = {
                            let a = self.app.lock_recover();
                            effective_targets(&a)
                        };
                        if keys.len() > 1 {
                            worker::batch_local_update(&self.app, &keys, "priority", p);
                        } else if let Some((repo, number)) = keys.first().cloned() {
                            worker::local_update(&self.app, &repo, number, "priority", p);
                        }
                    }
                    _ => {}
                }
            }
            // 详情面板 tab（含惰性拉取）
            "tab" => {
                let t: usize = arg.parse().unwrap_or(0);
                self.panel_tab = t;
                self.panel_tab_changed(t);
            }
            "panelclose" => {
                self.app.lock_recover().selected = None;
                self.panel = None;
            }
            "review" => {
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    let _ = self.cmd_tx.send(Cmd::RunReview { repo, number });
                }
            }
            "copyreview" => {
                let _ = self.cmd_tx.send(Cmd::CopyReview);
            }
            // 批量（作用于多选集合，无多选则当前选中）
            "batch" => match arg {
                "pin" => worker::batch_toggle(&self.app, "pinned"),
                "board" => worker::batch_toggle(&self.app, "on_board"),
                "skip" => {
                    let keys = { let a = self.app.lock_recover(); effective_targets(&a) };
                    worker::batch_local_update(&self.app, &keys, "status", "rejected");
                }
                "restore" => {
                    let keys = { let a = self.app.lock_recover(); effective_targets(&a) };
                    worker::batch_local_update(&self.app, &keys, "status", "pending");
                }
                "links" => {
                    if let Some(text) = worker::copy_links_text(&self.app) {
                        let _ = self.cmd_tx.send(Cmd::CopyLinks(text));
                    }
                }
                "review" => {
                    let keys = { let a = self.app.lock_recover(); effective_targets(&a) };
                    let n = keys.len();
                    for (repo, number) in keys {
                        let _ = self.cmd_tx.send(Cmd::RunReview { repo, number });
                    }
                    let lang = self.lang();
                    worker::send_msg(Msg::Status(trf(&lang, "review_queued", &[&n.to_string()])));
                }
                _ => {}
            },
            "clearselection" => self.clear_multi_selection(),
            // Inbox
            "inboxrow" => {
                let n: usize = arg.parse().unwrap_or(0);
                self.inbox_activated(n);
            }
            "inboxadd" => {
                let n: usize = arg.parse().unwrap_or(0);
                self.inbox_add_review(n);
            }
            "inboxbrowser" => self.inbox_open_browser(),
            "inboxclose" => {
                self.nd_open = false;
                self.app.lock_recover().inbox_detail = None;
            }
            "markall" => {
                let _ = self.cmd_tx.send(Cmd::MarkAllNotifsRead);
            }
            "loadmore" => {
                let _ = self.cmd_tx.send(Cmd::FetchInboxMore);
            }
            "inboxfilter" => {
                let mut a = self.app.lock_recover();
                a.inbox_filter = arg.to_string();
                a.inbox_view = inbox_filtered(&a);
            }
            "unreadonly" => {
                let mut a = self.app.lock_recover();
                a.inbox_all = false;
                a.inbox_page = 1;
                a.inbox_has_more = false;
                drop(a);
                self.inbox_loading = true;
                let _ = self.cmd_tx.send(Cmd::FetchInbox);
            }
            // 设置页
            "settings" => crate::ui_settings::open_settings(self),
            "settingssave" => {
                let _ = crate::ui_settings::save_settings_pub(self);
            }
            "settab" => self.settings_tab = arg.parse().unwrap_or(0),
            // 设置页表单字段直写（验证 proxy_invalid 等校验路径）：setfield proxy|llm_base|llm_model <值>
            "setfield" => {
                let val = it.collect::<Vec<_>>().join(" ");
                match arg {
                    "proxy" => self.s_proxy = val,
                    "llm_base" => self.s_llm_base = val,
                    "llm_model" => self.s_llm_model = val,
                    "server" => self.s_server = val,
                    _ => {}
                }
            }
            "fetchmodels" => {
                let (base, key) = (self.s_llm_base.clone(), self.s_llm_key.clone());
                let _ = self.cmd_tx.send(Cmd::FetchModels { base, key });
            }
            // 档案切换
            "profile" => {
                let _ = self.cmd_tx.send(Cmd::SwitchProfile(arg.to_string()));
            }
            // 排序 / 快筛 / 组折叠
            "sort" => self.header_clicked(arg),
            "highscore" => {
                let mut a = self.app.lock_recover();
                a.min_score = if a.min_score > 0.0 { 0.0 } else { 8.0 };
                recompute_flat(&mut a);
            }
            "collapse" => self.group_toggled(arg),
            // 列宽调整（与表头拖拽同一赋值路径）：colwidth <列idx> <宽>
            "colgrid" => self.show_col_grid = !self.show_col_grid,
            // 拖拽调宽（与把手 Sense::drag 同语义：调宽 + 记拖拽时刻抑制点击排序）
            "coldrag" => {
                let i: usize = arg.parse().unwrap_or(0);
                let dx: f32 = it.next().unwrap_or("0").parse().unwrap_or(0.0);
                if let Some(spec) = ui_list::COL_SPECS.get(i) {
                    if !spec.flex {
                        self.col_widths[i] = (self.col_widths[i] + dx).max(spec.min_w);
                        self.header_drag_at = Some(std::time::Instant::now());
                    }
                }
            }
            "colwidth" => {
                let i: usize = arg.parse().unwrap_or(0);
                let w: f32 = it.next().unwrap_or("0").parse().unwrap_or(0.0);
                if let Some(spec) = ui_list::COL_SPECS.get(i) {
                    if !spec.flex && w > 0.0 {
                        self.col_widths[i] = w.max(spec.min_w);
                    }
                }
            }
            // 托盘
            "hide" => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.save_geometry(ctx);
            }
            "trayshow" => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            "quit" => {
                // 与托盘「退出」同路径（wndproc 菜单 → Msg::TrayQuit）
                self.allow_close = true;
                self.save_geometry(ctx);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            "dumpstate" => self.dump_state(),
            _ => tracing::warn!("未知测试指令: {line}"),
        }
        tracing::info!("test-cmd 已处理: {line}");
    }

    /// F12 / `shot` 指令共用：置位挂起，ui() 里发 ViewportCommand::Screenshot
    ///（窗口隐藏时 GL surface 尺寸为 0，直接发会让 egui_glow 回读 panic）。
    pub fn request_shot(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().inner_rect.map(|r| r.width() < 2.0 || r.height() < 2.0).unwrap_or(true)) {
            tracing::warn!("窗口隐藏/零尺寸，跳过截图");
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(|image| {
            let path = std::env::temp_dir().join("prdesk-shot.png");
            let img = image::DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(
                    image.width() as u32,
                    image.height() as u32,
                    image.as_raw().to_vec(),
                )
                .unwrap_or_default(),
            );
            match img.save(&path) {
                Ok(()) => tracing::info!("截图已存 {}", path.display()),
                Err(e) => tracing::warn!("截图保存失败: {e}"),
            }
        })));
    }

    /// 状态快照（JSON 落盘，供验证脚本断言）。
    fn dump_state(&self) {
        let a = self.app.lock_recover();
        let v = serde_json::json!({
            "view_mode": self.view_mode,
            "rows": a.rows.len(),
            "flat": a.flat.len(),
            "selected": a.selected,
            "selection": a.selection,
            "collapsed": a.collapsed,
            "filter": a.filter,
            "sort_col": a.sort_col,
            "sort_asc": a.sort_asc,
            "col_widths": self.col_widths,
            "header_drag_age_ms": self.header_drag_at.map(|t| t.elapsed().as_millis() as u64),
            "wants_keyboard": self.last_wants_keyboard,
            "focus_id": self.last_focus_id.clone(),
            "min_score": a.min_score,
            "panel_open": self.panel.is_some(),
            "panel_tab": self.panel_tab,
            "panel_repo": self.panel.as_ref().map(|p| p.repo.clone()),
            "panel_number": self.panel.as_ref().map(|p| p.number),
            "panel_review_has": self.panel.as_ref().map(|p| p.review.has),
            "panel_review_status": self.panel.as_ref().map(|p| p.review_status.clone()),
            "panel_convo": self.panel.as_ref().map(|p| p.convo.len()),
            "panel_checks": self.panel.as_ref().map(|p| p.checks_json.len()),
            "diff_files": self.diff_files.len(),
            "status_msg": a.status_msg,
            "gh_refreshing": a.gh_refreshing,
            "sync_inflight": a.sync_inflight,
            "inbox": a.inbox.len(),
            "inbox_view": a.inbox_view.len(),
            "inbox_has_more": a.inbox_has_more,
            "inbox_page": a.inbox_page,
            "inbox_loading": self.inbox_loading,
            "nd_open": self.nd_open,
            "nd_loading": self.nd_loading,
            "nd_convo": self.nd_convo.len(),
            "settings_open": self.settings_open,
            "dialog_error": self.dialog_error,
            "profile_names": self.profile_names,
            "active_profile": a.cfg.active_profile,
            "theme": a.cfg.theme,
            "language": a.cfg.language,
            "total": a.total,
        });
        let path = std::env::temp_dir().join("prdesk-state.json");
        if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()) {
            tracing::warn!("状态快照写入失败: {e}");
        }
    }

    /// 快捷键分发（无输入框聚焦时）。Esc 分级：设置弹窗 → 关弹窗；
    /// 引导 → 吞掉；Inbox 预览 → 关预览；多选 → 清多选；详情面板 → 关面板；→ 清选中。
    fn hotkeys(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        // debug：按键事件可见性（排查热键被吞）
        if true {
            let keys = ctx.input(|i| i.keys_down.iter().map(|k| format!("{k:?}")).collect::<Vec<_>>());
            let pressed: Vec<String> = ctx.input(|i| i.events.iter().filter_map(|e| match e {
                egui::Event::Key { key, pressed: true, .. } => Some(format!("{key:?}")),
                _ => None,
            }).collect());
            if !pressed.is_empty() {
                tracing::debug!("hotkeys: keys_down={keys:?} pressed={pressed:?} wants_kb={}", ctx.egui_wants_keyboard_input());
            }
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let settings_or_onboarding = self.settings_open
            || (self.app.lock_recover().cfg.is_unconfigured() && !self.onboarding_dismissed);
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if settings_or_onboarding {
            if esc && self.settings_open {
                self.settings_open = false;
            }
            return; // 快捷键不穿透到背后的列表（弹窗内输入框正常收键）
        }
        if esc {
            if self.view_mode == 1 && self.nd_open {
                self.nd_open = false;
                self.app.lock_recover().inbox_detail = None;
                return;
            }
            let (sel_count, panel_open) = {
                let a = self.app.lock_recover();
                (a.selection.len(), self.panel.is_some())
            };
            if sel_count > 0 {
                self.clear_multi_selection();
            } else if panel_open {
                self.app.lock_recover().selected = None;
                self.panel = None;
            } else {
                self.app.lock_recover().selected = None;
            }
            return;
        }
        // j/k 移动
        let (j, k) = (ctx.input(|i| i.key_pressed(egui::Key::J)), ctx.input(|i| i.key_pressed(egui::Key::K)));
        if j || k {
            let delta = if j { 1i64 } else { -1i64 };
            if self.view_mode == 0 {
                let sel_key = {
                    let mut a = self.app.lock_recover();
                    if a.view_visible.is_empty() {
                        None
                    } else {
                        let n = a.view_visible.len();
                        let pos = a
                            .selected
                            .as_ref()
                            .and_then(|s| a.view_visible.iter().position(|k2| k2 == s));
                        let npos = match pos {
                            None => {
                                if delta > 0 { 0 } else { n - 1 }
                            }
                            Some(p) => (p as i64 + delta).clamp(0, n as i64 - 1) as usize,
                        };
                        a.selected = Some(a.view_visible[npos].clone());
                        tracing::debug!("jk: pos={:?} npos={} n={}", pos, npos, n);
                        a.selected.clone()
                    }
                };
                if let Some(k2) = sel_key {
                    // 滚动到选中行：渲染时由行消费（scroll_to_rect）
                    self.scroll_to = Some(k2);
                }
            } else {
                // Inbox：在过滤视图上下标移动
                let n = self.app.lock_recover().inbox_view.len();
                if n > 0 {
                    let pos = self.inbox_sel.unwrap_or(if delta > 0 { usize::MAX } else { 0 });
                    let npos = if pos == usize::MAX {
                        0
                    } else {
                        (pos as i64 + delta).clamp(0, n as i64 - 1) as usize
                    };
                    self.inbox_sel = Some(npos);
                }
            }
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            if self.view_mode == 0 {
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    let _ = self.cmd_tx.send(Cmd::OpenDrawer { repo, number, refetch: true });
                }
            } else if let Some(i) = self.inbox_sel {
                if self.nd_open {
                    self.inbox_open_browser();
                } else {
                    self.inbox_activated(i);
                }
            }
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::O)) {
            if self.view_mode == 0 {
                worker::batch_open_browser(&self.app);
            } else {
                self.inbox_open_browser();
            }
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::P)) && self.view_mode == 0 {
            // 快捷键 p：多选存在时批量 toggle，否则单行
            let (has_sel, single) = {
                let a = self.app.lock_recover();
                (a.selection.is_empty(), a.selected.clone())
            };
            if !has_sel {
                worker::batch_toggle(&self.app, "pinned");
            } else if let Some((repo, number)) = single {
                worker::toggle_field(&self.app, &repo, number, "pinned");
            }
            return;
        }
        // 快捷键 1/2/3：多选存在时批量设优先级，否则单行
        for (key, p) in [(egui::Key::Num1, 1i64), (egui::Key::Num2, 2), (egui::Key::Num3, 3)] {
            if ctx.input(|i| i.key_pressed(key)) && self.view_mode == 0 {
                let keys = {
                    let a = self.app.lock_recover();
                    effective_targets(&a)
                };
                if keys.len() > 1 {
                    worker::batch_local_update(&self.app, &keys, "priority", p);
                } else if let Some((repo, number)) = keys.first().cloned() {
                    worker::local_update(&self.app, &repo, number, "priority", p);
                }
                return;
            }
        }
        // "/" 聚焦搜索框
        if ctx.input(|i| i.key_pressed(egui::Key::Slash)) {
            ctx.memory_mut(|m| m.request_focus(ui_list::search_id()));
        }
    }

    /// 轮询定时（每轮重读当前间隔，设置页改间隔下轮生效）；
    /// 在途去重：上一轮还没跑完就跳过本轮（不排队堆叠全量同步）。
    fn poll_tick(&mut self, ctx: &egui::Context) {
        let interval = {
            let a = self.app.lock_recover();
            Duration::from_secs(a.cfg.poll_interval_min.max(1) * 60)
        };
        let elapsed = self.last_poll.elapsed();
        if elapsed >= interval {
            self.last_poll = Instant::now();
            let send = {
                let mut a = self.app.lock_recover();
                if a.sync_inflight {
                    false
                } else {
                    a.sync_inflight = true;
                    true
                }
            };
            if send {
                let _ = self.cmd_tx.send(Cmd::Sync);
            }
        } else {
            ctx.request_repaint_after((interval - elapsed).min(Duration::from_secs(5)));
        }
    }
}

impl eframe::App for EguiApp {
    /// 非绘制逻辑（eframe 0.37：窗口被完全遮挡/隐藏时不跑 ui()，只跑 logic()——
    /// 消息落地/轮询/托盘还原必须在这里，否则窗口被盖住后 worker 消息全堵死，
    /// 托盘「显示」也永远不生效）。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 诊断（debug 级）：指针/点击事件可见性
        {
            let ptr = ctx.input(|i| i.pointer.latest_pos());
            let click = ctx.input(|i| i.pointer.any_click());
            if ptr.is_some() || click {
                tracing::debug!("logic: ptr={ptr:?} click={click}");
            }
        }
        // ── 消息落地 ──
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.apply_msg(ctx, msg);
        }
        // F12 = 视口截图存 %TEMP%/prdesk-shot.png（自动化验证读屏用：
        // 回调由 GPU 回读，不受窗口遮挡/多屏 DPI 影响；PostMessage WM_KEYDOWN 可达）
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            self.request_shot(&ctx);
        }
        self.transient_tick();
        self.poll_tick(ctx);

        // ── 关窗 → 托盘（而非退出）──
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            tracing::info!("收到 close_requested：隐藏到托盘");
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.save_geometry(ctx);
        }

        self.last_wants_keyboard = ctx.egui_wants_keyboard_input();
        self.last_focus_id = ctx.memory(|m| m.focused()).map(|id| format!("{id:?}")).unwrap_or_default();
        self.hotkeys(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // ── 布局：顶栏 / 状态栏 /（选中条）/ 中央视图 ──
        ui_list::top_bar(self, ui);
        ui_list::status_bar(self, ui);
        ui_list::selection_bar(self, ui);
        match self.view_mode {
            0 => {
                ui_detail::detail_panel(self, ui);
                ui_list::pr_list_view(self, ui);
            }
            _ => {
                ui_inbox::notif_preview(self, ui);
                ui_inbox::inbox_view(self, ui);
            }
        }

        // ── 弹窗（模态，顶层）──
        ui_settings::settings_window(self, &ctx);
        ui_settings::onboarding_window(self, &ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 真正退出（托盘「退出」）：通知 worker 停命令循环 + 收托盘线程
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(t) = self.tray.as_mut() {
            t.stop();
        }
        if let Some(h) = self.worker_handle.take() {
            let _ = h.join();
        }
    }
}

// ── 主题 / 字体 ──

pub fn apply_theme(ctx: &egui::Context, theme: &str) {
    ctx.set_theme(if theme == "dark" { egui::Theme::Dark } else { egui::Theme::Light });
}

/// 字号档位：从 egui 默认基准重算（不累乘，避免多次切换漂移）。
pub fn apply_font_scale(ctx: &egui::Context, scale: f32) {
    use std::collections::BTreeMap;
    let base: BTreeMap<egui::TextStyle, egui::FontId> = egui::Style::default().text_styles;
    ctx.all_styles_mut(|style| {
        style.text_styles = base
            .iter()
            .map(|(k, v)| (k.clone(), egui::FontId { size: v.size * scale, ..v.clone() }))
            .collect();
    });
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
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .push("cjk".into());
                fonts
                    .families
                    .entry(egui::FontFamily::Monospace)
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

    // 窗口几何记忆：配置里有记录才应用（首启不覆盖默认值；小于最小值视为垃圾忽略）
    let (w, h) = match (cfg.window_width, cfg.window_height) {
        (Some(w), Some(h)) if w >= 900.0 && h >= 480.0 => (w as f32, h as f32),
        _ => (1280.0, 760.0),
    };
    // 窗口图标：PR 形状 logo（assets/app-icon-256.png，Slint 版 app.svg 的离线渲染）
    let icon = {
        let img = image::load_from_memory(include_bytes!("../assets/app-icon-256.png"))
            .expect("内嵌图标 PNG 解码失败")
            .to_rgba8();
        let (iw, ih) = img.dimensions();
        egui::IconData { rgba: img.into_raw(), width: iw, height: ih }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([w, h])
            .with_icon(icon),
        ..Default::default()
    };

    let unconfigured = cfg.is_unconfigured();
    let gh_token_from_env = std::env::var("GITHUB_TOKEN").map(|t| !t.is_empty()).unwrap_or(false);
    let profile_names = cfg.profile_names();
    let status_msg = config_status(&cfg);
    let s_review_prompt = String::new(); // 打开设置页时经 LoadReviewPrompt 重读

    eframe::run_native(
        "PR Desk",
        options,
        Box::new(move |cc| {
            install_cjk_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, &cfg.theme);
            apply_font_scale(&cc.egui_ctx, cfg.font_scale);

            let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
            let app = Arc::new(Mutex::new(App::new(cfg.clone(), db)));
            app.lock_recover().status_msg = status_msg;
            let (cmd_tx, worker_handle) =
                worker::spawn(app.clone(), msg_tx.clone(), cc.egui_ctx.clone(), cfg_path.clone());
            // 托盘：左键还原 / 右键菜单「显示 / 退出」
            let lang = cfg.language.clone();
            let tray = tray::Tray::start(
                msg_tx.clone(),
                cc.egui_ctx.clone(),
                tr(&lang, "tray_show"),
                tr(&lang, "tray_quit"),
            );

            // 自动化验证：指令文件监听（仅显式开启；指令经消息泵走 logic()，与 UI 回调同线程）
            if std::env::var_os("PR_DESK_TEST").is_some() {
                let tx = msg_tx.clone();
                let ctx2 = cc.egui_ctx.clone();
                std::thread::spawn(move || {
                    let path = std::env::temp_dir().join("prdesk-cmd.txt");
                    loop {
                        if let Ok(text) = std::fs::read_to_string(&path) {
                            if !text.trim().is_empty() {
                                let _ = std::fs::write(&path, "");
                                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                                    let _ = tx.send(Msg::TestCmd(line.trim().to_string()));
                                }
                                ctx2.request_repaint();
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                });
            }

            // 启动：初始刷新 + 同步
            let _ = cmd_tx.send(Cmd::RefreshGithub);
            let _ = cmd_tx.send(Cmd::Sync);

            Ok(Box::new(EguiApp {
                app,
                cmd_tx,
                msg_rx,
                worker_handle: Some(worker_handle),
                md_cache: egui_commonmark::CommonMarkCache::default(),
                last_poll: Instant::now(),
                cfg_path,
                view_mode: 0,
                profile_names,
                filter_buf: String::new(),
                col_widths: ui_list::default_col_widths(),
                diff_requested: None,
                show_col_grid: false,
                header_drag_at: None,
                last_wants_keyboard: false,
                last_focus_id: String::new(),
                panel: None,
                diff_files: Vec::new(),
                panel_tab: 0,
                panel_width_cur: cfg.panel_width.unwrap_or(420.0).max(320.0) as f32,
                scroll_to: None,
                inbox_loading: false,
                inbox_sel: None,
                nd_open: false,
                nd_title: String::new(),
                nd_kind: String::new(),
                nd_kind_label: String::new(),
                nd_state: String::new(),
                nd_loading: false,
                nd_convo: Vec::new(),
                nd_files: Vec::new(),
                nd_diff: Vec::new(),
                mar_confirm: None,
                settings_open: unconfigured, // 首启自动打开设置
                settings_tab: 0,
                settings_profile: "local".into(),
                s_server: String::new(),
                s_sync_token: String::new(),
                s_ak: String::new(),
                s_gh_token: String::new(),
                s_proxy: String::new(),
                s_repos: String::new(),
                s_poll: 5,
                s_llm_base: String::new(),
                s_llm_key: String::new(),
                s_llm_model: String::new(),
                s_ocr_path: String::new(),
                s_review_conc: 3,
                s_review_prompt,
                s_prompt_loading: false,
                dialog_error: String::new(),
                model_list: Vec::new(),
                model_list_open: false,
                adding_profile: false,
                add_profile_name: String::new(),
                del_confirm: None,
                gh_token_from_env,
                onboarding_dismissed: false,
                allow_close: false,
                tray: Some(tray),
            }))
        }),
    )
    // eframe 的错误类型不保证 Send+Sync，转字符串收敛进 anyhow
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
