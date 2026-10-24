//! GitHub API 客户端（只读）：open PR 列表 + PR diff（带 Accept: diff header）。
//! 每次响应解析 X-RateLimit-Remaining 写入共享状态（rate_limit_handle），供 UI 状态栏展示。

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::Value;

const API: &str = "https://api.github.com";

pub struct GitHub {
    client: reqwest::Client,
    /// 最近一次响应的 X-RateLimit-Remaining（worker 写入，UI 状态栏读取）。
    rate_limit: Arc<Mutex<Option<u64>>>,
}

#[derive(Debug, Clone)]
pub struct OpenPr {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub author: String,
    pub url: String,
    pub head_sha: String,
}

impl GitHub {
    pub fn new(token: Option<String>) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Accept", "application/vnd.github+json".parse()?);
        headers.insert("X-GitHub-Api-Version", "2022-11-28".parse()?);
        if let Some(t) = token {
            headers.insert(
                "Authorization",
                format!("Bearer {t}").parse().context("token 含非法字符")?,
            );
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent("pr-desk-rs")
            .build()?;
        Ok(Self { client, rate_limit: Arc::new(Mutex::new(None)) })
    }

    /// 配额共享句柄：最近一次响应的 X-RateLimit-Remaining。
    pub fn rate_limit_handle(&self) -> Arc<Mutex<Option<u64>>> {
        self.rate_limit.clone()
    }

    /// 当前已知的剩余配额（未发过请求或响应无该头时为 None）。
    #[allow(dead_code)]
    pub fn rate_limit_remaining(&self) -> Option<u64> {
        *self.rate_limit.lock().unwrap()
    }

    fn record_rate_limit(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(n) = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
        {
            *self.rate_limit.lock().unwrap() = Some(n);
        }
    }

    /// 拉取仓库的 open PR 列表。
    pub async fn fetch_open_prs(&self, repo: &str) -> Result<Vec<OpenPr>> {
        let url = format!("{API}/repos/{repo}/pulls?state=open&per_page=100");
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub API {}: {}", resp.status(), repo);
        }
        let body: Vec<Value> = resp.json().await?;
        Ok(body
            .into_iter()
            .map(|it| OpenPr {
                repo: repo.to_string(),
                number: it["number"].as_i64().unwrap_or(0),
                title: it["title"].as_str().unwrap_or("").to_string(),
                author: it["user"]["login"].as_str().unwrap_or("").to_string(),
                url: it["html_url"].as_str().unwrap_or("").to_string(),
                head_sha: it["head"]["sha"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    /// 拉取 PR 的 diff 文本（Accept: application/vnd.github.diff）。
    pub async fn fetch_diff(&self, repo: &str, number: i64) -> Result<String> {
        let url = format!("{API}/repos/{repo}/pulls/{number}");
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github.diff")
            .send()
            .await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub diff {}: {}", resp.status(), repo);
        }
        Ok(resp.text().await?)
    }
}
