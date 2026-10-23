//! 配置：config.toml（服务器地址、sync_token、repos、轮询间隔），首启生成模板。
//! GitHub token 只从环境变量 GITHUB_TOKEN 读，不写文件。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const TEMPLATE: &str = r#"# PR Desk 桌面端配置
# AstrBot 服务器地址（插件 web API 挂载于 /api/v1/plugins/extensions/astrbot_plugin_github_pr_review）
server_url = "http://127.0.0.1:6185"
# 插件配置里的 sync_token（留空 = 同步关闭，仅本地浏览 GitHub 数据）
sync_token = ""
# 关注的仓库（拉取 open PR 列表）
repos = ["owner/repo"]
# 轮询/自动同步间隔（分钟）
poll_interval_min = 5
"#;

#[derive(Debug, Clone)]
pub struct Config {
    pub server_url: String,
    pub sync_token: String,
    pub repos: Vec<String>,
    pub poll_interval_min: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:6185".into(),
            sync_token: String::new(),
            repos: Vec::new(),
            poll_interval_min: 5,
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

impl Config {
    /// 读取 config.toml；不存在则生成模板并用默认值。
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(path, TEMPLATE).context("写入 config.toml 模板失败")?;
            tracing::info!("已生成配置模板 {:?}，请填写后重启", path);
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
            }
        }
        Ok(cfg)
    }

    pub fn github_token() -> Option<String> {
        std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty())
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
