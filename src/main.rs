//! PR Desk 桌面端：egui/eframe UI（自 Slint 版全量移植）。
//! 纯逻辑在 lib（pr_desk_rs）与本 crate 的 state.rs；worker.rs 是后台命令循环；
//! 本文件做装配：窗口/字体/主题/托盘/几何记忆/快捷键/消息落地。

// dumpstate 的大 json!{} 字面量会撞默认递归上限
#![recursion_limit = "512"]

mod avatars;
mod kimi_terminal;
mod native_backdrop;
mod state;
mod system_fonts;
mod theme;
mod tray;
mod ui_agent;
mod ui_detail;
mod ui_inbox;
mod ui_list;
mod ui_settings;
mod worker;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use pr_desk_rs::config::Config;
use pr_desk_rs::db::Db;
use pr_desk_rs::i18n::{tr, trf};

use state::*;

const MAX_PARKED_KIMI_TERMINALS: usize = 4;

fn test_artifact_path(name: &str) -> PathBuf {
    let directory = std::env::var_os("PR_DESK_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&directory);
    directory.join(name)
}

struct ParkedKimiTerminal {
    terminal: kimi_terminal::KimiTerminal,
    context: kimi_terminal::PrContext,
    last_used: Instant,
}

/// UI 侧即时状态（App 是业务状态；这里是面板/弹窗/表单等展示态）。
pub struct EguiApp {
    pub app: Arc<Mutex<App>>,
    pub cmd_tx: UnboundedSender<Cmd>,
    pub msg_rx: mpsc::Receiver<Msg>,
    pub worker_handle: Option<std::thread::JoinHandle<()>>,
    pub md_cache: egui_commonmark::CommonMarkCache,
    pub last_poll: Instant,
    pub cfg_path: PathBuf,
    pub backdrop: native_backdrop::NativeBackdrop,
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
    pub review_run_started: Option<Instant>,
    pub agent_confirm: Option<(i64, String)>,
    pub next_kimi_request_id: u64,
    pub kimi_terminal: Option<kimi_terminal::KimiTerminal>,
    pub kimi_terminal_error: String,
    pub kimi_terminal_preparing: bool,
    pub kimi_terminal_fresh: bool,
    pub kimi_context: Option<kimi_terminal::PrContext>,
    pub kimi_key: Option<kimi_terminal::KimiKey>,
    kimi_terminal_cache: HashMap<kimi_terminal::KimiKey, ParkedKimiTerminal>,
    // ── Inbox ──
    pub inbox_loading: bool,
    pub inbox_sel: Option<usize>, // 当前选中通知在 inbox_view 里的下标
    pub nd_open: bool,
    pub nd_tab: usize, // 预览面板 tab：0 会话 / 1 Files（切通知重置为 0，随主面板习惯）
    pub nd_epoch: u64, // 每激活一条通知 +1：滚动区 salt 带它 → 重开回顶部
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
    pub settings_reset_geometry: bool,
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
    pub s_auto_review_on_insert: bool,
    pub s_auto_agent_rereview: bool,
    pub s_agent_review_conc: i64,
    pub s_kimi_cli_path: String,
    pub s_kimi_workspace_root: String,
    pub s_review_prompt: String,
    pub s_prompt_loading: bool,
    pub dialog_error: String,
    pub model_list: Vec<String>,
    pub model_list_open: bool,
    pub adding_profile: bool,
    pub add_profile_name: String,
    pub del_confirm: Option<(String, Instant)>, // 档案删除两步确认（3s 超时还原）
    pub database_busy: bool,
    pub last_database_export: Option<PathBuf>,
    pub import_database_source: Option<PathBuf>,
    pub import_profile_name: String,
    pub gh_token_from_env: bool,
    // ── 引导 ──
    pub onboarding_dismissed: bool,
    // ── URL 导入 ──
    pub import_url_open: bool,
    pub import_url_buf: String,
    // ── 托盘 ──
    #[cfg(windows)]
    pub tray: Option<tray::Tray>,
    pub allow_close: bool,
    /// 键盘导航后请求列表滚动到该行（渲染时消费）
    pub scroll_to: Option<(String, i64)>,
    pub inbox_scroll_to: Option<usize>,
    /// 「复制到档案…」目标选择弹层开关（选择集在点中目标时现取，防弹层期间选择变了）
    pub copy_picker_open: bool,
    /// 「删除…」确认框内容（repo, number, title 快照；仅 local 档案可进入）
    pub delete_confirm: Option<Vec<(String, i64, String)>>,
    /// 首帧几何钳制是否已执行（clamp_geometry_once）
    pub geom_clamped: bool,
}

impl EguiApp {
    pub fn lang(&self) -> String {
        self.app.lock_recover().cfg.language.clone()
    }

    /// 首帧几何钳制：记忆的窗口尺寸是在别的（更大）显示器上存的时，窗口下缘会掉出
    /// 当前屏幕——底部的多选操作条和状态栏整体不可见（用户「下方的功能框没有出现」
    /// 的主因）。builder 的 clamp_size_to_monitor_size 钳的是「最大的显示器」，窗口
    /// 实际开在当前显示器上仍可能超高——这里按当前显示器再钳一次并把位置拉回屏内。
    fn clamp_geometry_once(&mut self, ctx: &egui::Context) {
        if self.geom_clamped {
            return;
        }
        let (monitor, outer, inner) = ctx.input(|i| {
            (
                i.viewport().monitor_size,
                i.viewport().outer_rect,
                i.viewport().inner_rect,
            )
        });
        let (Some(mon), Some(out), Some(inner)) = (monitor, outer, inner) else {
            return;
        }; // 首帧可能还没探测到，下帧再试
        self.geom_clamped = true;
        // 按外框（含标题栏）钳制：内框目标高 = 显示器 94% − 窗框占用
        let chrome = (out.height() - inner.height()).max(0.0);
        let chrome_w = (out.width() - inner.width()).max(0.0);
        let max_inner_w = mon.x * 0.96 - chrome_w;
        let max_inner_h = mon.y * 0.94 - chrome;
        let mut size = egui::vec2(inner.width(), inner.height());
        let mut pos = egui::pos2(out.left(), out.top());
        let mut changed = false;
        if size.x > max_inner_w {
            size.x = max_inner_w;
            changed = true;
        }
        if size.y > max_inner_h {
            size.y = max_inner_h;
            changed = true;
        }
        if pos.x + size.x + chrome_w > mon.x {
            pos.x = (mon.x - size.x - chrome_w).max(0.0);
            changed = true;
        }
        if pos.y + size.y + chrome > mon.y {
            pos.y = (mon.y - size.y - chrome).max(0.0);
            changed = true;
        }
        if pos.x < 0.0 {
            pos.x = 0.0;
            changed = true;
        }
        if pos.y < 0.0 {
            pos.y = 0.0;
            changed = true;
        }
        if changed {
            tracing::info!("窗口几何钳制到当前显示器: {size:?} @{pos:?}（原外框 {out:?}）");
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        }
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
            let _ = self.cmd_tx.send(Cmd::OpenDrawer {
                repo: key.0,
                number: key.1,
                refetch: true,
            });
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
        if self
            .header_drag_at
            .is_some_and(|t| t.elapsed().as_millis() < 300)
        {
            return; // 拖拽调宽刚松开：吞掉这次点击（不当成排序）
        }
        let mut a = self.app.lock_recover();
        if a.sort_col == col {
            a.sort_asc = !a.sort_asc;
        } else {
            a.sort_col = col.to_string();
            a.sort_asc = matches!(
                col,
                "num" | "title" | "author" | "turn" | "review" | "ci" | "size"
            );
        }
        recompute_flat(&mut a);
    }

    /// 详情面板 tab 切换 → 惰性拉取对应数据（会话/Checks/Files；各自 120s 节流）。
    /// Files（合并 diff 视图）tab：diff 缓存缺失时补发 FetchDiff（每 PR 一次去重）。
    pub fn panel_tab_changed(&mut self, t: usize, ctx: &egui::Context) {
        if t == 4 {
            self.ensure_kimi_terminal(ctx);
            return;
        }
        let parts = match t {
            1 => PART_CONVO,
            2 => PART_CHECKS,
            3 => PART_FILES,
            _ => return, // 评审用本地数据
        };
        let sel = self.app.lock_recover().selected.clone();
        if let Some((repo, number)) = sel {
            let _ = self.cmd_tx.send(Cmd::FetchPrLive {
                repo: repo.clone(),
                number,
                parts,
            });
            if t == 3
                && self.diff_files.is_empty()
                && self.diff_requested != Some((repo.clone(), number))
            {
                self.diff_requested = Some((repo.clone(), number));
                let _ = self.cmd_tx.send(Cmd::FetchDiff { repo, number });
            }
        }
    }

    fn next_kimi_request(&mut self) -> u64 {
        self.next_kimi_request_id = self.next_kimi_request_id.wrapping_add(1).max(1);
        self.next_kimi_request_id
    }

    fn park_kimi_terminal(&mut self) {
        let Some(key) = self.kimi_key.clone() else {
            return;
        };
        let Some(terminal) = self.kimi_terminal.take() else {
            return;
        };
        let Some(context) = self.kimi_context.clone() else {
            self.kimi_terminal = Some(terminal);
            return;
        };
        self.kimi_terminal_cache.insert(
            key,
            ParkedKimiTerminal {
                terminal,
                context,
                last_used: Instant::now(),
            },
        );
        while self.kimi_terminal_cache.len() > MAX_PARKED_KIMI_TERMINALS {
            let oldest = self
                .kimi_terminal_cache
                .iter()
                .min_by_key(|(_, parked)| parked.last_used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                break;
            };
            self.kimi_terminal_cache.remove(&oldest);
        }
    }

    fn current_kimi_key(&mut self) -> Option<kimi_terminal::KimiKey> {
        let profile = self.app.lock_recover().cfg.active_profile.clone();
        let panel = self.panel.as_ref()?;
        if let Some(key) = self.kimi_key.as_ref().filter(|key| {
            key.profile == profile && key.repo == panel.repo && key.number == panel.number
        }) {
            return Some(key.clone());
        }
        if let Some(key) = self.kimi_terminal_cache.keys().find(|key| {
            key.profile == profile && key.repo == panel.repo && key.number == panel.number
        }) {
            let key = key.clone();
            self.kimi_key = Some(key.clone());
            return Some(key);
        }
        let key = kimi_terminal::KimiKey {
            profile,
            repo: panel.repo.clone(),
            number: panel.number,
            request_id: self.next_kimi_request(),
        };
        self.kimi_key = Some(key.clone());
        Some(key)
    }

    pub fn ensure_kimi_terminal(&mut self, ctx: &egui::Context) {
        let Some(key) = self.current_kimi_key() else {
            self.kimi_terminal = None;
            return;
        };
        if self
            .kimi_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.key() == &key)
            || self.kimi_terminal_preparing
            || !self.kimi_terminal_error.is_empty()
        {
            return;
        }
        if let Some(parked) = self.kimi_terminal_cache.remove(&key) {
            self.kimi_context = Some(parked.context);
            if !parked.terminal.is_closed() {
                self.kimi_terminal = Some(parked.terminal);
                return;
            }
        }
        self.kimi_terminal = None;
        if let Some(context) = self.kimi_context.clone() {
            let cfg = self.app.lock_recover().cfg.clone();
            let fresh = std::mem::take(&mut self.kimi_terminal_fresh);
            match kimi_terminal::KimiTerminal::spawn(key, &cfg, &context, ctx, fresh) {
                Ok(terminal) => self.kimi_terminal = Some(terminal),
                Err(error) => self.kimi_terminal_error = error.to_string(),
            }
        } else {
            self.kimi_terminal_preparing = true;
            let _ = self.cmd_tx.send(Cmd::PrepareKimiContext { key });
        }
    }

    pub fn restart_kimi_terminal(&mut self, ctx: &egui::Context) {
        self.kimi_terminal = None;
        self.kimi_terminal_error.clear();
        self.kimi_terminal_fresh = false;
        self.ensure_kimi_terminal(ctx);
    }

    pub fn new_kimi_terminal_session(&mut self, ctx: &egui::Context) {
        self.kimi_terminal = None;
        self.kimi_terminal_error.clear();
        self.kimi_terminal_fresh = true;
        self.ensure_kimi_terminal(ctx);
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
        let Some(number) = notif_url_number(&url) else {
            return;
        };
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
        // 立即开面板并进加载态（数据由 worker 回送）；tab/滚动随通知身份重置（主面板同款习惯）
        self.nd_tab = 0;
        self.nd_epoch += 1;
        self.nd_title = title;
        self.nd_kind_label = notif_kind_label(&lang, &kind);
        self.nd_kind = kind.clone();
        self.nd_state = state;
        self.nd_loading = true;
        self.nd_open = true;
        let _ = self
            .cmd_tx
            .send(Cmd::FetchNotifDetail { repo, number, kind });
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
                    worker::send_msg(Msg::Status(trf(
                        &lang,
                        "open_browser_failed",
                        &[&e.to_string()],
                    )));
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
                let _ = self.cmd_tx.send(Cmd::AddNotifReview {
                    repo,
                    number,
                    title,
                    url,
                });
            }
        }
    }

    pub fn open_import_url(&mut self) {
        self.import_url_buf.clear();
        self.import_url_open = true;
    }

    /// URL 弹窗与剪贴板快捷键共用的导入入口。
    pub fn import_pr_url(&mut self, input: &str) -> bool {
        let lang = self.lang();
        let Some((repo, number)) = pr_desk_rs::github::parse_pr_url(input) else {
            self.set_transient_status(tr(&lang, "import_invalid_url").into());
            return false;
        };
        let url = format!("https://github.com/{repo}/pull/{number}");
        let _ = self.cmd_tx.send(Cmd::AddNotifReview {
            repo,
            number,
            title: String::new(),
            url,
        });
        self.import_url_buf.clear();
        self.import_url_open = false;
        self.view_mode = 0;
        true
    }

    pub fn import_pr_from_clipboard(&mut self) {
        let lang = self.lang();
        match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(text) if !text.trim().is_empty() => {
                self.import_pr_url(&text);
            }
            Ok(_) => self.set_transient_status(tr(&lang, "import_clipboard_empty").into()),
            Err(e) => {
                self.set_transient_status(trf(&lang, "import_clipboard_failed", &[&e.to_string()]));
            }
        }
    }

    // ── 跨档案复制 / 本地删除 ──

    /// 右键/操作条「复制到档案…」：打开目标档案选择弹层（选择集在点中目标时现取，
    /// 防弹层开着期间用户改了选择）。
    pub fn open_copy_picker(&mut self) {
        self.copy_picker_open = true;
    }

    /// 弹层点中目标档案：当前选择集（多选优先，否则当前行）交给 worker 复制。
    pub fn copy_to_profile(&mut self, target: &str) {
        self.copy_picker_open = false;
        let keys = {
            let a = self.app.lock_recover();
            effective_targets(&a)
        };
        if keys.is_empty() {
            return;
        }
        let _ = self.cmd_tx.send(Cmd::CopyToProfile {
            target: target.to_string(),
            keys,
        });
    }

    /// 右键/操作条「删除…」（仅 local 档案）：填充确认框（数量 + 前几个标题快照）。
    pub fn delete_selected(&mut self) {
        let items = {
            let a = self.app.lock_recover();
            if a.cfg.active_profile != "local" {
                return;
            }
            effective_targets(&a)
                .into_iter()
                .map(|(repo, number)| {
                    let title = find_row(&a, &repo, number)
                        .and_then(|i| a.rows[i]["title"].as_str().map(|s| s.to_string()))
                        .unwrap_or_default();
                    (repo, number, title)
                })
                .collect::<Vec<_>>()
        };
        if !items.is_empty() {
            self.delete_confirm = Some(items);
        }
    }

    /// 确认框「删除」：事务删 PR 行 + 评审 + diff 缓存并记墓碑；关被删 PR 的详情面板；
    /// 清选择集里的被删身份；刷新列表。SQLite 本地操作微秒级，同步做（同 local_update 先例）。
    pub fn confirm_delete(&mut self) {
        let Some(items) = self.delete_confirm.take() else {
            return;
        };
        let keys: Vec<(String, i64)> = items.iter().map(|(r, n, _)| (r.clone(), *n)).collect();
        if let Some(pd) = &self.panel {
            if keys.iter().any(|(r, n)| *r == pd.repo && *n == pd.number) {
                self.panel = None;
            }
        }
        let (r, lang) = {
            let a = self.app.lock_recover();
            (a.db.delete_prs(&keys), a.cfg.language.clone())
        };
        let n = match r {
            Ok(n) => n,
            Err(e) => {
                worker::send_msg(Msg::Status(trf(&lang, "delete_failed", &[&e.to_string()])));
                return;
            }
        };
        {
            let mut a = self.app.lock_recover();
            a.selection.retain(|k| !keys.contains(k));
            if a.selected.as_ref().is_some_and(|s| keys.contains(s)) {
                a.selected = None;
            }
        }
        worker::send_msg(Msg::Status(trf(&lang, "delete_done", &[&n.to_string()])));
        worker::reload_rows_global(&self.app);
    }

    /// 右键菜单「清空删除记录」（仅 local 且有墓碑时显示）：清掉墓碑，
    /// 被删 PR 之后刷新/手动加入可正常回库。
    pub fn clear_tombstones(&mut self) {
        let (r, lang) = {
            let a = self.app.lock_recover();
            (a.db.clear_tombstones(), a.cfg.language.clone())
        };
        match r {
            Ok(n) => worker::send_msg(Msg::Status(trf(
                &lang,
                "tombstones_cleared",
                &[&n.to_string()],
            ))),
            Err(e) => worker::send_msg(Msg::Status(trf(&lang, "delete_failed", &[&e.to_string()]))),
        }
    }

    /// 主题切换：即时持久化到 config.toml。
    pub fn theme_changed(&self, ctx: &egui::Context, name: &str) {
        let theme = theme::normalize(name).to_string();
        let mut a = self.app.lock_recover();
        if a.cfg.theme == theme {
            return;
        }
        a.cfg.theme = theme.clone();
        if let Err(e) = a.cfg.save(&self.cfg_path) {
            tracing::warn!("主题写入 config.toml 失败: {e}");
        }
        drop(a);
        apply_theme(ctx, &theme);
    }

    pub fn theme_toggled(&self, ctx: &egui::Context) {
        let current = self.app.lock_recover().cfg.theme.clone();
        self.theme_changed(ctx, theme::next(&current));
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
        let snapped = if s < 0.95 {
            0.9
        } else if s > 1.07 {
            1.15
        } else {
            1.0
        };
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
            i.viewport()
                .outer_rect
                .map(|r| (r.width() as f64, r.height() as f64))
                .unwrap_or((0.0, 0.0))
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

    /// 导出当前档案数据库；文件选择由设置页或测试指令完成。
    pub fn export_database_to(&mut self, destination: PathBuf) {
        if self.database_busy {
            return;
        }
        self.dialog_error.clear();
        self.database_busy = true;
        if let Err(e) = self.cmd_tx.send(Cmd::ExportDatabase { destination }) {
            self.database_busy = false;
            self.dialog_error = trf(&self.lang(), "db_export_failed", &[&e.to_string()]);
        }
    }

    /// 导入数据库为新档案；UI 先校验，worker 落盘前会再次复验。
    pub fn import_database_from(&mut self, source: PathBuf, name: String) {
        if self.database_busy {
            return;
        }
        self.dialog_error.clear();
        let lang = self.lang();
        let name = name.trim().to_string();
        if !pr_desk_rs::config::valid_profile_name(&name) {
            self.dialog_error = tr(&lang, "profile_invalid_name").into();
            return;
        }
        if pr_desk_rs::config::is_reserved_profile_name(&name) {
            self.dialog_error = trf(&lang, "profile_reserved", &[&name]);
            return;
        }
        if self.app.lock_recover().cfg.has_profile(&name) {
            self.dialog_error = trf(&lang, "profile_exists", &[&name]);
            return;
        }
        self.import_profile_name = name.clone();
        self.database_busy = true;
        if let Err(e) = self.cmd_tx.send(Cmd::ImportDatabase { source, name }) {
            self.database_busy = false;
            self.dialog_error = trf(&lang, "db_import_failed", &[&e.to_string()]);
        }
    }

    fn apply_msg(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::Status(s) => self.set_transient_status(s),
            Msg::RefreshDone(open_count) => {
                // 刷新完成：停顶栏转圈（去重标志已在 worker 复位）+ 状态栏结果（走 6s 自动清）
                let lang = self.lang();
                self.set_transient_status(trf(
                    &lang,
                    "gh_refresh_done",
                    &[&open_count.to_string()],
                ));
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
            Msg::DatabaseExportFinished(result) => {
                self.database_busy = false;
                let lang = self.lang();
                match result {
                    Ok(path) => {
                        self.dialog_error.clear();
                        let display = path.to_string_lossy().into_owned();
                        self.last_database_export = Some(path);
                        self.set_transient_status(trf(&lang, "db_export_success", &[&display]));
                    }
                    Err(e) => {
                        let msg = trf(&lang, "db_export_failed", &[&e]);
                        self.dialog_error = msg.clone();
                        self.set_transient_status(msg);
                    }
                }
            }
            Msg::DatabaseImportFinished(result) => {
                self.database_busy = false;
                let lang = self.lang();
                match result {
                    Ok(name) => {
                        self.dialog_error.clear();
                        self.settings_profile = name.clone();
                        self.import_profile_name = name.clone();
                        self.import_database_source = None;
                        self.set_transient_status(trf(&lang, "db_import_success", &[&name]));
                    }
                    Err(e) => {
                        let msg = trf(&lang, "db_import_failed", &[&e]);
                        self.dialog_error = msg.clone();
                        self.set_transient_status(msg);
                    }
                }
            }
            Msg::Profiles { names, active: _ } => {
                self.profile_names = names;
                self.agent_confirm = None;
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
                    let _ = self.cmd_tx.send(Cmd::OpenDrawer {
                        repo,
                        number,
                        refetch: false,
                    });
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
                let reset = self
                    .panel
                    .as_ref()
                    .is_none_or(|old| old.repo != pd.repo || old.number != pd.number);
                self.panel = Some(*pd);
                if reset {
                    self.park_kimi_terminal();
                    self.review_run_started = None;
                    self.kimi_terminal_error.clear();
                    self.kimi_terminal_preparing = false;
                    self.kimi_terminal_fresh = false;
                    self.kimi_context = None;
                    self.kimi_key = None;
                }
            }
            Msg::ClosePanel => {
                self.panel = None;
                self.review_run_started = None;
            }
            Msg::ReviewProgress { repo, number, text } => {
                {
                    // 直写 status_msg 也要推序号，否则在途的 6s 自动清除会误清这条
                    let mut a = self.app.lock_recover();
                    a.status_msg = text.clone();
                    a.status_gen = a.status_gen.wrapping_add(1);
                    a.status_msg_at = Some(Instant::now());
                }
                if panel_matches(&self.app, &repo, number) {
                    if self.review_run_started.is_none() {
                        self.review_run_started = Some(Instant::now());
                    }
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
                    self.review_run_started = None;
                    let rv = {
                        self.app
                            .lock_recover()
                            .db
                            .get_review(&repo, number)
                            .ok()
                            .flatten()
                    };
                    if let Some(p) = self.panel.as_mut() {
                        p.review = review_props(rv.as_ref(), &lang);
                        p.review_status = "done".into();
                        p.review_progress.clear();
                    }
                }
            }
            Msg::ReviewFailed { repo, number, err } => {
                let lang = self.lang();
                self.set_transient_status(trf(
                    &lang,
                    "review_failed",
                    &[&repo, &number.to_string(), &err],
                ));
                if panel_matches(&self.app, &repo, number) {
                    self.review_run_started = None;
                    if let Some(p) = self.panel.as_mut() {
                        p.review_status = "failed".into();
                    }
                }
            }
            Msg::AgentReviewUpdated {
                profile,
                repo,
                number,
            } => {
                if self.app.lock_recover().cfg.active_profile == profile {
                    if panel_matches(&self.app, &repo, number) {
                        let _ = self.cmd_tx.send(Cmd::OpenDrawer {
                            repo,
                            number,
                            refetch: false,
                        });
                    }
                    let _ = self.cmd_tx.send(Cmd::LoadNeedsYou);
                }
            }
            Msg::NeedsYouLoaded(rows) => {
                let mut a = self.app.lock_recover();
                a.needs_you = rows;
                a.needs_you_loading = false;
            }
            Msg::KimiContextReady { key, context } => {
                if self.kimi_key == Some(key.clone()) && self.kimi_terminal_preparing {
                    self.kimi_terminal_preparing = false;
                    self.kimi_context = Some(*context);
                    let workspace = self.kimi_context.clone().unwrap();
                    let cfg = self.app.lock_recover().cfg.clone();
                    let fresh = std::mem::take(&mut self.kimi_terminal_fresh);
                    match kimi_terminal::KimiTerminal::spawn(key, &cfg, &workspace, ctx, fresh) {
                        Ok(terminal) => self.kimi_terminal = Some(terminal),
                        Err(error) => self.kimi_terminal_error = error.to_string(),
                    }
                }
            }
            Msg::KimiContextFailed { key, error } => {
                if self.kimi_key == Some(key.clone()) && self.kimi_terminal_preparing {
                    self.kimi_terminal_preparing = false;
                    self.kimi_terminal_error = error;
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
            Msg::NotifDetail {
                repo,
                number,
                convo,
                files_json,
                diff_files,
            } => {
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
            Msg::PanelLive {
                repo,
                number,
                ci,
                state: st,
                updated,
                head_sha,
                convo,
                checks_json,
                files_json,
            } => {
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
                        if let Some(head_sha) = head_sha {
                            if !head_sha.is_empty() && p.head_sha != head_sha {
                                p.head_sha = head_sha;
                                p.agent_review.stale = !p.agent_review.reviewed_head_sha.is_empty()
                                    && p.agent_review.reviewed_head_sha != p.head_sha;
                                self.diff_files.clear();
                                self.diff_requested = None;
                                let _ = self.cmd_tx.send(Cmd::FetchDiff {
                                    repo: repo.clone(),
                                    number,
                                });
                            }
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
            Msg::DiffParsed {
                repo,
                number,
                head_sha,
                files,
            } => {
                if panel_matches(&self.app, &repo, number)
                    && self.panel.as_ref().is_some_and(|panel| {
                        panel.head_sha.is_empty() || panel.head_sha == head_sha
                    })
                {
                    self.diff_files = files;
                }
            }
            Msg::ConvoAvatars { .. } => {
                // egui 的纹理缓存按文件 mtime 自动失效（avatars.rs load_cached），
                // 重绘已由通道触发——无需重装任何数据，天然「原地更新不闪烁」。
            }
            Msg::TrayShow => {
                tracing::info!("托盘：还原窗口");
                self.backdrop.restore();
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

    /// ── 自动化验证指令（PR_DESK_TEST=1 时启用；PR_DESK_TEST_DIR 或 %TEMP% 下的指令文件）──
    /// 每条指令调用的就是对应 UI 控件背后的处理器（同函数同路径）。
    /// `dumpstate` 把关键状态写成 JSON，供同目录下的验证脚本断言。
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
            "importopen" => self.open_import_url(),
            "importurl" => {
                self.import_pr_url(arg);
            }
            "importclip" => self.import_pr_from_clipboard(),
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
            "theme" => {
                if arg.is_empty() {
                    self.theme_toggled(ctx);
                } else {
                    self.theme_changed(ctx, arg);
                }
            }
            // row <n> [ctrl|shift]：点可见行（view_visible 序）
            // 按身份直接打开详情（测试指令；等价于点行 + Enter）
            "openpr" => {
                let number: i64 = it.next().unwrap_or("0").parse().unwrap_or(0);
                {
                    self.app.lock_recover().selected = Some((arg.to_string(), number));
                }
                let _ = self.cmd_tx.send(Cmd::OpenDrawer {
                    repo: arg.to_string(),
                    number,
                    refetch: true,
                });
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
                    let _ = self.cmd_tx.send(Cmd::OpenDrawer {
                        repo,
                        number,
                        refetch: true,
                    });
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
                // key up/down/pageup/pagedown/o/p/1/2/3：与 hotkeys 同一处理器语义
                match arg {
                    "up" | "down" | "pageup" | "pagedown" => {
                        let delta = match arg {
                            "down" => 1,
                            "up" => -1,
                            "pagedown" => 20,
                            _ => -20,
                        };
                        self.move_selection(delta);
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
                self.panel_tab_changed(t, ctx);
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
            "agentreview" | "rereview" => {
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    if cmd == "agentreview" {
                        let _ = self.cmd_tx.send(Cmd::StartAgentReview { repo, number });
                    } else {
                        let _ = self.cmd_tx.send(Cmd::RereviewAgent { repo, number });
                    }
                }
            }
            "needsyou" => {
                self.view_mode = 2;
                self.app.lock_recover().needs_you_loading = true;
                let _ = self.cmd_tx.send(Cmd::LoadNeedsYou);
            }
            "ackagent" => {
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    let _ = self
                        .cmd_tx
                        .send(Cmd::AcknowledgeAgentReview { repo, number });
                }
            }
            "answeragent" => {
                let decision_id = arg.parse().unwrap_or(0);
                let answer = it.collect::<Vec<_>>().join(" ");
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    let _ = self.cmd_tx.send(Cmd::AnswerAgentDecision {
                        repo,
                        number,
                        decision_id,
                        execute: true,
                        answer,
                    });
                }
            }
            "copyreview" => {
                let _ = self.cmd_tx.send(Cmd::CopyReview);
            }
            "kimiinput" => {
                let text = std::iter::once(arg).chain(it).collect::<Vec<_>>().join(" ");
                if let Some(terminal) = self.kimi_terminal.as_mut()
                    && let Err(error) = terminal.send_bytes(text.as_bytes())
                {
                    self.kimi_terminal_error = error.to_string();
                }
            }
            "kimienter" => {
                if let Some(terminal) = self.kimi_terminal.as_mut()
                    && let Err(error) = terminal.send_bytes(b"\r")
                {
                    self.kimi_terminal_error = error.to_string();
                }
            }
            "kimikey" => {
                let bytes: &[u8] = match arg {
                    "esc" => b"\x1b",
                    "tab" => b"\t",
                    "ctrlc" => b"\x03",
                    "up" => b"\x1b[A",
                    "down" => b"\x1b[B",
                    _ => return,
                };
                if let Some(terminal) = self.kimi_terminal.as_mut()
                    && let Err(error) = terminal.send_bytes(bytes)
                {
                    self.kimi_terminal_error = error.to_string();
                }
            }
            "kimirestart" => self.restart_kimi_terminal(ctx),
            "kiminewsession" => self.new_kimi_terminal_session(ctx),
            // 批量（作用于多选集合，无多选则当前选中）
            "batch" => match arg {
                "pin" => worker::batch_toggle(&self.app, "pinned"),
                "board" => worker::batch_toggle(&self.app, "on_board"),
                "skip" => {
                    let keys = {
                        let a = self.app.lock_recover();
                        effective_targets(&a)
                    };
                    worker::batch_local_update(&self.app, &keys, "status", "rejected");
                }
                "restore" => {
                    let keys = {
                        let a = self.app.lock_recover();
                        effective_targets(&a)
                    };
                    worker::batch_local_update(&self.app, &keys, "status", "pending");
                }
                "links" => {
                    if let Some(text) = worker::copy_links_text(&self.app) {
                        let _ = self.cmd_tx.send(Cmd::CopyLinks(text));
                    }
                }
                "review" => {
                    let keys = {
                        let a = self.app.lock_recover();
                        effective_targets(&a)
                    };
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
            "exportdb" => {
                let path = std::iter::once(arg).chain(it).collect::<Vec<_>>().join(" ");
                self.export_database_to(PathBuf::from(path));
            }
            "importdb" => {
                let name = it.next().unwrap_or("").to_string();
                self.import_database_from(PathBuf::from(arg), name);
            }
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
            // 新建档案（与设置页「＋」同一处理函数）
            "addprofile" => {
                self.add_profile_name = arg.to_string();
                crate::ui_settings::profile_add(self);
            }
            // 跨档案复制 / 本地删除（与弹层按钮同一处理函数）
            "copypicker" => self.open_copy_picker(),
            "copyto" => self.copy_to_profile(arg),
            "delpr" => self.delete_selected(),
            "confirmdel" => self.confirm_delete(),
            "cleartomb" => self.clear_tombstones(),
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
            // 拖拽调宽（与把手 Sense::drag 同语义：三分支调宽 + 记拖拽时刻抑制点击排序）
            "coldrag" => {
                let i: usize = arg.parse().unwrap_or(0);
                let dx: f32 = it.next().unwrap_or("0").parse().unwrap_or(0.0);
                let n = ui_list::COL_SPECS.len();
                if let Some(spec) = ui_list::COL_SPECS.get(i) {
                    let dragged = if spec.flex {
                        self.col_widths[i + 1] =
                            (self.col_widths[i + 1] - dx).max(ui_list::COL_SPECS[i + 1].min_w);
                        true
                    } else if ui_list::COL_SPECS.get(i + 1).is_some_and(|s| s.flex) {
                        self.col_widths[i] = (self.col_widths[i] + dx).max(spec.min_w);
                        true
                    } else if i + 1 < n {
                        ui_list::apply_pair_drag(&mut self.col_widths, i, dx);
                        true
                    } else {
                        false
                    };
                    if dragged {
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
                self.backdrop.restore();
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
            "dumpstate" => self.dump_state(ctx),
            _ => tracing::warn!("未知测试指令: {line}"),
        }
        tracing::info!("test-cmd 已处理: {line}");
    }

    /// F12 / `shot` 指令共用：置位挂起，ui() 里发 ViewportCommand::Screenshot
    ///（窗口隐藏时 GL surface 尺寸为 0，直接发会让 egui_glow 回读 panic）。
    pub fn request_shot(&self, ctx: &egui::Context) {
        self.backdrop.restore();
        if ctx.input(|i| {
            i.viewport()
                .inner_rect
                .map(|r| r.width() < 2.0 || r.height() < 2.0)
                .unwrap_or(true)
        }) {
            tracing::warn!("窗口隐藏/零尺寸，跳过截图");
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
            egui::ScreenshotCallback::new(|image| {
                let path = test_artifact_path("prdesk-shot.png");
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
            }),
        ));
    }

    /// 状态快照（JSON 落盘，供验证脚本断言）。
    fn dump_state(&self, ctx: &egui::Context) {
        let visuals = ctx.global_style().visuals.clone();
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
            "panel_review_progress": self.panel.as_ref().map(|p| p.review_progress.clone()),
            "agent_review_status": self.panel.as_ref().map(|p| p.agent_review.status.clone()),
            "agent_reviewed_head": self.panel.as_ref().map(|p| p.agent_review.reviewed_head_sha.clone()),
            "agent_requested_head": self.panel.as_ref().map(|p| p.agent_review.requested_head_sha.clone()),
            "agent_session_id": self.panel.as_ref().map(|p| p.agent_review.session_id.clone()),
            "agent_findings": self.panel.as_ref().map(|p| p.agent_review.findings.len()),
            "agent_decisions": self.panel.as_ref().map(|p| p.agent_review.decisions.len()),
            "needs_you_count": a.needs_you.len(),
            "review_elapsed_secs": self.review_run_started.map(|started| started.elapsed().as_secs()),
            "kimi_terminal_active": self.kimi_terminal.is_some(),
            "kimi_terminal_preparing": self.kimi_terminal_preparing,
            "kimi_terminal_closed": self.kimi_terminal.as_ref().map(|terminal| terminal.is_closed()),
            "kimi_terminal_dimensions": self.kimi_terminal.as_ref().and_then(|terminal| terminal.dimensions()),
            "kimi_terminal_error": self.kimi_terminal_error,
            "kimi_cached_sessions": self.kimi_terminal_cache.len(),
            "kimi_cache_limit": MAX_PARKED_KIMI_TERMINALS,
            "kimi_cached_live": self.kimi_terminal_cache.values().filter(|parked| !parked.terminal.is_closed()).count(),
            "kimi_request_id": self.kimi_key.as_ref().map(|key| key.request_id),
            "kimi_workspace": self.kimi_context.as_ref().map(|context| pr_desk_rs::ocr::context_dir_for(&context.repo, context.number, &context.base_sha, &context.head_sha, &a.cfg.kimi_workspace_root_path()).join("worktree").to_string_lossy().into_owned()),
            "panel_convo": self.panel.as_ref().map(|p| p.convo.len()),
            "panel_checks": self.panel.as_ref().map(|p| p.checks_json.len()),
            "diff_files": self.diff_files.len(),
            "status_msg": a.status_msg,
            "gh_refreshing": a.gh_refreshing,
            "sync_inflight": a.sync_inflight,
            "database_busy": self.database_busy,
            "last_database_export": self.last_database_export.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "inbox": a.inbox.len(),
            "inbox_view": a.inbox_view.len(),
            "inbox_has_more": a.inbox_has_more,
            "inbox_page": a.inbox_page,
            "inbox_loading": self.inbox_loading,
            "nd_open": self.nd_open,
            "nd_tab": self.nd_tab,
            "nd_loading": self.nd_loading,
            "nd_convo": self.nd_convo.len(),
            "settings_open": self.settings_open,
            "import_url_open": self.import_url_open,
            "import_url_buf": self.import_url_buf,
            "dialog_error": self.dialog_error,
            "profile_names": self.profile_names,
            "active_profile": a.cfg.active_profile,
            "copy_picker_open": self.copy_picker_open,
            "delete_confirm_n": self.delete_confirm.as_ref().map(|v| v.len()),
            "tombstones": a.db.tombstone_count().unwrap_or(-1),
            "theme": a.cfg.theme,
            "visual_dark_mode": visuals.dark_mode,
            "panel_alpha": visuals.panel_fill.a(),
            "window_alpha": visuals.window_fill.a(),
            "backdrop_mode": format!("{:?}", self.backdrop.mode()),
            "backdrop_backend": self.backdrop.backend_name(),
            "backdrop_diagnostic": self.backdrop.diagnostic(),
            "clear_alpha": self.backdrop.clear_alpha(),
            "language": a.cfg.language,
            "total": a.total,
        });
        let path = test_artifact_path("prdesk-state.json");
        if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()) {
            tracing::warn!("状态快照写入失败: {e}");
        }
    }

    fn move_selection(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
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
                        .and_then(|s| a.view_visible.iter().position(|key| key == s));
                    let npos = match pos {
                        Some(p) => (p as i64 + delta).clamp(0, n as i64 - 1) as usize,
                        None if delta > 0 => (delta - 1).clamp(0, n as i64 - 1) as usize,
                        None => (n as i64 + delta).clamp(0, n as i64 - 1) as usize,
                    };
                    a.selected = Some(a.view_visible[npos].clone());
                    a.selected.clone()
                }
            };
            if let Some(key) = sel_key {
                self.scroll_to = Some(key);
            }
        } else {
            let n = self.app.lock_recover().inbox_view.len();
            if n > 0 {
                let npos = match self.inbox_sel {
                    Some(p) => (p as i64 + delta).clamp(0, n as i64 - 1) as usize,
                    None if delta > 0 => (delta - 1).clamp(0, n as i64 - 1) as usize,
                    None => (n as i64 + delta).clamp(0, n as i64 - 1) as usize,
                };
                self.inbox_sel = Some(npos);
                self.inbox_scroll_to = Some(npos);
            }
        }
    }

    /// 快捷键分发（无输入框聚焦时）。Esc 分级：URL 导入/设置弹窗 → 关弹窗；
    /// 引导 → 吞掉；Inbox 预览 → 关预览；多选 → 清多选；详情面板 → 关面板；→ 清选中。
    fn hotkeys(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        // debug：按键事件可见性（排查热键被吞）
        if true {
            let keys = ctx.input(|i| {
                i.keys_down
                    .iter()
                    .map(|k| format!("{k:?}"))
                    .collect::<Vec<_>>()
            });
            let pressed: Vec<String> = ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Key {
                            key, pressed: true, ..
                        } => Some(format!("{key:?}")),
                        _ => None,
                    })
                    .collect()
            });
            if !pressed.is_empty() {
                tracing::debug!(
                    "hotkeys: keys_down={keys:?} pressed={pressed:?} wants_kb={}",
                    ctx.egui_wants_keyboard_input()
                );
            }
        }
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if self.import_url_open {
            if esc {
                self.import_url_open = false;
            }
            return;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let settings_or_onboarding = self.settings_open
            || (self.app.lock_recover().cfg.is_unconfigured() && !self.onboarding_dismissed);
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
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::V)) {
            self.import_pr_from_clipboard();
            return;
        }
        let (down, up, page_down, page_up) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::PageDown),
                i.key_pressed(egui::Key::PageUp),
            )
        });
        if down || up || page_down || page_up {
            let row_height = if self.view_mode == 0 { 24.0 } else { 30.0 };
            let viewport_height =
                ctx.input(|i| i.viewport().inner_rect.map(|r| r.height()).unwrap_or(600.0));
            let page_rows = ((viewport_height - 140.0) / row_height).floor().max(1.0) as i64;
            let delta = if down {
                1
            } else if up {
                -1
            } else if page_down {
                page_rows
            } else {
                -page_rows
            };
            self.move_selection(delta);
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            if self.view_mode == 0 {
                let sel = self.app.lock_recover().selected.clone();
                if let Some((repo, number)) = sel {
                    let _ = self.cmd_tx.send(Cmd::OpenDrawer {
                        repo,
                        number,
                        refetch: true,
                    });
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
        for (key, p) in [
            (egui::Key::Num1, 1i64),
            (egui::Key::Num2, 2),
            (egui::Key::Num3, 3),
        ] {
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
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let (current_theme, visible) = {
            let theme = self.app.lock_recover().cfg.theme.clone();
            let visible = ctx.input(|input| input.viewport().visible().unwrap_or(true));
            (theme, visible)
        };
        self.backdrop.sync(frame, &current_theme, visible);
        self.clamp_geometry_once(ctx);
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
        self.last_focus_id = ctx
            .memory(|m| m.focused())
            .map(|id| format!("{id:?}"))
            .unwrap_or_default();
        self.hotkeys(ctx);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.backdrop.mode() == native_backdrop::BackdropMode::Native {
            return [0.0, 0.0, 0.0, 0.0];
        }
        let fill = visuals.panel_fill;
        egui::Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 255)
            .to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let current_theme = self.app.lock_recover().cfg.theme.clone();
        if self.backdrop.mode() == native_backdrop::BackdropMode::Gradient {
            theme::paint_background(&ctx, &current_theme);
        }

        // ── 布局：顶栏 / 状态栏 /（选中条）/ 中央视图 ──
        ui_list::top_bar(self, ui);
        ui_list::status_bar(self, ui);
        ui_list::selection_bar(self, ui);
        match self.view_mode {
            0 => {
                ui_detail::detail_panel(self, ui);
                ui_list::pr_list_view(self, ui);
            }
            1 => {
                ui_inbox::notif_preview(self, ui);
                ui_inbox::inbox_view(self, ui);
            }
            _ => {
                ui_agent::needs_you_view(self, ui);
            }
        }

        // ── 弹窗（模态，顶层）──
        ui_settings::settings_window(self, &ctx);
        ui_settings::onboarding_window(self, &ctx);
        ui_list::import_url_window(self, &ctx);
        ui_list::copy_picker_window(self, &ctx);
        ui_list::delete_confirm_window(self, &ctx);
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
    theme::apply(ctx, theme);
}

/// 字号档位：从 egui 默认基准重算（不累乘，避免多次切换漂移）。
pub fn apply_font_scale(ctx: &egui::Context, scale: f32) {
    use std::collections::BTreeMap;
    let base: BTreeMap<egui::TextStyle, egui::FontId> = egui::Style::default().text_styles;
    ctx.all_styles_mut(|style| {
        style.text_styles = base
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    egui::FontId {
                        size: v.size * scale,
                        ..v.clone()
                    },
                )
            })
            .collect();
    });
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
    let cfg_path = if cwd_cfg.exists() && cwd_cfg != exe_cfg {
        cwd_cfg
    } else {
        exe_cfg
    };
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
    let cfg = Config {
        active_profile: active_name,
        ..cfg
    };

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
        egui::IconData {
            rgba: img.into_raw(),
            width: iw,
            height: ih,
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_transparent(true)
            .with_inner_size([w, h])
            // 记忆尺寸若是在更大显示器上存的，钳到当前显示器——否则窗口下缘掉出屏幕，
            // 底部多选操作条/状态栏整排不可见（用户「下方的功能框没有出现」的主因）
            .with_clamp_size_to_monitor_size(true)
            .with_icon(icon),
        ..Default::default()
    };

    let unconfigured = cfg.is_unconfigured();
    let gh_token_from_env = std::env::var("GITHUB_TOKEN")
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    let profile_names = cfg.profile_names();
    let status_msg = config_status(&cfg);
    let s_review_prompt = String::new(); // 打开设置页时经 LoadReviewPrompt 重读

    eframe::run_native(
        "PR Desk",
        options,
        Box::new(move |cc| {
            system_fonts::install(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, &cfg.theme);
            apply_font_scale(&cc.egui_ctx, cfg.font_scale);
            let backdrop = native_backdrop::NativeBackdrop::new(cc, &cfg.theme);

            let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
            let app = Arc::new(Mutex::new(App::new(cfg.clone(), db)));
            app.lock_recover().status_msg = status_msg;
            let (cmd_tx, worker_handle) = worker::spawn(
                app.clone(),
                msg_tx.clone(),
                cc.egui_ctx.clone(),
                cfg_path.clone(),
            );
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
                    let path = test_artifact_path("prdesk-cmd.txt");
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
                backdrop,
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
                review_run_started: None,
                agent_confirm: None,
                next_kimi_request_id: 0,
                kimi_terminal: None,
                kimi_terminal_error: String::new(),
                kimi_terminal_preparing: false,
                kimi_terminal_fresh: false,
                kimi_context: None,
                kimi_key: None,
                kimi_terminal_cache: HashMap::new(),
                scroll_to: None,
                inbox_loading: false,
                inbox_sel: None,
                inbox_scroll_to: None,
                nd_open: false,
                nd_tab: 0,
                nd_epoch: 0,
                nd_title: String::new(),
                nd_kind: String::new(),
                nd_kind_label: String::new(),
                nd_state: String::new(),
                nd_loading: false,
                nd_convo: Vec::new(),
                nd_files: Vec::new(),
                nd_diff: Vec::new(),
                mar_confirm: None,
                copy_picker_open: false,
                delete_confirm: None,
                geom_clamped: false,
                settings_open: unconfigured, // 首启自动打开设置
                settings_reset_geometry: true,
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
                s_auto_review_on_insert: false,
                s_auto_agent_rereview: false,
                s_agent_review_conc: 2,
                s_kimi_cli_path: "kimi".into(),
                s_kimi_workspace_root: String::new(),
                s_review_prompt,
                s_prompt_loading: false,
                dialog_error: String::new(),
                model_list: Vec::new(),
                model_list_open: false,
                adding_profile: false,
                add_profile_name: String::new(),
                del_confirm: None,
                database_busy: false,
                last_database_export: None,
                import_database_source: None,
                import_profile_name: String::new(),
                gh_token_from_env,
                onboarding_dismissed: false,
                import_url_open: false,
                import_url_buf: String::new(),
                allow_close: false,
                tray: Some(tray),
            }))
        }),
    )
    // eframe 的错误类型不保证 Send+Sync，转字符串收敛进 anyhow
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
