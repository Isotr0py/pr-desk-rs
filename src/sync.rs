//! 同步引擎：sync_pull 增量拉取 → 本地 upsert；outbox 改动 → sync_push；
//! 冲突按服务器返回处理（服务器赢：把服务器值写回本地）。
//!
//! 拆分为纯 HTTP（pull_body/push_body）与纯 DB（apply_pull/apply_push_result）两层，
//! 调用方（worker）在两次锁之间不跨 await 持有 DB 锁。

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime};
use rusqlite::params;
use serde_json::Value;

use crate::config::Config;
use crate::db::Db;

/// 最近一次同步成功时间的共享状态（worker 写入，UI 状态栏读取）。
pub type SharedLastSync = Arc<Mutex<Option<NaiveDateTime>>>;

pub fn shared_last_sync() -> SharedLastSync {
    Arc::new(Mutex::new(None))
}

/// 同步成功后调用：把当前本地时间记入共享状态。
pub fn mark_synced(state: &SharedLastSync) {
    *state.lock().unwrap() = Some(Local::now().naive_local());
}

/// 「已同步 27m 前」状态栏片段；从未同步过返回 None。
pub fn last_sync_text(state: &SharedLastSync) -> Option<String> {
    let ts = *state.lock().unwrap();
    ts.map(|ts| format!("已同步 {} 前", rel_short(ts)))
}

/// 相对时间简写："27m" / "2h" / "3d"；未来时间归零为 "0m"。
fn rel_short(ts: NaiveDateTime) -> String {
    let mins = (Local::now().naive_local() - ts).num_minutes().max(0);
    if mins < 60 {
        format!("{mins}m")
    } else if mins < 24 * 60 {
        format!("{}h", mins / 60)
    } else {
        format!("{}d", mins / (24 * 60))
    }
}

pub struct SyncClient {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl SyncClient {
    pub fn new(cfg: &Config) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: format!(
                "{}/api/v1/plugins/extensions/astrbot_plugin_github_pr_review",
                cfg.server_url.trim_end_matches('/')
            ),
            token: cfg.sync_token.clone(),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.token.is_empty()
    }

    /// GET sync_pull（since 为 None = 全量）。
    pub async fn pull_body(&self, since: Option<String>) -> Result<Value> {
        let mut url = format!("{}/sync_pull", self.base);
        if let Some(s) = &since {
            url = format!("{url}?since={s}");
        }
        let resp = self
            .client
            .get(&url)
            .header("X-Sync-Token", &self.token)
            .send()
            .await?;
        check_status(&resp)?;
        Ok(resp.json().await?)
    }

    /// POST sync_push。
    pub async fn push_body(&self, prs_updates: Vec<Value>) -> Result<Value> {
        let body = serde_json::json!({
            "prs_updates": prs_updates,
            "todos_updates": [],
            "todos_new": [],
        });
        let resp = self
            .client
            .post(format!("{}/sync_push", self.base))
            .header("X-Sync-Token", &self.token)
            .json(&body)
            .send()
            .await?;
        check_status(&resp)?;
        Ok(resp.json().await?)
    }
}

fn check_status(resp: &reqwest::Response) -> Result<()> {
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("sync token 无效或 sync 未启用（401）");
    }
    resp.error_for_status_ref().context("sync HTTP 错误")?;
    Ok(())
}

/// 应用 pull 响应到本地。返回 (prs, reviews, todos) 行数。
pub fn apply_pull(db: &Db, body: &Value) -> Result<(usize, usize, usize)> {
    let prs = body["prs"].as_array().cloned().unwrap_or_default();
    let reviews = body["reviews"].as_array().cloned().unwrap_or_default();
    let todos = body["todos"].as_array().cloned().unwrap_or_default();
    for p in &prs {
        db.upsert_pr(p)?;
    }
    for r in &reviews {
        db.upsert_review(r)?;
    }
    for t in &todos {
        db.upsert_todo(t)?;
    }
    if let Some(ts) = body["server_time"].as_str() {
        db.set_cursor(ts)?;
    }
    Ok((prs.len(), reviews.len(), todos.len()))
}

/// 把 outbox 条目按 (repo, number) 合并为 prs_updates（同字段后者覆盖，时间取最新）。
pub fn merge_outbox(outbox: &[(i64, Value)]) -> (Vec<Value>, Vec<i64>) {
    let mut merged: std::collections::BTreeMap<(String, i64), Value> = Default::default();
    let mut ids = Vec::new();
    for (id, payload) in outbox {
        ids.push(*id);
        let key = (
            payload["repo"].as_str().unwrap_or("").to_string(),
            payload["number"].as_i64().unwrap_or(0),
        );
        let entry = merged.entry(key).or_insert_with(|| {
            serde_json::json!({"repo": payload["repo"], "number": payload["number"]})
        });
        if let Some(obj) = payload.as_object() {
            for (k, v) in obj {
                if k != "repo" && k != "number" {
                    entry[k] = v.clone();
                }
            }
        }
    }
    (merged.into_values().collect(), ids)
}

#[derive(Debug, Default, PartialEq)]
pub struct PushSummary {
    pub applied: usize,
    pub conflicts: usize,
}

/// 处理 sync_push 响应：冲突项把服务器值写回本地（服务器赢，不再入队）。
/// 纯逻辑（入参为响应 JSON），便于单测。
pub fn apply_push_result(db: &Db, result: &Value) -> Result<PushSummary> {
    let applied = result["applied"].as_array().map(|a| a.len()).unwrap_or(0);
    let mut conflicts = 0;
    for c in result["conflicts"].as_array().cloned().unwrap_or_default() {
        conflicts += 1;
        let repo = c["repo"].as_str().unwrap_or("").to_string();
        let number = c["number"].as_i64().unwrap_or(0);
        if repo.is_empty() || number == 0 {
            continue;
        }
        if let Some(values) = c["server_values"].as_object() {
            for (field, value) in values {
                apply_server_value(db, &repo, number, field, value)?;
            }
        }
    }
    Ok(PushSummary { applied, conflicts })
}

/// 把服务器字段值写回本地（不入队——这是冲突解决，不是新改动）。
fn apply_server_value(db: &Db, repo: &str, number: i64, field: &str, value: &Value) -> Result<()> {
    match field {
        "status" => db.conn.execute(
            "UPDATE prs SET status=?1 WHERE repo=?2 AND number=?3",
            params![value.as_str().unwrap_or("pending"), repo, number],
        ),
        "tags" => db.conn.execute(
            "UPDATE prs SET tags=?1 WHERE repo=?2 AND number=?3",
            params![serde_json::to_string(value)?, repo, number],
        ),
        "priority" | "on_board" | "pinned" => db.conn.execute(
            &format!("UPDATE prs SET {field}=?1 WHERE repo=?2 AND number=?3"),
            params![value.as_i64().unwrap_or(0), repo, number],
        ),
        _ => return Ok(()),
    }?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_pr(db: &Db) {
        db.upsert_pr(&serde_json::json!({
            "repo": "o/r", "number": 1, "title": "t", "status": "pending",
        }))
        .unwrap();
        db.local_pr_update("o/r", 1, "priority", &serde_json::json!(1))
            .unwrap();
    }

    #[test]
    fn conflict_server_wins() {
        let db = Db::open_memory().unwrap();
        setup_pr(&db);
        // 客户端刚把 priority 设为 1；服务器上该 PR 已被改成 3 且更新更晚 → 服务器赢
        let resp = serde_json::json!({
            "applied": [],
            "conflicts": [{
                "repo": "o/r", "number": 1, "reason": "server_newer",
                "server_values": {"priority": 3}
            }],
        });
        let summary = apply_push_result(&db, &resp).unwrap();
        assert_eq!(summary.conflicts, 1);
        let rows = db.list_prs().unwrap();
        assert_eq!(rows[0]["priority"], 3); // 本地被服务器值覆盖
    }

    #[test]
    fn applied_no_overwrite() {
        let db = Db::open_memory().unwrap();
        setup_pr(&db);
        let resp = serde_json::json!({
            "applied": [{"repo": "o/r", "number": 1, "fields": ["priority"]}],
            "conflicts": [],
        });
        let summary = apply_push_result(&db, &resp).unwrap();
        assert_eq!(summary.applied, 1);
        let rows = db.list_prs().unwrap();
        assert_eq!(rows[0]["priority"], 1); // 客户端值保留
    }

    #[test]
    fn merge_outbox_same_pr() {
        let db = Db::open_memory().unwrap();
        setup_pr(&db);
        db.local_pr_update("o/r", 1, "pinned", &serde_json::json!(1)).unwrap();
        let outbox = db.outbox_drain().unwrap();
        let (updates, ids) = merge_outbox(&outbox);
        assert_eq!(updates.len(), 1); // priority + pinned 合并为一条
        assert_eq!(updates[0]["priority"], 1);
        assert_eq!(updates[0]["pinned"], 1);
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn apply_pull_upserts() {
        let db = Db::open_memory().unwrap();
        let body = serde_json::json!({
            "prs": [{"repo": "o/r", "number": 1, "title": "t", "status": "pending"}],
            "reviews": [{"repo": "o/r", "number": 1, "head_sha": "s", "score": 8.5, "reviewed_at": "t"}],
            "todos": [{"id": 1, "text": "x", "done": 0, "priority": 0, "updated_at": "t"}],
            "server_time": "2026-01-01T00:00:00",
        });
        let (p, r, t) = apply_pull(&db, &body).unwrap();
        assert_eq!((p, r, t), (1, 1, 1));
        assert_eq!(db.get_cursor().unwrap().as_deref(), Some("2026-01-01T00:00:00"));
        assert_eq!(db.list_prs().unwrap().len(), 1);
    }

    #[test]
    fn last_sync_state() {
        let state = shared_last_sync();
        assert_eq!(last_sync_text(&state), None); // 从未同步
        mark_synced(&state);
        let text = last_sync_text(&state).unwrap();
        assert!(text.starts_with("已同步 0m 前"), "got: {text}");
    }
}
