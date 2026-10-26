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
# GitHub token（明文本地存储自用；环境变量 GITHUB_TOKEN 优先级更高）
github_token = ""
# 关注的仓库（拉取 open PR 列表）
repos = ["owner/repo"]
# 轮询/自动同步间隔（分钟）
poll_interval_min = 5
# 界面主题：light 或 dark
theme = "light"
# 本地 LLM 预评审（OpenAI 兼容 API；llm_api_key 留空时可用环境变量 LLM_API_KEY）
llm_base_url = "https://api.openai.com/v1"
llm_api_key = ""
llm_model = ""
# open-code-review CLI 路径（行级评论评审引擎；默认 "ocr" 从 PATH 查找）
ocr_path = "ocr"
"#;

#[derive(Debug, Clone)]
pub struct Config {
    pub server_url: String,
    pub sync_token: String,
    pub github_token: Option<String>,
    pub repos: Vec<String>,
    pub poll_interval_min: u64,
    /// 界面主题："light" | "dark"
    pub theme: String,
    /// OpenAI 兼容 API 地址（本地预评审用）
    pub llm_base_url: String,
    pub llm_api_key: Option<String>,
    /// LLM 模型名（空 = 未配置，预评审不可用）
    pub llm_model: String,
    /// ocr CLI 路径（默认 "ocr" 从 PATH 查找）
    pub ocr_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:6185".into(),
            sync_token: String::new(),
            github_token: None,
            repos: Vec::new(),
            poll_interval_min: 5,
            theme: "light".into(),
            llm_base_url: "https://api.openai.com/v1".into(),
            llm_api_key: None,
            llm_model: String::new(),
            ocr_path: "ocr".into(),
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

    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(path, TEMPLATE).context("写入 config.toml 模板失败")?;
            tracing::info!("已生成配置模板 {:?}，请在应用内 ⚙ 设置页填写", path);
            return Ok(Config::default());
        }
        let text = fs::read_to_string(path).context("读取 config.toml 失败")?;
        let mut cfg = Config::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(v) = parse_value(line, "server_url") {
                cfg.server_url = unquote(v);
            } else if let Some(v) = parse_value(line, "sync_token") {
                cfg.sync_token = unquote(v);
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
            }
        }
        Ok(cfg)
    }

    /// 写回 config.toml：逐行替换已知键的值，注释/其它行原样保留；缺键追加到末尾。
    pub fn save(&self, path: &Path) -> Result<()> {
        let repos_toml = format!(
            "[{}]",
            self.repos.iter().map(|r| quote(r)).collect::<Vec<_>>().join(", ")
        );
        let kv: [(&str, String); 10] = [
            ("server_url", quote(&self.server_url)),
            ("sync_token", quote(&self.sync_token)),
            ("github_token", quote(self.github_token.as_deref().unwrap_or(""))),
            ("repos", repos_toml),
            ("poll_interval_min", self.poll_interval_min.to_string()),
            ("theme", quote(&self.theme)),
            ("llm_base_url", quote(&self.llm_base_url)),
            ("llm_api_key", quote(self.llm_api_key.as_deref().unwrap_or(""))),
            ("llm_model", quote(&self.llm_model)),
            ("ocr_path", quote(&self.ocr_path)),
        ];

        let mut lines: Vec<String> = match fs::read_to_string(path) {
            Ok(text) => text.lines().map(|l| l.to_string()).collect(),
            Err(_) => TEMPLATE.lines().map(|l| l.to_string()).collect(),
        };
        let mut seen = [false; 10];
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
            github_token: Some("ghp_x".into()),
            repos: vec!["a/b".into(), "c/d".into()],
            poll_interval_min: 15,
            theme: "dark".into(),
            llm_base_url: "http://nas:11434/v1".into(),
            llm_api_key: Some("sk-test".into()),
            llm_model: "qwen3-coder".into(),
            ocr_path: "D:/tools/ocr.exe".into(),
        };
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 轮询/自动同步间隔（分钟）"), "注释应保留");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.server_url, "http://nas:6185");
        assert_eq!(loaded.sync_token, "tok-1");
        assert_eq!(loaded.github_token.as_deref(), Some("ghp_x"));
        assert_eq!(loaded.repos, vec!["a/b".to_string(), "c/d".to_string()]);
        assert_eq!(loaded.poll_interval_min, 15);
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.llm_base_url, "http://nas:11434/v1");
        assert_eq!(loaded.llm_api_key.as_deref(), Some("sk-test"));
        assert_eq!(loaded.llm_model, "qwen3-coder");
        assert_eq!(loaded.ocr_path, "D:/tools/ocr.exe");
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
}
