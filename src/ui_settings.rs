//! 设置弹窗 + 首启引导。保存语义逐条对照旧 Slint 版（代理校验拒绝保存、
//! 档案三字段按编辑档案写回、并发数即时生效、模板回填竞态防护）。

use std::time::Instant;

use egui::RichText;

use pr_desk_rs::config::Profile;
use pr_desk_rs::i18n::{tr, trf};

use crate::state::*;
use crate::EguiApp;

/// 打开设置：回填当前配置（LLM 字段可能在别处被改动）+ 重读自定义评审模板。
pub fn open_settings(app: &mut EguiApp) {
    let cfg = app.app.lock_recover().cfg.clone();
    // 首启兜底：default 档未配置且无自定义档案时，编辑目标停在 local 会把
    // 服务器三字段锁死、永远填不了服务器地址 → 自动切到 default 编辑
    if app.settings_profile == "local" && !cfg.has_default_profile() && cfg.profiles.is_empty() {
        app.settings_profile = "default".into();
    }
    // 服务器三字段按「正在编辑的档案」回填（local 档为空且字段锁定）
    let (server, token, ak) = profile_fields(&cfg, &app.settings_profile);
    app.s_server = server;
    app.s_sync_token = token;
    app.s_ak = ak;
    app.s_gh_token = cfg.github_token.clone().unwrap_or_default();
    app.s_proxy = cfg.proxy.clone();
    app.s_repos = cfg.repos.join("\n");
    app.s_poll = cfg.poll_interval_min as i64;
    app.s_llm_base = cfg.llm_base_url.clone();
    app.s_llm_key = cfg.llm_api_key.clone().unwrap_or_default();
    app.s_llm_model = cfg.llm_model.clone();
    app.s_ocr_path = cfg.ocr_path.clone();
    app.s_review_conc = cfg.review_concurrency as i64;
    // 弹窗内联错误行清空（上一次失败的残留不带进本次会话）
    app.dialog_error.clear();
    // 自定义评审模板每次打开重读（可能被外部编辑过）：
    // 读盘挪进 worker（UI 线程不做阻塞 IO），回来经 Msg::ReviewPromptLoaded 回填。
    // 回填竞态防护：先清空 + loading——回填完成前编辑框锁定、保存不回写模板，
    // 防止用户瞬间点保存用旧文本覆盖磁盘新模板
    app.s_review_prompt.clear();
    app.s_prompt_loading = true;
    let _ = app.cmd_tx.send(Cmd::LoadReviewPrompt);
    app.settings_open = true;
}

/// 弹窗内发起的操作失败：除了状态栏还要写弹窗内联错误行（状态栏被遮罩盖住）。
fn dialog_err(app: &mut EguiApp, msg: String) {
    crate::worker::send_msg(Msg::Status(msg.clone()));
    app.dialog_error = msg;
}

/// 保存设置。返回 true = 成功（调用方关弹窗）；false = 失败（错误已落内联行）。
fn save_settings(app: &mut EguiApp) -> bool {
    let repos: Vec<String> = app
        .s_repos
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l.contains('/'))
        .collect();
    let poll = app.s_poll.max(1) as u64;
    let review_concurrency = app.s_review_conc.clamp(1, 16) as u64;
    let ocr_path = app.s_ocr_path.trim();
    let proxy = app.s_proxy.trim().to_string();
    let lang = app.lang();
    // 代理地址保存前校验：非法直接拒绝保存（弹窗内联错误 + 状态栏各发一份）
    if let Err(e) = pr_desk_rs::http_client(&proxy, std::time::Duration::from_secs(30)) {
        dialog_err(app, trf(&lang, "proxy_invalid", &[&e.to_string()]));
        return false;
    }
    // 服务器三字段按「正在编辑的档案」写回（local 档字段锁定，不读表单值）
    let edit_profile = app.settings_profile.clone();
    #[allow(clippy::type_complexity)]
    let (theme, language, mut profiles, active_profile, mut server_url, mut sync_token, mut ak_s,
         font_scale, renderer, window_width, window_height, panel_width) = {
        let a = app.app.lock_recover();
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
            a.cfg.renderer.clone(),
            a.cfg.window_width,
            a.cfg.window_height,
            a.cfg.panel_width,
        )
    };
    match edit_profile.as_str() {
        "local" => {} // 本地档无服务器字段，顶层键（= default 档）保持不动
        "default" => {
            server_url = app.s_server.trim().to_string();
            sync_token = app.s_sync_token.trim().to_string();
            ak_s = app.s_ak.trim().to_string();
        }
        name => {
            let vals = (
                app.s_server.trim().to_string(),
                app.s_sync_token.trim().to_string(),
                app.s_ak.trim().to_string(),
            );
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
    let new_cfg = pr_desk_rs::config::Config {
        server_url,
        sync_token,
        astrbot_api_key: ak_s,
        github_token: Some(app.s_gh_token.trim().to_string()).filter(|t| !t.is_empty()),
        proxy,
        repos,
        poll_interval_min: poll,
        theme,
        language,
        font_scale,
        renderer,
        llm_base_url: app.s_llm_base.trim().to_string(),
        llm_api_key: Some(app.s_llm_key.trim().to_string()).filter(|t| !t.is_empty()),
        llm_model: app.s_llm_model.trim().to_string(),
        ocr_path: if ocr_path.is_empty() { "ocr".into() } else { ocr_path.to_string() },
        review_concurrency,
        profiles,
        active_profile,
        window_width,
        window_height,
        panel_width,
    };
    if let Err(e) = new_cfg.save(&app.cfg_path) {
        dialog_err(app, trf(&lang, "cfg_save_failed", &[&e.to_string()]));
        return false;
    }
    {
        let mut a = app.app.lock_recover();
        a.cfg = new_cfg.clone();
        a.status_msg = config_status(&new_cfg);
        // 配置状态是常驻消息：推序号让在途的自动清除失效
        a.status_gen = a.status_gen.wrapping_add(1);
        a.status_msg_at = Some(Instant::now());
        // 并发上限即时生效（不影响已占用的槽位）
        a.review_slots.lock_recover().set_capacity(review_concurrency as usize);
    }
    // 自定义评审模板落盘（exe 旁 review_prompt.md；清空 = 删除文件回到默认模板）。
    // 回填竞态：异步回填未完成时跳过（此时表单里是空占位，写盘会覆盖磁盘新模板）
    if !app.s_prompt_loading {
        let prompt_path = pr_desk_rs::exe_dir().join("review_prompt.md");
        if app.s_review_prompt.trim().is_empty() {
            let _ = std::fs::remove_file(&prompt_path);
        } else if let Err(e) = std::fs::write(&prompt_path, &app.s_review_prompt) {
            tracing::warn!("写入 review_prompt.md 失败: {e}");
        }
    }
    // 设置页改的是顶层键（= "default" 档）：token 从空变非空时名单要多出 default
    app.profile_names = new_cfg.profile_names();
    crate::worker::send_msg(Msg::Status(tr(&lang, "cfg_saved").into()));
    let _ = app.cmd_tx.send(Cmd::ReloadConfig); // 热生效，不用重启
    true
}

/// 新增自定义档案：校验名字（非空/字符合法/非保留/不重名）→ 复制 default 档配置入库 + save。
pub(crate) fn profile_add(app: &mut EguiApp) {
    let name = app.add_profile_name.trim().to_string();
    let lang = app.lang();
    let prev = app.settings_profile.clone();
    {
        let mut a = app.app.lock_recover();
        if !pr_desk_rs::config::valid_profile_name(&name) {
            drop(a);
            dialog_err(app, tr(&lang, "profile_invalid_name").into());
            app.settings_profile = prev;
            return;
        }
        if pr_desk_rs::config::is_reserved_profile_name(&name) {
            drop(a);
            dialog_err(app, trf(&lang, "profile_reserved", &[&name]));
            app.settings_profile = prev;
            return;
        }
        if a.cfg.profiles.iter().any(|p| p.name == name) {
            drop(a);
            dialog_err(app, trf(&lang, "profile_exists", &[&name]));
            app.settings_profile = prev;
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
        if let Err(e) = a.cfg.save(&app.cfg_path) {
            a.cfg.profiles.retain(|p| p.name != name);
            drop(a);
            dialog_err(app, trf(&lang, "cfg_save_failed", &[&e.to_string()]));
            app.settings_profile = prev;
            return;
        }
        app.profile_names = a.cfg.profile_names();
    }
    app.dialog_error.clear(); // 成功：清掉之前的内联错误
    crate::worker::send_msg(Msg::Status(trf(&lang, "profile_added", &[&name])));
}

/// 删除自定义档案：只允许删非保留且非当前 active 的（db 文件保留，不删数据）。
fn profile_remove(app: &mut EguiApp) {
    let name = app.settings_profile.trim().to_string();
    let lang = app.lang();
    {
        let mut a = app.app.lock_recover();
        let removable = !pr_desk_rs::config::is_reserved_profile_name(&name)
            && a.cfg.profiles.iter().any(|p| p.name == name)
            && a.cfg.active_profile != name;
        if !removable {
            drop(a);
            dialog_err(app, tr(&lang, "profile_remove_denied").into());
            return;
        }
        let backup = a.cfg.profiles.clone();
        a.cfg.profiles.retain(|p| p.name != name);
        if let Err(e) = a.cfg.save(&app.cfg_path) {
            a.cfg.profiles = backup;
            drop(a);
            dialog_err(app, trf(&lang, "cfg_save_failed", &[&e.to_string()]));
            return;
        }
        app.profile_names = a.cfg.profile_names();
    }
    // 被删的正是设置页在编辑的档案：编辑目标回到 local（回填三字段）
    app.settings_profile = "local".into();
    let (s, t, ak) = {
        let a = app.app.lock_recover();
        profile_fields(&a.cfg, "local")
    };
    app.s_server = s;
    app.s_sync_token = t;
    app.s_ak = ak;
    app.dialog_error.clear();
    crate::worker::send_msg(Msg::Status(trf(&lang, "profile_removed", &[&name])));
}

/// 档案切换（设置页内编辑目标）：回填服务器三字段。
fn settings_profile_focus(app: &mut EguiApp) {
    let (server, token, ak) = {
        let a = app.app.lock_recover();
        profile_fields(&a.cfg, &app.settings_profile)
    };
    app.s_server = server;
    app.s_sync_token = token;
    app.s_ak = ak;
}

pub fn settings_window(app: &mut EguiApp, ctx: &egui::Context) {
    if !app.settings_open {
        return;
    }
    let lang = app.lang();
    let mut open = true;
    egui::Window::new(tr(&lang, "settings_title"))
        .id(egui::Id::unique("settings"))
        .collapsible(false)
        .resizable(true)
        .default_size([520.0, 560.0])
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            // 功能分 tab：数据源 / 通用 / LLM 预评审
            ui.horizontal(|ui| {
                for (i, t) in [
                    tr(&lang, "settings_tab_source"),
                    tr(&lang, "settings_tab_general"),
                    tr(&lang, "settings_tab_llm"),
                ]
                .iter()
                .enumerate()
                {
                    if ui.selectable_label(app.settings_tab == i, *t).clicked() {
                        app.settings_tab = i;
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("settings-scroll")
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    match app.settings_tab {
                        0 => tab_source(app, ui, &lang),
                        1 => tab_general(app, ui, &lang),
                        _ => tab_llm(app, ui, &lang),
                    }
                });
            // 弹窗内联错误行
            if !app.dialog_error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(209, 36, 47), &app.dialog_error);
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr(&lang, "save")).clicked() {
                        app.dialog_error.clear();
                        if save_settings(app) {
                            app.settings_open = false;
                        }
                    }
                    if ui.button(tr(&lang, "cancel")).clicked() {
                        app.settings_open = false;
                    }
                });
            });
        });
    if !open {
        app.settings_open = false;
    }
}

fn tab_source(app: &mut EguiApp, ui: &mut egui::Ui, lang: &str) {
    // ── 档案选择行：服务器地址 / sync_token / AstrBot Key 三字段按档案独立保存 ──
    ui.horizontal(|ui| {
        ui.label(tr(lang, "settings_profile_label"));
        let cur = app.settings_profile.clone();
        let label = if cur == "local" { tr(lang, "source_local").to_string() } else { cur.clone() };
        egui::ComboBox::from_id_salt("settings-profile")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for p in app.profile_names.clone() {
                    let text = if p == "local" { tr(lang, "source_local").to_string() } else { p.clone() };
                    if ui.selectable_label(app.settings_profile == p, text).clicked() {
                        app.settings_profile = p;
                        settings_profile_focus(app);
                    }
                }
            });
        // 「+」新档案：内联输入框，Enter 确认
        if app.adding_profile {
            let resp = ui.add(egui::TextEdit::singleline(&mut app.add_profile_name)
                .hint_text(tr(lang, "profile_add_hint"))
                .desired_width(140.0));
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                || ui.input(|i| i.key_pressed(egui::Key::Enter)) && resp.has_focus()
            {
                if !app.add_profile_name.trim().is_empty() {
                    let name = app.add_profile_name.trim().to_string();
                    profile_add(app);
                    app.settings_profile = name;
                    settings_profile_focus(app);
                }
                app.add_profile_name.clear();
                app.adding_profile = false;
            }
            if ui.button("✕").clicked() {
                app.adding_profile = false;
            }
        } else {
            if ui.button("＋").on_hover_text(tr(lang, "profile_add_tooltip")).clicked() {
                app.adding_profile = true;
            }
            // 删除当前档案（local 不可删）。两步确认：第一次点进入确认态（3s 超时还原）
            if app.settings_profile != "local" {
                let confirming = matches!(&app.del_confirm, Some((n, t)) if *n == app.settings_profile && t.elapsed().as_secs() < 3);
                let btn = ui
                    .button(RichText::new("×").color(if confirming {
                        egui::Color32::from_rgb(209, 36, 47)
                    } else {
                        ui.visuals().weak_text_color()
                    }))
                    .on_hover_text(if confirming {
                        tr(lang, "profile_delete_confirm")
                    } else {
                        tr(lang, "profile_delete_tooltip")
                    });
                if btn.clicked() {
                    if confirming {
                        app.del_confirm = None;
                        profile_remove(app);
                    } else {
                        app.del_confirm = Some((app.settings_profile.clone(), Instant::now()));
                    }
                }
                if confirming {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                }
            }
        }
    });
    let editing_local = app.settings_profile == "local";
    ui.add_enabled_ui(!editing_local, |ui| {
        field(ui, tr(lang, "settings_server_url"), |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.s_server)
                .hint_text(tr(lang, "settings_server_placeholder"))
                .desired_width(f32::INFINITY));
        });
        field(ui, tr(lang, "settings_sync_token"), |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.s_sync_token)
                .password(true)
                .hint_text(tr(lang, "settings_sync_token_placeholder"))
                .desired_width(f32::INFINITY));
        });
        field(ui, tr(lang, "settings_astrbot_key"), |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.s_ak)
                .password(true)
                .hint_text(tr(lang, "settings_astrbot_key_placeholder"))
                .desired_width(f32::INFINITY));
        });
    });
    if editing_local {
        ui.weak(tr(lang, "source_local_hint"));
    }
}

fn tab_general(app: &mut EguiApp, ui: &mut egui::Ui, lang: &str) {
    let gh_label = if app.gh_token_from_env {
        tr(lang, "settings_gh_token_env")
    } else {
        "GitHub token"
    };
    field(ui, gh_label, |ui| {
        ui.add(egui::TextEdit::singleline(&mut app.s_gh_token)
            .password(true)
            .hint_text(if app.gh_token_from_env {
                tr(lang, "settings_gh_token_placeholder_env")
            } else {
                tr(lang, "settings_gh_token_placeholder")
            })
            .desired_width(f32::INFINITY));
    });
    field(ui, tr(lang, "settings_proxy"), |ui| {
        ui.add(egui::TextEdit::singleline(&mut app.s_proxy)
            .hint_text("http://127.0.0.1:7890")
            .desired_width(f32::INFINITY));
    });
    ui.weak(tr(lang, "settings_proxy_hint"));
    field(ui, tr(lang, "settings_repos"), |ui| {
        ui.add(egui::TextEdit::multiline(&mut app.s_repos)
            .hint_text("owner/repo")
            .desired_rows(4)
            .desired_width(f32::INFINITY));
    });
    field(ui, tr(lang, "settings_poll_interval"), |ui| {
        ui.add(egui::DragValue::new(&mut app.s_poll).range(1..=120));
    });
    // 主题：四种风格；切换即时生效并持久化
    field(ui, tr(lang, "settings_theme"), |ui| {
        ui.horizontal_wrapped(|ui| {
            let cur = app.app.lock_recover().cfg.theme.clone();
            for name in crate::theme::THEMES {
                if ui.selectable_label(cur == name, tr(lang, crate::theme::label_key(name))).clicked() {
                    app.theme_changed(ui.ctx(), name);
                }
            }
        });
    });
    // 语言：分段控件；切换即时生效并持久化
    field(ui, tr(lang, "settings_language"), |ui| {
        ui.horizontal(|ui| {
            let cur = app.lang();
            if ui.selectable_label(cur != "en", tr(lang, "lang_name_zh")).clicked() {
                app.language_changed("zh");
            }
            if ui.selectable_label(cur == "en", tr(lang, "lang_name_en")).clicked() {
                app.language_changed("en");
            }
        });
    });
    // 字号三档（小/标准/大）
    field(ui, tr(lang, "settings_font_size"), |ui| {
        ui.horizontal(|ui| {
            let cur = app.app.lock_recover().cfg.font_scale;
            for (v, label) in [
                (0.9, tr(lang, "font_size_small")),
                (1.0, tr(lang, "font_size_normal")),
                (1.15, tr(lang, "font_size_large")),
            ] {
                if ui.selectable_label((cur - v).abs() < 0.01, label).clicked() {
                    app.font_scale_changed(ui.ctx(), v);
                }
            }
        });
    });
}

fn tab_llm(app: &mut EguiApp, ui: &mut egui::Ui, lang: &str) {
    field(ui, tr(lang, "settings_llm_base"), |ui| {
        ui.add(egui::TextEdit::singleline(&mut app.s_llm_base)
            .hint_text("https://api.openai.com/v1")
            .desired_width(f32::INFINITY));
    });
    field(ui, tr(lang, "settings_llm_key"), |ui| {
        ui.add(egui::TextEdit::singleline(&mut app.s_llm_key)
            .password(true)
            .hint_text("sk-...")
            .desired_width(f32::INFINITY));
    });
    field(ui, tr(lang, "settings_model_name"), |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.s_llm_model)
                .hint_text("gpt-4o-mini")
                .desired_width(200.0));
            // 「获取」：用弹窗里正在编辑的 base/key 拉取（无需先保存）
            if ui.button(tr(lang, "settings_fetch_models")).clicked() {
                let (base, key) = (app.s_llm_base.clone(), app.s_llm_key.clone());
                let _ = app.cmd_tx.send(Cmd::FetchModels { base, key });
            }
        });
    });
    // 拉取成功后的模型选择列表（标题行可折叠；选中即填入输入框）
    if !app.model_list.is_empty() {
        let title = format!(
            "{}{}{}",
            tr(lang, "settings_model_list_pre"),
            app.model_list.len(),
            tr(lang, "settings_model_list_post")
        );
        egui::CollapsingHeader::new(title)
            .id_salt("model-list")
            .default_open(app.model_list_open)
            .show(ui, |ui| {
                app.model_list_open = true;
                for m in app.model_list.clone() {
                    if ui
                        .selectable_label(app.s_llm_model == m, &m)
                        .clicked()
                    {
                        app.s_llm_model = m;
                    }
                }
            });
    }
    field(ui, tr(lang, "settings_ocr_path"), |ui| {
        ui.add(egui::TextEdit::singleline(&mut app.s_ocr_path)
            .hint_text("ocr")
            .desired_width(f32::INFINITY));
    });
    field(ui, tr(lang, "settings_review_concurrency"), |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut app.s_review_conc).range(1..=16));
            ui.weak(tr(lang, "settings_concurrency_hint"));
        });
    });
    field(ui, tr(lang, "settings_review_prompt"), |ui| {
        ui.add_enabled(
            !app.s_prompt_loading,
            egui::TextEdit::multiline(&mut app.s_review_prompt)
                .hint_text(tr(lang, "settings_review_prompt_hint"))
                .desired_rows(6)
                .desired_width(f32::INFINITY),
        );
    });
}

fn field(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.label(label);
    add(ui);
    ui.add_space(6.0);
}

/// 首启引导：未配置时盖住主区，引导打开设置；「稍后再说」仅本次会话不再弹。
pub fn onboarding_window(app: &mut EguiApp, ctx: &egui::Context) {
    let onboarding = app.app.lock_recover().cfg.is_unconfigured();
    if !onboarding || app.settings_open || app.onboarding_dismissed {
        return;
    }
    let lang = app.lang();
    egui::Window::new(tr(&lang, "welcome_title"))
        .id(egui::Id::unique("onboarding"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(tr(&lang, "welcome_hint"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(tr(&lang, "welcome_later")).clicked() {
                    app.onboarding_dismissed = true;
                }
                if ui.button(tr(&lang, "welcome_open_settings")).clicked() {
                    open_settings(app);
                }
            });
        });
}

/// 测试指令用（与「保存」按钮同函数）。
pub fn save_settings_pub(app: &mut EguiApp) -> bool {
    save_settings(app)
}
