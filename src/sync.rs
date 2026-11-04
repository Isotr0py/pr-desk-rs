//! 同步引擎：sync_pull 增量拉取 → 本地 upsert；outbox 改动 → sync_push；
//! 冲突按服务器返回处理（服务器赢：把服务器值写回本地）。
//!
//! 拆分为纯 HTTP（pull_body/push_body）与纯 DB（apply_pull/apply_push_result）两层，
//! 调用方（worker）在两次锁之间不跨 await 持有 DB 锁。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use hmac::{Hmac, Mac};
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::db::Db;

type HmacSha256 = Hmac<Sha256>;

/// 请求路径前缀（不含 host 与 query），签名输入串使用它。
const API_PATH: &str = "/api/v1/plugins/extensions/astrbot_plugin_github_pr_review";

/// 纯签名函数：HMAC_SHA256(key=token, msg="{ts}\n{nonce}\n{METHOD}\n{path}\n{sha256_hex(body)}")，
/// 输出小写 hex。参数化 timestamp/nonce 便于测试。
pub fn sign_request(
    token: &str,
    method: &str,
    path: &str,
    body: &[u8],
    timestamp: &str,
    nonce: &str,
) -> String {
    let body_hash = hex::encode(Sha256::digest(body));
    let msg = format!("{timestamp}\n{nonce}\n{method}\n{path}\n{body_hash}");
    let mut mac = HmacSha256::new_from_slice(token.as_bytes()).expect("HMAC 接受任意长度 key");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 当前 unix 秒时间戳字符串。
pub fn current_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

/// 随机 nonce：16 位小写 hex。由 nanos 时间戳 + pid + 原子计数器拼 sha256 取前 8 字节，
/// 计数器保证同一秒（乃至同一纳秒）内多次调用不重复。
pub fn gen_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let material = format!("{}:{}:{}", nanos, std::process::id(), seq);
    let digest = Sha256::digest(material.as_bytes());
    hex::encode(&digest[..8])
}

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
/// lang："zh"/"en"（i18n::tr）。
pub fn last_sync_text(state: &SharedLastSync, lang: &str) -> Option<String> {
    let ts = *state.lock().unwrap();
    ts.map(|ts| crate::i18n::trf(lang, "sync_last", &[&rel_short(ts)]))
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

/// 最小 percent-encode（不引 url 依赖）：保留 RFC3986 unreserved 字符，其余按字节 %XX。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Clone)]
pub struct SyncClient {
    client: reqwest::Client,
    base: String,
    token: String,
    /// AstrBot 面板 API Key（新版 AstrBot 对插件路由强制鉴权；空 = 不发送，兼容旧部署）
    api_key: String,
}

impl SyncClient {
    pub fn new(cfg: &Config) -> Self {
        // 数据源取当前生效档案（多数据源）：local 档 / 空 token → enabled() = false
        let p = cfg.active_sync();
        Self {
            client: reqwest::Client::builder()
                .user_agent("pr-desk-rs")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base: p
                .as_ref()
                .map(|p| format!("{}{}", p.server_url.trim_end_matches('/'), API_PATH))
                .unwrap_or_default(),
            token: p.as_ref().map(|p| p.sync_token.clone()).unwrap_or_default(),
            api_key: p.map(|p| p.api_key).unwrap_or_default(),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.token.is_empty()
    }

    /// AstrBot 中间件鉴权头（X-API-Key）；未配置则不加。
    fn with_api_key(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            req
        } else {
            req.header("X-API-Key", &self.api_key)
        }
    }

    /// 生成三个签名头（timestamp / nonce / signature），token 不出现在请求里。
    fn sign_headers(&self, method: &str, path: &str, body: &[u8]) -> [(String, String); 3] {
        let ts = current_timestamp();
        let nonce = gen_nonce();
        let sig = sign_request(&self.token, method, path, body, &ts, &nonce);
        [
            ("X-Sync-Timestamp".to_string(), ts),
            ("X-Sync-Nonce".to_string(), nonce),
            ("X-Sync-Signature".to_string(), sig),
        ]
    }

    /// GET sync_pull（since 为 None = 全量）。
    pub async fn pull_body(&self, since: Option<String>) -> Result<Value> {
        let path = format!("{API_PATH}/sync_pull");
        let mut url = format!("{}/sync_pull", self.base);
        if let Some(s) = &since {
            url = format!("{url}?since={}", url_encode(s));
        }
        let mut req = self.with_api_key(self.client.get(&url));
        for (k, v) in self.sign_headers("GET", &path, b"") {
            req = req.header(k, v);
        }
        let resp = ensure_success(req.send().await?).await?;
        Ok(resp.json().await?)
    }

    /// POST sync_push。先序列化为字节，对这份字节签名后原样发送，
    /// 保证签名覆盖线上真实字节（不让 reqwest 再序列化一次）。
    pub async fn push_body(&self, prs_updates: Vec<Value>, reviews_new: Vec<Value>) -> Result<Value> {
        let path = format!("{API_PATH}/sync_push");
        let body = serde_json::json!({
            "prs_updates": prs_updates,
            "todos_updates": [],
            "todos_new": [],
            "reviews_new": reviews_new,
        });
        let body_bytes = serde_json::to_vec(&body)?;
        let mut req = self.with_api_key(
            self.client
                .post(format!("{}/sync_push", self.base))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_bytes.clone()),
        );
        for (k, v) in self.sign_headers("POST", &path, &body_bytes) {
            req = req.header(k, v);
        }
        let resp = ensure_success(req.send().await?).await?;
        Ok(resp.json().await?)
    }
}

/// 非 2xx 时把服务器返回体带进错误（401 的原因有好几种：插件 sync 未启用 /
/// token 不符 / 签名不匹配 / AstrBot 中间件拦截，原文是唯一的区分手段）。
async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let body = body.trim().chars().take(200).collect::<String>();
    let detail = if body.is_empty() { "服务器无详细说明".to_string() } else { body };
    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("同步认证失败（401）：{detail}");
    }
    anyhow::bail!("sync HTTP {status}：{detail}");
}

/// 服务器时间戳归一化为 UTC RFC3339（带 Z 后缀），与 db::now() / GitHub meta 同格式，
/// 保证列表排序（字符串字典序）可靠：
/// - 已带时区的 RFC3339 → 转 UTC；
/// - 裸 "YYYY-MM-DDTHH:MM:SS[.f]" / 空格分隔 → 按 UTC 处理补 Z
///   （假设：插件服务器与本机时区一致或本就是 UTC；旧插件写本地时间无时区，偏差由该假设承担）；
/// - 其它形态原样返回。
pub fn normalize_server_ts(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return s.to_string();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, fmt) {
            return t
                .and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        }
    }
    s.to_string()
}

/// 就地归一化一行 JSON 里的指定时间戳字段。
fn normalize_row_ts(row: &mut Value, fields: &[&str]) {
    for f in fields {
        if let Some(s) = row[*f].as_str() {
            let n = normalize_server_ts(s);
            row[*f] = Value::from(n);
        }
    }
}

/// 应用 pull 响应到本地。返回 (prs, reviews, todos) 行数。
/// prs 行注入 source="server"：标记为服务器插件数据库跟踪的 PR（列表按来源过滤）。
pub fn apply_pull(db: &Db, body: &Value) -> Result<(usize, usize, usize)> {
    let prs = body["prs"].as_array().cloned().unwrap_or_default();
    let reviews = body["reviews"].as_array().cloned().unwrap_or_default();
    let todos = body["todos"].as_array().cloned().unwrap_or_default();
    for p in &prs {
        let mut p = p.clone();
        p["source"] = Value::from("server");
        normalize_row_ts(&mut p, &["updated_at", "extras_updated_at"]);
        db.upsert_pr(&p)?;
    }
    for r in &reviews {
        let mut r = r.clone();
        normalize_row_ts(&mut r, &["reviewed_at"]);
        db.upsert_review(&r)?;
    }
    for t in &todos {
        let mut t = t.clone();
        normalize_row_ts(&mut t, &["created_at", "done_at", "updated_at"]);
        db.upsert_todo(&t)?;
    }
    // server_time 原样存游标：它会在下次 sync_pull 时作为 since 原样回传服务器，
    // 服务器按自家格式比较，本地不做归一化以免破坏服务器侧语义。
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
    fn sign_request_push_vector() {
        let sig = sign_request(
            "test-secret-123",
            "POST",
            "/api/v1/plugins/extensions/astrbot_plugin_github_pr_review/sync_push",
            br#"{"prs_updates":[],"todos_updates":[],"todos_new":[],"reviews_new":[]}"#,
            "1730000000",
            "abc123def456",
        );
        assert_eq!(
            sig,
            "9bd9394f72f951b84b5f15d41cd00e3437a6e4a6fa9a60b7f05c7efd04428528"
        );
    }

    #[test]
    fn sign_request_pull_vector() {
        let sig = sign_request(
            "test-secret-123",
            "GET",
            "/api/v1/plugins/extensions/astrbot_plugin_github_pr_review/sync_pull",
            b"",
            "1730000000",
            "abc123def456",
        );
        assert_eq!(
            sig,
            "5beab6cd98aa8199908b9834ee3b3346727495de0bc3afbb45333ea9e1b182f5"
        );
    }

    #[test]
    fn nonce_unique() {
        let nonces: std::collections::HashSet<String> = (0..1000).map(|_| gen_nonce()).collect();
        assert_eq!(nonces.len(), 1000);
        assert!(nonces.iter().all(|n| n.len() == 16
            && n.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())));
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
        let rows = db.list_prs(&[]).unwrap();
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
        let rows = db.list_prs(&[]).unwrap();
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
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        // pull 下来的行标记为 server 来源（sync 模式列表过滤依据）
        assert_eq!(rows[0]["source"], "server");
    }

    #[test]
    fn last_sync_state() {
        let state = shared_last_sync();
        assert_eq!(last_sync_text(&state, "zh"), None); // 从未同步
        mark_synced(&state);
        let text = last_sync_text(&state, "zh").unwrap();
        assert!(text.starts_with("已同步 0m 前"), "got: {text}");
        let text_en = last_sync_text(&state, "en").unwrap();
        assert!(text_en.starts_with("Synced 0m ago"), "got: {text_en}");
    }

    #[test]
    fn url_encode_percent_encodes_non_unreserved() {
        assert_eq!(url_encode("2026-01-01T00:00:00Z"), "2026-01-01T00%3A00%3A00Z");
        assert_eq!(url_encode("abc-DEF_019.~"), "abc-DEF_019.~");
        assert_eq!(url_encode(""), "");
        assert_eq!(url_encode("a b+c"), "a%20b%2Bc");
    }

    #[test]
    fn normalize_server_ts_variants() {
        // 裸本地时间 → 按 UTC 补 Z
        assert_eq!(normalize_server_ts("2026-01-01T12:00:00"), "2026-01-01T12:00:00Z");
        // 带小数秒 / 空格分隔
        assert_eq!(normalize_server_ts("2026-01-01T12:00:00.123"), "2026-01-01T12:00:00Z");
        assert_eq!(normalize_server_ts("2026-01-01 12:00:00"), "2026-01-01T12:00:00Z");
        // 带时区 → 转 UTC
        assert_eq!(normalize_server_ts("2026-01-01T20:00:00+08:00"), "2026-01-01T12:00:00Z");
        // 已是 UTC Z → 原样（秒精度）
        assert_eq!(normalize_server_ts("2026-01-01T12:00:00Z"), "2026-01-01T12:00:00Z");
        // 垃圾输入原样返回
        assert_eq!(normalize_server_ts("not-a-time"), "not-a-time");
        assert_eq!(normalize_server_ts(""), "");
    }

    #[test]
    fn apply_pull_normalizes_timestamps() {
        let db = Db::open_memory().unwrap();
        let body = serde_json::json!({
            "prs": [{"repo": "o/r", "number": 1, "title": "t", "status": "pending",
                     "updated_at": "2026-01-01T12:00:00"}],
            "reviews": [{"repo": "o/r", "number": 1, "head_sha": "s", "score": 8.5,
                         "reviewed_at": "2026-01-01T20:00:00+08:00"}],
            "todos": [{"id": 1, "text": "x", "updated_at": "2026-01-01 12:00:00"}],
            "server_time": "2026-01-01T12:00:00",
        });
        apply_pull(&db, &body).unwrap();
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows[0]["updated_at"], "2026-01-01T12:00:00Z");
        let r = db.get_review("o/r", 1).unwrap().unwrap();
        assert_eq!(r["reviewed_at"], "2026-01-01T12:00:00Z");
        // 游标原样保存（回传服务器用，不做归一化）
        assert_eq!(db.get_cursor().unwrap().as_deref(), Some("2026-01-01T12:00:00"));
    }
}
