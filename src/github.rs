//! GitHub API 客户端：PR 快照、评论、评审、合并及仓库状态查询。
//! 每次响应解析 X-RateLimit-Remaining 写入共享状态（rate_limit_handle），供 UI 状态栏展示。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

const API: &str = "https://api.github.com";

fn immutable_file_cache_path(
    root: &Path,
    repo: &str,
    path: &str,
    git_ref: &str,
) -> Option<PathBuf> {
    if git_ref.len() != 40 || !git_ref.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let key = format!("{repo}\0{git_ref}\0{path}");
    Some(root.join(format!("{:x}.bin", Sha256::digest(key.as_bytes()))))
}

fn write_immutable_file_cache(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("系统时钟不可用")?
        .as_nanos();
    let part = path.with_extension(format!("part-{}-{nanos}", std::process::id()));
    std::fs::write(&part, bytes)?;
    if path.exists() {
        let _ = std::fs::remove_file(&part);
        return Ok(());
    }
    if let Err(error) = std::fs::rename(&part, path) {
        let _ = std::fs::remove_file(&part);
        return Err(error.into());
    }
    Ok(())
}

/// URL 路径段转义（percent-encode，保留 RFC 3986 unreserved 字符）。
/// GitHub login 名义上只含 [A-Za-z0-9-]，但 bot 账号（如 claude[bot]）带方括号，
/// 未编码直接拼进 /users/{login} 会被代理解析坏或被 GitHub 400。
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

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

#[derive(Clone)]
pub struct GitHub {
    client: reqwest::Client,
    api_base: String,
    /// 最近一次响应的 X-RateLimit-Remaining（worker 写入，UI 状态栏读取）。
    rate_limit: Arc<Mutex<Option<u64>>>,
}

/// 同一 base/head 下采集的 PR 元信息与 diff；captured_at 为 UTC RFC 3339 时间。
#[derive(Debug, Clone)]
pub struct PrSnapshot {
    pub repo: String,
    pub number: i64,
    pub meta: Value,
    pub base_sha: String,
    pub head_sha: String,
    pub diff: String,
    pub captured_at: String,
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

impl GitHub {
    /// proxy：HTTP/SOCKS5 代理地址（空 = 直连）；非法时返回带中文说明的错误，
    /// 调用方按现有降级路径处理（状态栏 gh_client_init_failed 提示，gh = None）。
    pub fn new(token: Option<String>, proxy: &str) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Accept", "application/vnd.github+json".parse()?);
        headers.insert("X-GitHub-Api-Version", "2022-11-28".parse()?);
        if let Some(t) = token {
            headers.insert(
                "Authorization",
                format!("Bearer {t}").parse().context("token 含非法字符")?,
            );
        }
        let client = crate::http_client_builder(proxy, std::time::Duration::from_secs(30))?
            .default_headers(headers)
            .build()?;
        Ok(Self {
            client,
            api_base: API.to_string(),
            rate_limit: Arc::new(Mutex::new(None)),
        })
    }

    #[cfg(test)]
    fn new_for_test(api_base: &str) -> Self {
        let mut client = Self::new(None, "").unwrap();
        client.api_base = api_base.trim_end_matches('/').to_string();
        client
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
        let cache_path =
            immutable_file_cache_path(&crate::data_dir().join("ocr-files"), repo, path, git_ref);
        if let Some(bytes) = cache_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
        {
            return Ok(Some(bytes));
        }
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
            anyhow::bail!(
                "GitHub contents {}:{} @{}: {}",
                repo,
                path,
                git_ref,
                resp.status()
            );
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
        if let Some(path) = cache_path {
            let _ = write_immutable_file_cache(&path, &bytes);
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

    /// 拉取一致 PR 快照：meta → diff → meta，base/head 变化时最多重试三轮。
    pub async fn fetch_pr_snapshot(&self, repo: &str, number: i64) -> Result<PrSnapshot> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let first_meta = self.fetch_pr_meta(repo, number).await?;
            let (first_base, first_head) = pr_snapshot_refs(&first_meta)
                .with_context(|| format!("GitHub PR snapshot {repo} #{number}"))?;
            let diff = self.fetch_diff(repo, number).await?;
            let final_meta = self.fetch_pr_meta(repo, number).await?;
            let (base_sha, head_sha) = pr_snapshot_refs(&final_meta)
                .with_context(|| format!("GitHub PR snapshot {repo} #{number}"))?;
            if first_base == base_sha && first_head == head_sha {
                return Ok(PrSnapshot {
                    repo: repo.to_string(),
                    number,
                    meta: final_meta,
                    base_sha,
                    head_sha,
                    diff,
                    captured_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                });
            }
            if attempt == 3 {
                anyhow::bail!(
                    "GitHub PR snapshot {repo} #{number}: base/head SHA 连续 3 轮采样不一致，无法获得稳定快照"
                );
            }
        }
    }

    /// 发布 Issue/PR 评论（POST /repos/{repo}/issues/{number}/comments）。
    pub async fn post_issue_comment(&self, repo: &str, number: i64, body: &str) -> Result<()> {
        let url = format!("{}/repos/{repo}/issues/{number}/comments", self.api_base);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;
        self.record_rate_limit(resp.headers());
        check_write_response(resp, &format!("GitHub issue comment {repo} #{number}")).await
    }

    /// 提交 PR 评审；event 仅允许 COMMENT、APPROVE、REQUEST_CHANGES。
    pub async fn submit_pr_review(
        &self,
        repo: &str,
        number: i64,
        body: &str,
        event: &str,
        expected_head_sha: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(event, "COMMENT" | "APPROVE" | "REQUEST_CHANGES"),
            "GitHub review event 仅允许 COMMENT/APPROVE/REQUEST_CHANGES，收到 {event:?}"
        );
        anyhow::ensure!(
            !expected_head_sha.trim().is_empty(),
            "GitHub review expected head SHA 不能为空"
        );
        let url = format!("{}/repos/{repo}/pulls/{number}/reviews", self.api_base);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "body": body,
                "event": event,
                "commit_id": expected_head_sha,
            }))
            .send()
            .await?;
        self.record_rate_limit(resp.headers());
        check_write_response(resp, &format!("GitHub PR review {repo} #{number}")).await
    }

    /// 按预期 head SHA 合并 PR；method 仅允许 merge、squash、rebase。
    pub async fn merge_pr(
        &self,
        repo: &str,
        number: i64,
        expected_head_sha: &str,
        method: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(method, "merge" | "squash" | "rebase"),
            "GitHub merge method 仅允许 merge/squash/rebase，收到 {method:?}"
        );
        let url = format!("{}/repos/{repo}/pulls/{number}/merge", self.api_base);
        let resp = self
            .client
            .put(&url)
            .json(&serde_json::json!({
                "sha": expected_head_sha,
                "merge_method": method,
            }))
            .send()
            .await?;
        self.record_rate_limit(resp.headers());
        check_write_response(resp, &format!("GitHub merge {repo} #{number}")).await
    }

    /// 拉取通知列表（all=true 连已读也拉，否则只返回未读；需要 token，匿名调用会 401）。
    pub async fn fetch_notifications(&self, all: bool) -> Result<Vec<Notification>> {
        Ok(self.fetch_notifications_page(all, 1).await?.0)
    }

    /// 分页拉取通知：page 从 1 开始；返回 (本页通知, 是否还有下一页)。
    /// has_more 优先看 Link 头的 rel="next"，缺头时退化为「本页满 50 条」。
    pub async fn fetch_notifications_page(
        &self,
        all: bool,
        page: u32,
    ) -> Result<(Vec<Notification>, bool)> {
        let url = if all {
            format!("{API}/notifications?per_page=50&all=true&page={page}")
        } else {
            format!("{API}/notifications?per_page=50&page={page}")
        };
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub notifications: {}", resp.status());
        }
        let link_next = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.contains("rel=\"next\""))
            .unwrap_or(false);
        let body: Vec<Value> = resp.json().await?;
        let has_more = link_next || body.len() == 50;
        let items = body
            .into_iter()
            .map(|it| Notification {
                id: it["id"].as_str().unwrap_or("").to_string(),
                repo: it["repository"]["full_name"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                title: it["subject"]["title"].as_str().unwrap_or("").to_string(),
                kind: it["subject"]["type"].as_str().unwrap_or("").to_string(),
                reason: it["reason"].as_str().unwrap_or("").to_string(),
                updated_at: it["updated_at"].as_str().unwrap_or("").to_string(),
                unread: it["unread"].as_bool().unwrap_or(false),
                url: it["subject"]["url"].as_str().unwrap_or("").to_string(),
            })
            .collect();
        Ok((items, has_more))
    }

    /// 批量查询 PR/Issue 的 open/closed/merged 状态（GraphQL，每 100 块一次请求）。
    /// items: (repo "owner/name", number, is_pr)；返回 "owner/name#number" → "open"/"closed"/"merged"。
    /// 查不到的条目（已删除/无权限）不出现在结果里，调用方按「无状态」处理。
    pub async fn fetch_subject_states(
        &self,
        items: &[(String, i64, bool)],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut out = std::collections::HashMap::new();
        for chunk in items.chunks(100) {
            let query = build_states_query(chunk)?;
            let resp = self
                .client
                .post(format!("{API}/graphql"))
                .json(&serde_json::json!({ "query": query }))
                .send()
                .await?;
            self.record_rate_limit(resp.headers());
            if !resp.status().is_success() {
                anyhow::bail!("GitHub graphql: {}", resp.status());
            }
            let body: Value = resp.json().await?;
            // GraphQL 允许部分字段报错；只有 data 整体缺失才算失败
            if body["data"].is_null() {
                anyhow::bail!("GitHub graphql errors: {}", body["errors"]);
            }
            parse_states_response(&body, chunk, &mut out);
        }
        Ok(out)
    }

    /// 拉用户的公开组织成员关系（GET /users/{login}/orgs）→ org login 列表。
    pub async fn fetch_user_orgs(&self, login: &str) -> Result<Vec<String>> {
        let url = format!("{API}/users/{}/orgs", encode_path_segment(login));
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            anyhow::bail!("GitHub user orgs {}: {}", login, resp.status());
        }
        let body: Vec<Value> = resp.json().await?;
        Ok(body
            .iter()
            .filter_map(|o| o["login"].as_str().map(|s| s.to_string()))
            .collect())
    }

    /// login → avatar_url（GET /users/{login}；org 账号同路径）。
    pub async fn fetch_avatar_url(&self, login: &str) -> Result<Option<String>> {
        let url = format!("{API}/users/{}", encode_path_segment(login));
        let resp = self.client.get(&url).send().await?;
        self.record_rate_limit(resp.headers());
        if !resp.status().is_success() {
            return Ok(None);
        }
        let v: Value = resp.json().await?;
        Ok(v["avatar_url"].as_str().map(|s| {
            // avatar_url 可能已带 query（如 ?u=…）：已有 ? 用 & 续参数，否则开新 query
            let sep = if s.contains('?') { '&' } else { '?' };
            format!("{s}{sep}s=64")
        }))
    }

    /// 下载任意 URL 的原始字节（头像 CDN 用；无鉴权需求）。
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.client.get(url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("GET {}: {}", url, resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
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

fn pr_snapshot_refs(meta: &Value) -> Result<(String, String)> {
    let base = meta["base"]["sha"].as_str().unwrap_or("").trim();
    let head = meta["head"]["sha"].as_str().unwrap_or("").trim();
    anyhow::ensure!(!base.is_empty(), "PR 元信息 base.sha 为空");
    anyhow::ensure!(!head.is_empty(), "PR 元信息 head.sha 为空");
    Ok((base.to_string(), head.to_string()))
}

async fn check_write_response(resp: reqwest::Response, action: &str) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let summary = match resp.text().await {
        Ok(body) => {
            let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut chars = collapsed.chars();
            let summary = chars.by_ref().take(500).collect::<String>();
            if collapsed.is_empty() {
                "<空响应>".to_string()
            } else if chars.next().is_some() {
                format!("{summary}…")
            } else {
                summary
            }
        }
        Err(error) => format!("<响应读取失败: {error}>"),
    };
    anyhow::bail!("{action}: {status}: {summary}")
}

/// owner/name 的字符白名单（GitHub 合法字符）：repo 名来自远程 API 响应，
/// 要 format! 进 GraphQL 查询字符串，白名单校验防注入。
fn valid_repo_part(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// 解析 GitHub PR 页面地址，返回 owner/repo 与正整数编号。
pub fn parse_pr_url(input: &str) -> Option<(String, i64)> {
    let url = reqwest::Url::parse(input.trim()).ok()?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("github.com") | Some("www.github.com"))
    {
        return None;
    }
    let mut parts = url.path_segments()?;
    let owner = parts.next()?;
    let repo = parts.next()?;
    if !valid_repo_part(owner) || !valid_repo_part(repo) || parts.next()? != "pull" {
        return None;
    }
    let number = parts.next()?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number = number.parse::<i64>().ok().filter(|n| *n > 0)?;
    if parts
        .next()
        .is_some_and(|s| !matches!(s, "" | "files" | "commits" | "checks" | "changes"))
    {
        return None;
    }
    Some((format!("{owner}/{repo}"), number))
}

/// 构造批量状态查询的 GraphQL：每个条目一个 repository 别名块（r{i}），
/// 内嵌 pullRequest/issue 字段（n{i}）。owner/name 先过白名单，非法直接报错。
fn build_states_query(items: &[(String, i64, bool)]) -> Result<String> {
    let mut q = String::from("query {");
    for (i, (repo, number, is_pr)) in items.iter().enumerate() {
        let (owner, name) = repo.split_once('/').unwrap_or(("", repo.as_str()));
        if !valid_repo_part(owner) || !valid_repo_part(name) {
            anyhow::bail!("GraphQL 状态查询：非法仓库名 {repo:?}");
        }
        let field = if *is_pr { "pullRequest" } else { "issue" };
        q.push_str(&format!(
            "r{i}: repository(owner: \"{owner}\", name: \"{name}\") {{ n{i}: {field}(number: {number}) {{ state }} }}"
        ));
    }
    q.push('}');
    Ok(q)
}

/// 解析 GraphQL 响应：按条目下标回读 r{i}/n{i}.state，小写化后写入 "repo#number"。
/// PR 的 MERGED 原样保留为 "merged"；条目为 null（已删除）时跳过。
fn parse_states_response(
    body: &Value,
    items: &[(String, i64, bool)],
    out: &mut std::collections::HashMap<String, String>,
) {
    for (i, (repo, number, _)) in items.iter().enumerate() {
        let state = body["data"][format!("r{i}")][format!("n{i}")]["state"]
            .as_str()
            .map(|s| s.to_ascii_lowercase());
        if let Some(s) = state {
            out.insert(format!("{repo}#{number}"), s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[derive(Debug)]
    struct RecordedRequest {
        method: String,
        path: String,
        body: Value,
    }

    fn spawn_mock(
        status: u16,
        response_body: &str,
    ) -> (
        String,
        std::sync::mpsc::Receiver<RecordedRequest>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let response_body = response_body.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0_u8; 1024];
            let mut expected_len = None;
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
                if expected_len.is_none() {
                    if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        expected_len = Some(header_end + 4 + content_length);
                    }
                }
                if expected_len.is_some_and(|len| bytes.len() >= len) {
                    break;
                }
            }
            let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
            let mut request_line = headers.lines().next().unwrap().split_whitespace();
            let request = RecordedRequest {
                method: request_line.next().unwrap().to_string(),
                path: request_line.next().unwrap().to_string(),
                body: serde_json::from_slice(
                    &bytes[header_end + 4..expected_len.unwrap_or(bytes.len())],
                )
                .unwrap(),
            };
            tx.send(request).unwrap();
            let reason = match status {
                200 => "OK",
                201 => "Created",
                422 => "Unprocessable Entity",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{addr}"), rx, handle)
    }

    #[test]
    fn snapshot_refs_require_nonempty_base_and_head() {
        let meta = serde_json::json!({
            "base": { "sha": " base-sha " },
            "head": { "sha": " head-sha " }
        });
        assert_eq!(
            pr_snapshot_refs(&meta).unwrap(),
            ("base-sha".to_string(), "head-sha".to_string())
        );
        for meta in [
            serde_json::json!({"base": {"sha": ""}, "head": {"sha": "head"}}),
            serde_json::json!({"base": {"sha": "base"}, "head": {"sha": "  "}}),
            serde_json::json!({"base": {}, "head": {}}),
        ] {
            assert!(pr_snapshot_refs(&meta).is_err(), "应拒绝 {meta}");
        }
    }

    #[tokio::test]
    async fn issue_comment_uses_expected_request() {
        let (api_base, rx, server) = spawn_mock(201, "{}");
        GitHub::new_for_test(&api_base)
            .post_issue_comment("octo/repo", 42, "looks good")
            .await
            .unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/octo/repo/issues/42/comments");
        assert_eq!(request.body, serde_json::json!({"body": "looks good"}));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn pr_review_uses_expected_request() {
        let (api_base, rx, server) = spawn_mock(200, "{}");
        GitHub::new_for_test(&api_base)
            .submit_pr_review(
                "octo/repo",
                42,
                "needs work",
                "REQUEST_CHANGES",
                "expected-head-sha",
            )
            .await
            .unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/octo/repo/pulls/42/reviews");
        assert_eq!(
            request.body,
            serde_json::json!({
                "body": "needs work",
                "event": "REQUEST_CHANGES",
                "commit_id": "expected-head-sha"
            })
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn merge_uses_expected_head_and_method() {
        let (api_base, rx, server) = spawn_mock(200, "{}");
        GitHub::new_for_test(&api_base)
            .merge_pr("octo/repo", 42, "expected-head-sha", "squash")
            .await
            .unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(request.method, "PUT");
        assert_eq!(request.path, "/repos/octo/repo/pulls/42/merge");
        assert_eq!(
            request.body,
            serde_json::json!({"sha": "expected-head-sha", "merge_method": "squash"})
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn write_methods_reject_invalid_parameters() {
        let github = GitHub::new_for_test("http://127.0.0.1:9");
        for event in ["comment", "APPROVED", ""] {
            let error = github
                .submit_pr_review("octo/repo", 42, "body", event, "head")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("COMMENT/APPROVE/REQUEST_CHANGES"));
        }
        for head in ["", "   "] {
            let error = github
                .submit_pr_review("octo/repo", 42, "body", "COMMENT", head)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("head SHA 不能为空"));
        }
        for method in ["MERGE", "fast-forward", ""] {
            let error = github
                .merge_pr("octo/repo", 42, "head", method)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("merge/squash/rebase"));
        }
    }

    #[tokio::test]
    async fn write_errors_include_response_summary() {
        let (api_base, _rx, server) = spawn_mock(422, "{\"message\":\"Validation Failed\"}");
        let error = GitHub::new_for_test(&api_base)
            .merge_pr("octo/repo", 42, "stale-head", "merge")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("422"));
        assert!(error.contains("Validation Failed"));
        assert!(error.contains("GitHub merge octo/repo #42"));
        server.join().unwrap();
    }

    #[test]
    fn encode_path_segment_encodes_non_unreserved() {
        // GitHub 用户名片段只需 unreserved 原样通过
        assert_eq!(encode_path_segment("octocat"), "octocat");
        assert_eq!(encode_path_segment("some-user_1.2~3"), "some-user_1.2~3");
        // bot 名的方括号必须编码（未编码拼进 /users/{login} 路径会被拒绝/解析坏）
        assert_eq!(encode_path_segment("claude[bot]"), "claude%5Bbot%5D");
        // 非 ASCII 按 UTF-8 逐字节编码
        assert_eq!(encode_path_segment("中文"), "%E4%B8%AD%E6%96%87");
        // 斜杠不得穿透成路径分隔
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(encode_path_segment(""), "");
    }

    #[test]
    fn parse_pr_url_accepts_github_pr_pages() {
        for (url, expected) in [
            (
                "https://github.com/octocat/Hello-World/pull/42",
                ("octocat/Hello-World", 42),
            ),
            (
                "  https://www.github.com/A.b/C-d_e.f/pull/7/?tab=diff#discussion  ",
                ("A.b/C-d_e.f", 7),
            ),
            (
                "https://github.com/octocat/Hello-World/pull/123/files",
                ("octocat/Hello-World", 123),
            ),
            (
                "https://github.com/octocat/Hello-World/pull/123/commits/deadbeef",
                ("octocat/Hello-World", 123),
            ),
            (
                "https://github.com/octocat/Hello-World/pull/123/checks",
                ("octocat/Hello-World", 123),
            ),
        ] {
            assert_eq!(
                parse_pr_url(url),
                Some((expected.0.to_string(), expected.1)),
                "应接受 {url}"
            );
        }
    }

    #[test]
    fn parse_pr_url_rejects_non_pr_and_malformed_urls() {
        for url in [
            "",
            "not a url",
            "http://github.com/octocat/Hello-World/pull/42",
            "https://gitlab.com/octocat/Hello-World/pull/42",
            "https://github.com/octocat/Hello-World/issues/42",
            "https://github.com/octocat/Hello-World/pull/",
            "https://github.com/octocat/Hello-World/pull/0",
            "https://github.com/octocat/Hello-World/pull/-1",
            "https://github.com/octocat/Hello-World/pull/+1",
            "https://github.com/octocat/Hello-World/pull/abc",
            "https://github.com/octocat/Hello-World/pull/42/unknown",
            "https://github.com/octocat//pull/42",
            "https://github.com/octocat/Hello-World/pull/999999999999999999999",
        ] {
            assert_eq!(parse_pr_url(url), None, "应拒绝 {url}");
        }
    }

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
        let runs = vec![
            run("completed", Some("success")),
            run("completed", Some("skipped")),
        ];
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

    #[test]
    fn states_query_embeds_repo_and_number() {
        let items = vec![
            ("vllm-project/vllm".to_string(), 123, true),
            ("o/r".to_string(), 7, false),
        ];
        let q = build_states_query(&items).unwrap();
        assert!(q.contains("r0: repository(owner: \"vllm-project\", name: \"vllm\")"));
        assert!(q.contains("n0: pullRequest(number: 123)"));
        assert!(q.contains("n1: issue(number: 7)"));
        assert!(q.starts_with("query {") && q.ends_with('}'));
    }

    #[test]
    fn states_query_rejects_unsafe_repo_names() {
        for bad in ["o/r\"; }", "o\"]/x", "o/r or true", "/r", "o/", "o/r;s"] {
            let items = vec![(bad.to_string(), 1, true)];
            assert!(build_states_query(&items).is_err(), "应拒绝 {bad:?}");
        }
        // 合法字符全集可通过
        let items = vec![("A.b/C-d_e.f".to_string(), 1, true)];
        assert!(build_states_query(&items).is_ok());
    }

    #[test]
    fn states_response_maps_and_skips_null() {
        let items = vec![
            ("o/r".to_string(), 1, true),
            ("o/r".to_string(), 2, true),
            ("o/r".to_string(), 3, false),
        ];
        let body = serde_json::json!({
            "data": {
                "r0": { "n0": { "state": "MERGED" } },
                "r1": { "n1": null },
                "r2": { "n2": { "state": "CLOSED" } }
            }
        });
        let mut out = std::collections::HashMap::new();
        parse_states_response(&body, &items, &mut out);
        assert_eq!(out.get("o/r#1").map(String::as_str), Some("merged"));
        assert_eq!(out.get("o/r#3").map(String::as_str), Some("closed"));
        assert!(!out.contains_key("o/r#2"));
    }

    #[test]
    fn immutable_file_cache_is_sha_scoped_and_atomic() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pr-desk-cache-test-{}-{unique}",
            std::process::id()
        ));
        let sha = "a".repeat(40);
        let path = immutable_file_cache_path(&root, "o/r", "src/lib.rs", &sha).unwrap();
        let other = immutable_file_cache_path(&root, "o/r", "src/other.rs", &sha).unwrap();
        assert_ne!(path, other);
        assert!(immutable_file_cache_path(&root, "o/r", "src/lib.rs", "main").is_none());
        assert!(immutable_file_cache_path(&root, "o/r", "src/lib.rs", "abc123").is_none());

        write_immutable_file_cache(&path, b"fn answer() -> u32 { 42 }\n").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"fn answer() -> u32 { 42 }\n"
        );
        write_immutable_file_cache(&path, b"fn answer() -> u32 { 42 }\n").unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
