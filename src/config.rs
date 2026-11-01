//! 配置：config.toml（服务器地址、sync_token、GitHub token、repos、轮询间隔）。
//! 首启生成模板；设置页保存后经 save() 写回（逐行替换，尽力保留注释）。
//! GitHub token 优先级：环境变量 GITHUB_TOKEN > config.toml。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const TEMPLATE: &str = r#"# PR Desk 桌面端配置（也可在应用内 ⚙ 设置页修改，保存后即时生效）
# AstrBot 服务器地址（插件 web API 挂载于 /api/v1/plugins/extensions/astrbot_plugin_github_pr_review）
server_url = "http://127.0.0.1:6185"
# 插件配置里的 sync_token（留空 = 同步关闭，仅本地浏览 GitHub 数据）
sync_token = ""
# AstrBot 面板的 API Key（新版 AstrBot 对插件 API 路由强制鉴权；在 AstrBot「API 密钥」管理页创建）
astrbot_api_key = ""
# GitHub token（明文本地存储自用；环境变量 GITHUB_TOKEN 优先级更高）
github_token = ""
# 关注的仓库（拉取 open PR 列表）
repos = ["owner/repo"]
# 轮询/自动同步间隔（分钟）
poll_interval_min = 5
# 界面主题：light 或 dark
theme = "light"
# 界面语言：zh 或 en
language = "zh"
# 本地模式（本地数据源）的列表拉取范围：mine = 与我相关（我提的/请求我评审的/指派我的），all = 仓库全部 open PR
local_scope = "mine"
# 本地 LLM 预评审（OpenAI 兼容 API；llm_api_key 留空时可用环境变量 LLM_API_KEY）
llm_base_url = "https://api.openai.com/v1"
llm_api_key = ""
llm_model = ""
# open-code-review CLI 路径（行级评论评审引擎；留空或 "ocr" = 自动查找：优先随包 bin/ 目录，其次 PATH）
ocr_path = "ocr"
# 预评审并发数（1-8；每个 PR 评审 = 评分 + OCR 多次 LLM 调用，配额紧张时调低）
review_concurrency = 3
# 多数据源：active_profile 选择当前数据源（"local" = 本地库 / "default" = 顶层键 / 自定义档案名）
# 自定义远程档案三段式（name 仅允许 a-z0-9_-）：
# profile.<name>.server_url / profile.<name>.sync_token / profile.<name>.api_key
"#;

/// 一个远程数据源档案（AstrBot 插件服务器连接参数）。
/// 名为 "default" 的档案不单独落盘——它是顶层 server_url/sync_token/astrbot_api_key 的映射。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub server_url: String,
    pub sync_token: String,
    pub api_key: String,
}

/// 档案名字符合法性：非空、≤64 字符、仅 a-z 0-9 _ -（防路径穿越，兼作 profile.* 键解析约束）。
pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// 保留档案名：local（本地模式）与 default（顶层键映射）不可用作自定义档案。
pub fn is_reserved_profile_name(name: &str) -> bool {
    name == "local" || name == "default"
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server_url: String,
    pub sync_token: String,
    /// AstrBot 面板 API Key（新版 AstrBot 对 /api 路由强制鉴权；空 = 不发送）
    pub astrbot_api_key: String,
    pub github_token: Option<String>,
    pub repos: Vec<String>,
    pub poll_interval_min: u64,
    /// 界面主题："light" | "dark"
    pub theme: String,
    /// 界面语言："zh" | "en"
    pub language: String,
    /// 本地模式列表拉取范围："mine"（与我相关）| "all"（仓库全部 open PR）
    pub local_scope: String,
    /// OpenAI 兼容 API 地址（本地预评审用）
    pub llm_base_url: String,
    pub llm_api_key: Option<String>,
    /// LLM 模型名（空 = 未配置，预评审不可用）
    pub llm_model: String,
    /// ocr CLI 路径（"ocr" = 自动查找：优先随包 bin/ 目录，其次 PATH）
    pub ocr_path: String,
    /// 预评审并发数（1-8）
    pub review_concurrency: u64,
    /// 远程数据源档案列表（自定义档；"default" 档由顶层键合成，见 load 的兼容迁移）
    pub profiles: Vec<Profile>,
    /// 当前数据源："local" = 本地模式；"default" = 顶层 server_url/sync_token；其它 = profiles 里的档案名
    pub active_profile: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:6185".into(),
            sync_token: String::new(),
            astrbot_api_key: String::new(),
            github_token: None,
            repos: Vec::new(),
            poll_interval_min: 5,
            theme: "light".into(),
            language: "zh".into(),
            local_scope: "mine".into(),
            llm_base_url: "https://api.openai.com/v1".into(),
            llm_api_key: None,
            llm_model: String::new(),
            ocr_path: "ocr".into(),
            review_concurrency: 3,
            profiles: Vec::new(),
            active_profile: "local".into(),
        }
    }
}

fn parse_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let line = line.trim();
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    Some(rest)
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 解析一行 `profile.<name>.<field> = value`（rest 已去掉 "profile." 前缀）。
/// 非法名字/保留名/未知字段跳过并告警，不让手写的坏行炸掉整个配置加载。
fn parse_profile_line(rest: &str, profiles: &mut Vec<Profile>) {
    let Some((name, rest)) = rest.split_once('.') else { return };
    let Some((field, value)) = rest.split_once('=') else { return };
    if !valid_profile_name(name) || is_reserved_profile_name(name) {
        tracing::warn!("忽略非法/保留档案名的配置行: profile.{name}.*");
        return;
    }
    let value = unquote(value);
    let idx = match profiles.iter().position(|p| p.name == name) {
        Some(i) => i,
        None => {
            profiles.push(Profile {
                name: name.to_string(),
                server_url: String::new(),
                sync_token: String::new(),
                api_key: String::new(),
            });
            profiles.len() - 1
        }
    };
    match field.trim() {
        "server_url" => profiles[idx].server_url = value,
        "sync_token" => profiles[idx].sync_token = value,
        "api_key" => profiles[idx].api_key = value,
        other => tracing::warn!("忽略未知档案字段: profile.{name}.{other}"),
    }
}

impl Config {
    /// 读取 config.toml；不存在则生成模板并用默认值。
    /// 返回是否「未配置」（repos 为空或全是模板占位）。
    pub fn is_unconfigured(&self) -> bool {
        self.repos.is_empty() || self.repos.iter().all(|r| r == "owner/repo")
    }

    /// 当前是否深色主题。
    pub fn is_dark(&self) -> bool {
        self.theme == "dark"
    }

    /// GitHub token：环境变量 GITHUB_TOKEN 优先于 config.toml。
    pub fn token(&self) -> Option<String> {
        std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| self.github_token.clone().filter(|t| !t.is_empty()))
    }

    /// LLM API key：环境变量 LLM_API_KEY 优先于 config.toml。
    pub fn llm_key(&self) -> Option<String> {
        std::env::var("LLM_API_KEY")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| self.llm_api_key.clone().filter(|t| !t.is_empty()))
    }

    /// 当前生效的同步档案："local" → None（本地模式）；"default" → 用顶层键构造；
    /// 其它 → profiles 查找。sync_token 为空一律视为 None（同步关闭）。
    pub fn active_sync(&self) -> Option<Profile> {
        match self.active_profile.as_str() {
            "local" => None,
            "default" => {
                if self.sync_token.is_empty() {
                    None
                } else {
                    Some(Profile {
                        name: "default".into(),
                        server_url: self.server_url.clone(),
                        sync_token: self.sync_token.clone(),
                        api_key: self.astrbot_api_key.clone(),
                    })
                }
            }
            name => self
                .profiles
                .iter()
                .find(|p| p.name == name)
                .filter(|p| !p.sync_token.is_empty())
                .cloned(),
        }
    }

    /// "default" 档案是否可用：顶层 sync_token 非空（load 的迁移会同时合成进 profiles）。
    pub fn has_default_profile(&self) -> bool {
        !self.sync_token.is_empty() || self.profiles.iter().any(|p| p.name == "default")
    }

    /// 档案是否存在（local 恒存在；default 需顶层 token；自定义查 profiles）。
    pub fn has_profile(&self, name: &str) -> bool {
        name == "local"
            || (name == "default" && self.has_default_profile())
            || self.profiles.iter().any(|p| p.name == name)
    }

    /// UI 展示名单：第一个恒为 "local"，第二个 "default"（存在的话），然后自定义名。
    pub fn profile_names(&self) -> Vec<String> {
        let mut out = vec!["local".to_string()];
        if self.has_default_profile() {
            out.push("default".into());
        }
        out.extend(
            self.profiles
                .iter()
                .filter(|p| !is_reserved_profile_name(&p.name))
                .map(|p| p.name.clone()),
        );
        out
    }

    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(path, TEMPLATE).context("写入 config.toml 模板失败")?;
            tracing::info!("已生成配置模板 {:?}，请在应用内设置页填写", path);
            return Ok(Config::default());
        }
        let text = fs::read_to_string(path).context("读取 config.toml 失败")?;
        let mut cfg = Config::default();
        let mut active: Option<String> = None;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // 自定义档案：profile.<name>.<server_url|sync_token|api_key> = "..."
            if let Some(rest) = line.strip_prefix("profile.") {
                parse_profile_line(rest, &mut cfg.profiles);
                continue;
            }
            if let Some(v) = parse_value(line, "server_url") {
                cfg.server_url = unquote(v);
            } else if let Some(v) = parse_value(line, "sync_token") {
                cfg.sync_token = unquote(v);
            } else if let Some(v) = parse_value(line, "astrbot_api_key") {
                cfg.astrbot_api_key = unquote(v);
            } else if let Some(v) = parse_value(line, "github_token") {
                cfg.github_token = Some(unquote(v)).filter(|t| !t.is_empty());
            } else if let Some(v) = parse_value(line, "repos") {
                cfg.repos = v
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|s| unquote(s.trim()))
                    .filter(|s| !s.is_empty())
                    .collect();
            } else if let Some(v) = parse_value(line, "poll_interval_min") {
                cfg.poll_interval_min = v.trim().parse().unwrap_or(5);
            } else if let Some(v) = parse_value(line, "theme") {
                // 只认 light/dark，其它值（老配置、手写错误）回退默认
                let v = unquote(v);
                cfg.theme = if v == "dark" { "dark".into() } else { "light".into() };
            } else if let Some(v) = parse_value(line, "language") {
                // 只认 zh/en，其它值（老配置、手写错误）回退默认
                let v = unquote(v);
                cfg.language = if v == "en" { "en".into() } else { "zh".into() };
            } else if let Some(v) = parse_value(line, "local_scope") {
                // 只认 mine/all，其它值回退默认
                let v = unquote(v);
                cfg.local_scope = if v == "all" { "all".into() } else { "mine".into() };
            } else if let Some(v) = parse_value(line, "llm_base_url") {
                let v = unquote(v);
                if !v.is_empty() {
                    cfg.llm_base_url = v;
                }
            } else if let Some(v) = parse_value(line, "llm_api_key") {
                cfg.llm_api_key = Some(unquote(v)).filter(|t| !t.is_empty());
            } else if let Some(v) = parse_value(line, "llm_model") {
                cfg.llm_model = unquote(v);
            } else if let Some(v) = parse_value(line, "ocr_path") {
                let v = unquote(v);
                if !v.is_empty() {
                    cfg.ocr_path = v;
                }
            } else if let Some(v) = parse_value(line, "review_concurrency") {
                // 1-8，超出范围回退默认 3
                cfg.review_concurrency = match v.trim().parse::<u64>() {
                    Ok(n) if (1..=8).contains(&n) => n,
                    _ => 3,
                };
            } else if let Some(v) = parse_value(line, "active_profile") {
                active = Some(unquote(v)).filter(|s| !s.is_empty());
            }
        }
        // 兼容迁移：顶层 sync_token 非空 = 旧版单服务器配置 → 合成 "default" 档（不落新键，
        // 值始终与顶层键同步：active_sync("default") 直接用顶层键构造）
        if !cfg.sync_token.is_empty() && !cfg.profiles.iter().any(|p| p.name == "default") {
            cfg.profiles.push(Profile {
                name: "default".into(),
                server_url: cfg.server_url.clone(),
                sync_token: cfg.sync_token.clone(),
                api_key: cfg.astrbot_api_key.clone(),
            });
        }
        // active_profile 缺省：有顶层 sync_token 的旧配置落 "default"，否则本地模式
        cfg.active_profile = match active {
            Some(name) => name,
            None => {
                if cfg.sync_token.is_empty() {
                    "local".into()
                } else {
                    "default".into()
                }
            }
        };
        Ok(cfg)
    }

    /// 写回 config.toml：逐行替换已知键的值，注释/其它行原样保留；缺键追加到末尾。
    pub fn save(&self, path: &Path) -> Result<()> {
        let repos_toml = format!(
            "[{}]",
            self.repos.iter().map(|r| quote(r)).collect::<Vec<_>>().join(", ")
        );
        let kv: [(&str, String); 15] = [
            ("server_url", quote(&self.server_url)),
            ("sync_token", quote(&self.sync_token)),
            ("astrbot_api_key", quote(&self.astrbot_api_key)),
            ("github_token", quote(self.github_token.as_deref().unwrap_or(""))),
            ("repos", repos_toml),
            ("poll_interval_min", self.poll_interval_min.to_string()),
            ("theme", quote(&self.theme)),
            ("language", quote(&self.language)),
            ("local_scope", quote(&self.local_scope)),
            ("llm_base_url", quote(&self.llm_base_url)),
            ("llm_api_key", quote(self.llm_api_key.as_deref().unwrap_or(""))),
            ("llm_model", quote(&self.llm_model)),
            ("ocr_path", quote(&self.ocr_path)),
            ("review_concurrency", self.review_concurrency.to_string()),
            ("active_profile", quote(&self.active_profile)),
        ];

        let mut lines: Vec<String> = match fs::read_to_string(path) {
            Ok(text) => text.lines().map(|l| l.to_string()).collect(),
            Err(_) => TEMPLATE.lines().map(|l| l.to_string()).collect(),
        };
        // 自定义档案行全部摘除后按 self.profiles 重写（profile.* 是唯一前缀，不碰注释行）
        lines.retain(|l| {
            let t = l.trim_start();
            t.is_empty() || t.starts_with('#') || !t.starts_with("profile.")
        });
        let mut seen = [false; 15];
        for (i, line) in lines.iter_mut().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            for (k, (key, val)) in kv.iter().enumerate() {
                if trimmed.starts_with(&format!("{key}")) && trimmed[key.len()..].trim_start().starts_with('=') {
                    *line = format!("{key} = {val}");
                    seen[k] = true;
                    break;
                }
            }
            let _ = i;
        }
        for (k, (key, val)) in kv.iter().enumerate() {
            if !seen[k] {
                lines.push(format!("{key} = {val}"));
            }
        }
        // 自定义档案重写到末尾（"default" 映射顶层键不落 profile.* 行；非法名防御性跳过）
        for p in &self.profiles {
            if is_reserved_profile_name(&p.name) || !valid_profile_name(&p.name) {
                continue;
            }
            lines.push(format!("profile.{}.server_url = {}", p.name, quote(&p.server_url)));
            lines.push(format!("profile.{}.sync_token = {}", p.name, quote(&p.sync_token)));
            lines.push(format!("profile.{}.api_key = {}", p.name, quote(&p.api_key)));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(path, lines.join("\n") + "\n").context("写入 config.toml 失败")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let dir = std::env::temp_dir().join(format!("prdesk-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        // 首次：生成模板并返回默认值
        let cfg = Config::load(&path).unwrap();
        assert!(path.exists());
        assert_eq!(cfg.poll_interval_min, 5);
        assert!(cfg.repos.is_empty()); // 生成模板的路径返回默认值（模板未解析）
        // 再次加载：解析模板内容
        let cfg2 = Config::load(&path).unwrap();
        assert_eq!(cfg2.repos, vec!["owner/repo".to_string()]);
        assert_eq!(cfg2.server_url, "http://127.0.0.1:6185");
        assert_eq!(cfg2.llm_base_url, "https://api.openai.com/v1");
        assert_eq!(cfg2.llm_api_key, None);
        assert_eq!(cfg2.llm_model, "");
        assert_eq!(cfg2.ocr_path, "ocr");
        assert_eq!(cfg2.language, "zh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_roundtrip_preserves_comments() {
        let dir = std::env::temp_dir().join(format!("prdesk-cfg-save-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = Config::load(&path).unwrap(); // 生成模板

        let cfg = Config {
            server_url: "http://nas:6185".into(),
            sync_token: "tok-1".into(),
            astrbot_api_key: "ak-1".into(),
            github_token: Some("ghp_x".into()),
            repos: vec!["a/b".into(), "c/d".into()],
            poll_interval_min: 15,
            theme: "dark".into(),
            language: "en".into(),
            local_scope: "mine".into(),
            llm_base_url: "http://nas:11434/v1".into(),
            llm_api_key: Some("sk-test".into()),
            llm_model: "qwen3-coder".into(),
            ocr_path: "D:/tools/ocr.exe".into(),
            review_concurrency: 5,
            profiles: Vec::new(),
            active_profile: "local".into(),
        };
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 轮询/自动同步间隔（分钟）"), "注释应保留");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.server_url, "http://nas:6185");
        assert_eq!(loaded.sync_token, "tok-1");
        assert_eq!(loaded.astrbot_api_key, "ak-1");
        assert_eq!(loaded.github_token.as_deref(), Some("ghp_x"));
        assert_eq!(loaded.repos, vec!["a/b".to_string(), "c/d".to_string()]);
        assert_eq!(loaded.poll_interval_min, 15);
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.language, "en");
        assert_eq!(loaded.llm_base_url, "http://nas:11434/v1");
        assert_eq!(loaded.llm_api_key.as_deref(), Some("sk-test"));
        assert_eq!(loaded.llm_model, "qwen3-coder");
        assert_eq!(loaded.ocr_path, "D:/tools/ocr.exe");
        assert_eq!(loaded.review_concurrency, 5);
        assert!(loaded.is_dark());
        assert!(!loaded.is_unconfigured());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn llm_key_fallback() {
        // 环境变量 LLM_API_KEY 未设置时回退 config.toml；两者皆空则为 None
        let mut c = Config::default();
        if std::env::var("LLM_API_KEY").is_err() {
            assert_eq!(c.llm_key(), None);
            c.llm_api_key = Some("sk-cfg".into());
            assert_eq!(c.llm_key().as_deref(), Some("sk-cfg"));
            c.llm_api_key = Some(String::new()).filter(|t| !t.is_empty());
            assert_eq!(c.llm_key(), None);
        }
    }

    #[test]
    fn unconfigured_detection() {
        assert!(Config::default().is_unconfigured());
        let mut c = Config::default();
        c.repos = vec!["owner/repo".into()];
        assert!(c.is_unconfigured()); // 模板占位
        c.repos = vec!["vllm-project/vllm".into()];
        assert!(!c.is_unconfigured());
    }

    // ── 多数据源档案（P1）──

    fn profile(name: &str, url: &str, token: &str, key: &str) -> Profile {
        Profile {
            name: name.into(),
            server_url: url.into(),
            sync_token: token.into(),
            api_key: key.into(),
        }
    }

    #[test]
    fn profile_roundtrip_and_stale_lines_removed() {
        let dir = std::env::temp_dir().join(format!("prdesk-cfg-prof-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = Config::load(&path).unwrap(); // 生成模板

        let mut cfg = Config::default();
        cfg.profiles = vec![
            profile("nas", "http://nas:6185", "tok-nas", "ak-nas"),
            profile("home_srv", "http://home:6185", "tok-home", ""),
        ];
        cfg.active_profile = "nas".into();
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("profile.nas.server_url = \"http://nas:6185\""));
        assert!(text.contains("profile.home_srv.api_key = \"\""));
        assert!(text.contains("active_profile = \"nas\""));
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.profiles, cfg.profiles);
        assert_eq!(loaded.active_profile, "nas");
        assert_eq!(
            loaded.active_sync(),
            Some(profile("nas", "http://nas:6185", "tok-nas", "ak-nas"))
        );
        assert_eq!(loaded.profile_names(), vec!["local".to_string(), "nas".to_string(), "home_srv".to_string()]);

        // 删掉一个档案后保存：旧的 profile.home_srv.* 行应被清除（先摘后重写）
        loaded.profiles.iter().find(|p| p.name == "home_srv").unwrap();
        let mut cfg2 = loaded;
        cfg2.profiles.retain(|p| p.name != "home_srv");
        cfg2.save(&path).unwrap();
        let text2 = std::fs::read_to_string(&path).unwrap();
        assert!(!text2.contains("profile.home_srv."), "已删档案的行应被清除");
        assert!(text2.contains("profile.nas.sync_token"));
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.profiles.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_config_migrates_to_default_profile() {
        let dir = std::env::temp_dir().join(format!("prdesk-cfg-mig-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        // 旧版配置：只有顶层服务器三键，没有 active_profile / profile.* 行
        std::fs::write(
            &path,
            "server_url = \"http://nas:6185\"\nsync_token = \"tok-1\"\nastrbot_api_key = \"ak-1\"\n",
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        // 迁移：合成 default 档；active_profile 缺省落 "default"
        assert!(cfg.profiles.contains(&profile("default", "http://nas:6185", "tok-1", "ak-1")));
        assert_eq!(cfg.active_profile, "default");
        assert_eq!(
            cfg.active_sync(),
            Some(profile("default", "http://nas:6185", "tok-1", "ak-1"))
        );
        assert_eq!(cfg.profile_names(), vec!["local".to_string(), "default".to_string()]);
        // 保存：default 档不落 profile.* 新键；重载后迁移仍成立
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("profile.default."), "default 档不应落 profile.* 行");
        assert!(text.contains("active_profile = \"default\""));
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.active_profile, "default");
        assert!(reloaded.has_profile("default"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_profile_defaults_to_local_without_token() {
        let dir = std::env::temp_dir().join(format!("prdesk-cfg-nod-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "server_url = \"http://x:6185\"\nsync_token = \"\"\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.active_profile, "local");
        assert!(!cfg.has_profile("default"));
        assert_eq!(cfg.profile_names(), vec!["local".to_string()]);
        assert_eq!(cfg.active_sync(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_profile_names_rejected_on_parse() {
        let dir = std::env::temp_dir().join(format!("prdesk-cfg-bad-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "profile.bad name.sync_token = \"x\"\n\
             profile..sync_token = \"x\"\n\
             profile.UPPER.sync_token = \"x\"\n\
             profile.default.sync_token = \"x\"\n\
             profile.nas.sync_token = \"tok\"\n\
             profile.nas.server_url = \"http://nas:6185\"\n\
             profile.nas.bogus = \"?\"\n",
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        // 含空格 / 空名 / 大写 / 保留名 全部拒绝；只有 nas 建档
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.profiles[0].name, "nas");
        assert_eq!(cfg.profiles[0].sync_token, "tok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_sync_semantics() {
        let mut c = Config::default();
        c.active_profile = "local".into();
        assert_eq!(c.active_sync(), None);
        // default：顶层 token 空 → None；非空 → Some（用顶层键构造）
        c.active_profile = "default".into();
        assert_eq!(c.active_sync(), None);
        c.sync_token = "tok".into();
        assert_eq!(c.active_sync().unwrap().sync_token, "tok");
        // 自定义：token 空 → None；非空 → Some；未知名 → None
        c.profiles = vec![profile("nas", "http://nas:6185", "", ""), profile("home", "http://h:1", "t2", "")];
        c.active_profile = "nas".into();
        assert_eq!(c.active_sync(), None);
        c.active_profile = "home".into();
        assert_eq!(c.active_sync().unwrap().name, "home");
        c.active_profile = "ghost".into();
        assert_eq!(c.active_sync(), None);
    }

    #[test]
    fn profile_name_validation() {
        assert!(valid_profile_name("nas"));
        assert!(valid_profile_name("home-srv_2"));
        assert!(!valid_profile_name(""));
        assert!(!valid_profile_name("Bad"));
        assert!(!valid_profile_name("a b"));
        assert!(!valid_profile_name("../etc"));
        assert!(!valid_profile_name(&"x".repeat(65)));
        assert!(is_reserved_profile_name("local"));
        assert!(is_reserved_profile_name("default"));
        assert!(!is_reserved_profile_name("nas"));
    }
}
