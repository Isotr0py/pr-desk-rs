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

/// GitHub 通知（/notifications）：kind 为 subject.type（PullRequest/Issue/Release/…）。
/// url 为 subject.url（API URL，UI 侧换成 HTML URL 后打开浏览器）。
#[derive(Debug, Clone)]
pub struct Notification {
    pub id: String,
    pub repo: String,
    pub title: String,
    pub kind: String,
    pub reason: String,
    pub updated_at: String,
    pub unread: bool,
    pub url: String,
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

    /// 拉取通知列表（all=true 连已读也拉，否则只返回未读；需要 token，匿名调用会 401）。
    pub async fn fetch_notifications(&self, all: bool) -> Result<Vec<Notification>> {
        let url = if all {
            format!("{API}/notifications?per_page=50&all=true")
        } else {
            format!("{API}/notifications?per_page=50")
        };
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub notifications: {}", resp.status());
        }
        let body: Vec<Value> = resp.json().await?;
        Ok(body
            .into_iter()
            .map(|it| Notification {
                id: it["id"].as_str().unwrap_or("").to_string(),
                repo: it["repository"]["full_name"].as_str().unwrap_or("").to_string(),
                title: it["subject"]["title"].as_str().unwrap_or("").to_string(),
                kind: it["subject"]["type"].as_str().unwrap_or("").to_string(),
                reason: it["reason"].as_str().unwrap_or("").to_string(),
                updated_at: it["updated_at"].as_str().unwrap_or("").to_string(),
                unread: it["unread"].as_bool().unwrap_or(false),
                url: it["subject"]["url"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    /// 把一条通知线程标记为已读（PATCH /notifications/threads/{id}，无 body）。
    pub async fn mark_notification_read(&self, thread_id: &str) -> Result<()> {
        let url = format!("{API}/notifications/threads/{thread_id}");
        let resp = self.client.patch(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub mark read {}: {}", resp.status(), thread_id);
        }
        Ok(())
    }

    /// 全部标记为已读（PUT /notifications，无 body）。
    pub async fn mark_all_notifications_read(&self) -> Result<()> {
        let url = format!("{API}/notifications");
        let resp = self.client.put(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub mark all read: {}", resp.status());
        }
        Ok(())
    }

    /// 拉取 PR/Issue 的会话评论（issues comments 端点对 PR 同样适用）。
    /// 返回 JSON 数组：[{author, time, text}]，body 截断到 2000 字符。
    pub async fn fetch_pr_convo(&self, repo: &str, number: i64) -> Result<Vec<Value>> {
        let url = format!("{API}/repos/{repo}/issues/{number}/comments?per_page=100");
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub comments {} #{}: {}", resp.status(), repo, number);
        }
        let body: Vec<Value> = resp.json().await?;
        Ok(body
            .into_iter()
            .map(|c| {
                let text = c["body"].as_str().unwrap_or("");
                serde_json::json!({
                    "author": c["user"]["login"].as_str().unwrap_or(""),
                    "time": c["created_at"].as_str().unwrap_or(""),
                    "text": text.chars().take(2000).collect::<String>(),
                })
            })
            .collect())
    }

    /// 拉取 PR 的文件清单（仅 PullRequest 有；返回 [{path, adds, dels}]）。
    pub async fn fetch_pr_files(&self, repo: &str, number: i64) -> Result<Vec<Value>> {
        let url = format!("{API}/repos/{repo}/pulls/{number}/files?per_page=100");
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub files {} #{}: {}", resp.status(), repo, number);
        }
        let body: Vec<Value> = resp.json().await?;
        Ok(body
            .into_iter()
            .map(|f| {
                serde_json::json!({
                    "path": f["filename"].as_str().unwrap_or(""),
                    "adds": f["additions"].as_i64().unwrap_or(0),
                    "dels": f["deletions"].as_i64().unwrap_or(0),
                })
            })
            .collect())
    }
}
