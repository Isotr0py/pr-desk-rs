//! GitHub API 客户端（只读）：open PR 列表 + PR diff（带 Accept: diff header）。
//! 每次响应解析 X-RateLimit-Remaining 写入共享状态（rate_limit_handle），供 UI 状态栏展示。

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::Value;

const API: &str = "https://api.github.com";

/// 聚合 check runs → ci_status：任一失败类 conclusion → "failure"；
/// 否则任一未完成（status 为 queued/in_progress/waiting 或 conclusion 为空）→ "pending"；
/// 空列表 → "none"；全部成功 → "success"。
/// runs 为 check-runs API 原始条目（读 status/conclusion 字段）。
pub fn aggregate_ci(runs: &[Value]) -> String {
    if runs.is_empty() {
        return "none".into();
    }
    let has_failure = runs.iter().any(|r| {
        matches!(
            r["conclusion"].as_str(),
            Some("failure") | Some("timed_out") | Some("cancelled") | Some("action_required")
        )
    });
    if has_failure {
        return "failure".into();
    }
    let has_pending = runs.iter().any(|r| {
        r["conclusion"].as_str().map_or(true, |c| c.is_empty())
            || matches!(
                r["status"].as_str(),
                Some("queued") | Some("in_progress") | Some("waiting")
            )
    });
    if has_pending {
        "pending".into()
    } else {
        "success".into()
    }
}

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

    /// 拉取单个文件在指定 ref（sha/分支）下的原始内容
    /// （GET /repos/{repo}/contents/{path}?ref={git_ref}，Accept: raw）。
    /// 404 / 超过 1MB / 前 8000 字节含 NUL（二进制）→ Ok(None)，调用方跳过。
    pub async fn fetch_file_content(
        &self,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<Vec<u8>>> {
        let mut url = reqwest::Url::parse(&format!("{API}/repos/{repo}/contents/"))?;
        for seg in path.split('/') {
            url.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("URL 拼接失败"))?
                .push(seg);
        }
        url.query_pairs_mut().append_pair("ref", git_ref);
        let resp = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github.raw")
            .send()
            .await?;
        self.record_rate_limit(resp.headers());
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            anyhow::bail!("GitHub contents {}:{} @{}: {}", repo, path, git_ref, resp.status());
        }
        // 超过 1MB 直接跳过（content-length 先知，兜底再按实际体判断）
        if resp.content_length().is_some_and(|n| n > 1024 * 1024) {
            return Ok(None);
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > 1024 * 1024 {
            return Ok(None);
        }
        let head = &bytes[..bytes.len().min(8000)];
        if head.contains(&0) {
            return Ok(None);
        }
        Ok(Some(bytes.to_vec()))
    }

    /// 拉取单个 PR 的元信息（GET /repos/{repo}/pulls/{number}，返回原始 JSON）。
    pub async fn fetch_pr_meta(&self, repo: &str, number: i64) -> Result<Value> {
        let url = format!("{API}/repos/{repo}/pulls/{number}");
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub PR meta {} #{}: {}", resp.status(), repo, number);
        }
        Ok(resp.json().await?)
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

    /// 拉取 PR 的评审列表（GET /repos/{repo}/pulls/{number}/reviews）。
    /// 返回 [{author, state(小写), time(submitted_at), text(body 截 2000 字符)}]。
    pub async fn fetch_pr_reviews(&self, repo: &str, number: i64) -> Result<Vec<Value>> {
        let url = format!("{API}/repos/{repo}/pulls/{number}/reviews?per_page=100");
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub reviews {} #{}: {}", resp.status(), repo, number);
        }
        let body: Vec<Value> = resp.json().await?;
        Ok(body
            .into_iter()
            .map(|r| {
                let text = r["body"].as_str().unwrap_or("");
                serde_json::json!({
                    "author": r["user"]["login"].as_str().unwrap_or(""),
                    "state": r["state"].as_str().unwrap_or("").to_lowercase(),
                    "time": r["submitted_at"].as_str().unwrap_or(""),
                    "text": text.chars().take(2000).collect::<String>(),
                })
            })
            .collect())
    }

    /// 拉取指定 commit 的 check runs（GET /repos/{repo}/commits/{sha}/check-runs）。
    /// 返回 (聚合 ci_status, [{name, conclusion}])；条目 conclusion 为空时取 status。
    pub async fn fetch_pr_checks(&self, repo: &str, sha: &str) -> Result<(String, Vec<Value>)> {
        let url = format!("{API}/repos/{repo}/commits/{sha}/check-runs?per_page=100");
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub check-runs {} @{}: {}", resp.status(), repo, sha);
        }
        let body: Value = resp.json().await?;
        let runs = body["check_runs"].as_array().cloned().unwrap_or_default();
        let ci = aggregate_ci(&runs);
        let items = runs
            .iter()
            .map(|c| {
                let conclusion = c["conclusion"].as_str().unwrap_or("");
                serde_json::json!({
                    "name": c["name"].as_str().unwrap_or(""),
                    "conclusion": if conclusion.is_empty() {
                        c["status"].as_str().unwrap_or("")
                    } else {
                        conclusion
                    },
                })
            })
            .collect();
        Ok((ci, items))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, conclusion: Option<&str>) -> Value {
        match conclusion {
            Some(c) => serde_json::json!({"name": "n", "status": status, "conclusion": c}),
            None => serde_json::json!({"name": "n", "status": status, "conclusion": null}),
        }
    }

    #[test]
    fn aggregate_ci_empty_is_none() {
        assert_eq!(aggregate_ci(&[]), "none");
    }

    #[test]
    fn aggregate_ci_success() {
        let runs = vec![run("completed", Some("success")), run("completed", Some("skipped"))];
        assert_eq!(aggregate_ci(&runs), "success");
    }

    #[test]
    fn aggregate_ci_failure_variants() {
        for c in ["failure", "timed_out", "cancelled", "action_required"] {
            let runs = vec![run("completed", Some("success")), run("completed", Some(c))];
            assert_eq!(aggregate_ci(&runs), "failure", "conclusion={c}");
        }
    }

    #[test]
    fn aggregate_ci_pending_on_unfinished_status() {
        for s in ["queued", "in_progress", "waiting"] {
            let runs = vec![run("completed", Some("success")), run(s, None)];
            assert_eq!(aggregate_ci(&runs), "pending", "status={s}");
        }
    }

    #[test]
    fn aggregate_ci_pending_on_null_conclusion() {
        let runs = vec![run("completed", None)];
        assert_eq!(aggregate_ci(&runs), "pending");
    }

    #[test]
    fn aggregate_ci_failure_beats_pending() {
        let runs = vec![run("in_progress", None), run("completed", Some("failure"))];
        assert_eq!(aggregate_ci(&runs), "failure");
    }
}
