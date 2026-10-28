//! 本地 SQLite 层：schema 对齐插件（prs/reviews/todos）+ sync_state 游标 + outbox 改动队列
//! + diff_cache。迁移用 PRAGMA user_version。

use anyhow::Result;
use rusqlite::{Connection, params};

pub const SCHEMA_VERSION: i32 = 2;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS prs (
    repo TEXT NOT NULL, number INTEGER NOT NULL, title TEXT, author TEXT, url TEXT,
    head_sha TEXT, status TEXT NOT NULL DEFAULT 'pending', tags TEXT NOT NULL DEFAULT '[]',
    role TEXT NOT NULL DEFAULT 'reviewer', on_board INTEGER NOT NULL DEFAULT 0,
    priority INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
    ci_status TEXT, review_decision TEXT, turn TEXT, updated_at TEXT,
    source TEXT NOT NULL DEFAULT 'github',
    PRIMARY KEY (repo, number)
);
CREATE TABLE IF NOT EXISTS reviews (
    repo TEXT NOT NULL, number INTEGER NOT NULL, head_sha TEXT NOT NULL, score REAL,
    dimensions TEXT, summary TEXT, flagged_files TEXT, reviewed_at TEXT,
    high_value INTEGER NOT NULL DEFAULT 0, oversized INTEGER NOT NULL DEFAULT 0,
    files_changed INTEGER, additions INTEGER, deletions INTEGER, size_label TEXT,
    pushed INTEGER NOT NULL DEFAULT 0, comments TEXT,
    PRIMARY KEY (repo, number, head_sha)
);
CREATE TABLE IF NOT EXISTS todos (
    id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, repo TEXT, number INTEGER,
    done INTEGER NOT NULL DEFAULT 0, priority INTEGER NOT NULL DEFAULT 0,
    created_at TEXT, done_at TEXT, updated_at TEXT
);
CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, payload TEXT NOT NULL,
    created_at TEXT
);
CREATE TABLE IF NOT EXISTS diff_cache (
    repo TEXT NOT NULL, number INTEGER NOT NULL, diff TEXT NOT NULL, fetched_at TEXT,
    PRIMARY KEY (repo, number)
);
"#;

// v2：prs 增加 extras 缓存列（recent_commits / extras_detail / extras_updated_at），
// 与插件 schema 对齐，会话/Checks 标签页的数据来源。
const MIGRATE_V2: &str = r#"
ALTER TABLE prs ADD COLUMN recent_commits TEXT NOT NULL DEFAULT '[]';
ALTER TABLE prs ADD COLUMN extras_detail TEXT;
ALTER TABLE prs ADD COLUMN extras_updated_at TEXT;
"#;

pub struct Db {
    pub conn: Connection,
}

pub fn now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::migrate(conn)
    }

    pub fn open_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(conn)
    }

    fn migrate(conn: Connection) -> Result<Db> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
        }
        // reviews 增加 pushed 列（老库迁移；新库已含该列，忽略 duplicate column name）
        if let Err(e) = conn.execute(
            "ALTER TABLE reviews ADD COLUMN pushed INTEGER NOT NULL DEFAULT 0",
            [],
        ) {
            if !e.to_string().contains("duplicate column name") {
                return Err(e.into());
            }
        }
        // reviews 增加 comments 列（ocr 行级评论 JSON；老库迁移，同上幂等）
        if let Err(e) = conn.execute("ALTER TABLE reviews ADD COLUMN comments TEXT", []) {
            if !e.to_string().contains("duplicate column name") {
                return Err(e.into());
            }
        }
        // prs 增加 source 列：github=GitHub 火hose / server=sync_pull / inbox=手动预评审
        // （老库迁移；表不存在或列已存在则跳过，同 MIGRATE_V2 的列检查做法）
        let prs_cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(prs)")?;
            stmt.query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !prs_cols.is_empty() && !prs_cols.iter().any(|c| c == "source") {
            conn.execute_batch("ALTER TABLE prs ADD COLUMN source TEXT NOT NULL DEFAULT 'github'")?;
        }
        if version < 2 {
            // v1 → v2：prs 加 extras 缓存列（幂等：列已存在则跳过）
            let cols: Vec<String> = {
                let mut stmt = conn.prepare("PRAGMA table_info(prs)")?;
                stmt.query_map([], |r| r.get::<_, String>(1))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if !cols.iter().any(|c| c == "recent_commits") {
                conn.execute_batch(MIGRATE_V2)?;
            }
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Db { conn })
    }

    // ── 游标 ──
    pub fn get_cursor(&self) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM sync_state WHERE key='last_sync'")?;
        let mut rows = stmt.query([])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    pub fn set_cursor(&self, ts: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state(key, value) VALUES('last_sync', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![ts],
        )?;
        Ok(())
    }

    // ── upsert（sync_pull 应用；幂等） ──
    pub fn upsert_pr(&self, p: &serde_json::Value) -> Result<()> {
        self.conn.execute(
            "INSERT INTO prs(repo, number, title, author, url, head_sha, status, tags, role,
                on_board, priority, pinned, ci_status, review_decision, turn, updated_at,
                recent_commits, extras_detail, extras_updated_at, source)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
             ON CONFLICT(repo, number) DO UPDATE SET
                title=excluded.title, author=excluded.author, url=excluded.url,
                head_sha=excluded.head_sha, status=excluded.status, tags=excluded.tags,
                role=excluded.role, on_board=excluded.on_board, priority=excluded.priority,
                pinned=excluded.pinned, ci_status=excluded.ci_status,
                review_decision=excluded.review_decision, turn=excluded.turn,
                updated_at=excluded.updated_at,
                recent_commits=excluded.recent_commits,
                extras_detail=excluded.extras_detail,
                extras_updated_at=excluded.extras_updated_at,
                source=CASE
                    WHEN excluded.source='server' OR prs.source='server' THEN 'server'
                    WHEN excluded.source='inbox' OR prs.source='inbox' THEN 'inbox'
                    ELSE 'github' END",
            params![
                p["repo"].as_str().unwrap_or(""),
                p["number"].as_i64().unwrap_or(0),
                p["title"].as_str(),
                p["author"].as_str(),
                p["url"].as_str(),
                p["head_sha"].as_str(),
                p["status"].as_str().unwrap_or("pending"),
                p["tags"].as_str().unwrap_or("[]"),
                p["role"].as_str().unwrap_or("reviewer"),
                p["on_board"].as_i64().unwrap_or(0),
                p["priority"].as_i64().unwrap_or(0),
                p["pinned"].as_i64().unwrap_or(0),
                p["ci_status"].as_str(),
                p["review_decision"].as_str(),
                p["turn"].as_str(),
                p["updated_at"].as_str(),
                p["recent_commits"].as_str().unwrap_or("[]"),
                p["extras_detail"].as_str(),
                p["extras_updated_at"].as_str(),
                p["source"].as_str().unwrap_or("github"),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_review(&self, r: &serde_json::Value) -> Result<()> {
        // comments：数组 → JSON 字符串；字符串透传；缺失/其它类型 → NULL
        let comments: Option<String> = match &r["comments"] {
            serde_json::Value::Array(_) => Some(serde_json::to_string(&r["comments"])?),
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        };
        self.conn.execute(
            "INSERT INTO reviews(repo, number, head_sha, score, dimensions, summary,
                flagged_files, reviewed_at, high_value, oversized, files_changed,
                additions, deletions, size_label, pushed, comments)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,0,?15)
             ON CONFLICT(repo, number, head_sha) DO UPDATE SET
                score=excluded.score, dimensions=excluded.dimensions,
                summary=excluded.summary, flagged_files=excluded.flagged_files,
                reviewed_at=excluded.reviewed_at, high_value=excluded.high_value,
                oversized=excluded.oversized, files_changed=excluded.files_changed,
                additions=excluded.additions, deletions=excluded.deletions,
                size_label=excluded.size_label, pushed=0, comments=excluded.comments",
            params![
                r["repo"].as_str().unwrap_or(""),
                r["number"].as_i64().unwrap_or(0),
                r["head_sha"].as_str().unwrap_or(""),
                r["score"].as_f64(),
                r["dimensions"].as_str(),
                r["summary"].as_str(),
                r["flagged_files"].as_str(),
                r["reviewed_at"].as_str(),
                r["high_value"].as_i64().unwrap_or(0),
                r["oversized"].as_i64().unwrap_or(0),
                r["files_changed"].as_i64(),
                r["additions"].as_i64(),
                r["deletions"].as_i64(),
                r["size_label"].as_str(),
                comments,
            ],
        )?;
        Ok(())
    }

    /// 待推送的本地评审（pushed=0）；dimensions/flagged_files 从 JSON 字符串解析回对象/数组。
    pub fn unpushed_reviews(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo, number, head_sha, score, dimensions, summary, flagged_files,
                reviewed_at, high_value, oversized, files_changed, additions, deletions, size_label,
                comments
             FROM reviews WHERE pushed=0",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let dims: Option<serde_json::Value> =
                    r.get::<_, Option<String>>(4)?.and_then(|s| serde_json::from_str(&s).ok());
                let flagged: Option<serde_json::Value> =
                    r.get::<_, Option<String>>(6)?.and_then(|s| serde_json::from_str(&s).ok());
                let comments: Option<serde_json::Value> =
                    r.get::<_, Option<String>>(14)?.and_then(|s| serde_json::from_str(&s).ok());
                Ok(serde_json::json!({
                    "repo": r.get::<_, String>(0)?,
                    "number": r.get::<_, i64>(1)?,
                    "head_sha": r.get::<_, String>(2)?,
                    "score": r.get::<_, Option<f64>>(3)?,
                    "dimensions": dims,
                    "summary": r.get::<_, Option<String>>(5)?,
                    "flagged_files": flagged,
                    "reviewed_at": r.get::<_, Option<String>>(7)?,
                    "high_value": r.get::<_, i64>(8)?,
                    "oversized": r.get::<_, i64>(9)?,
                    "files_changed": r.get::<_, Option<i64>>(10)?,
                    "additions": r.get::<_, Option<i64>>(11)?,
                    "deletions": r.get::<_, Option<i64>>(12)?,
                    "size_label": r.get::<_, Option<String>>(13)?,
                    "comments": comments,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 推送成功后按 (repo, number, head_sha) 置 pushed=1。
    pub fn mark_reviews_pushed(&self, keys: &[(String, i64, String)]) -> Result<()> {
        for (repo, number, head_sha) in keys {
            self.conn.execute(
                "UPDATE reviews SET pushed=1 WHERE repo=?1 AND number=?2 AND head_sha=?3",
                params![repo, number, head_sha],
            )?;
        }
        Ok(())
    }

    pub fn upsert_todo(&self, t: &serde_json::Value) -> Result<()> {
        let id = t["id"].as_i64().unwrap_or(0);
        self.conn.execute(
            "INSERT INTO todos(id, text, repo, number, done, priority, created_at, done_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET text=excluded.text, repo=excluded.repo,
                number=excluded.number, done=excluded.done, priority=excluded.priority,
                done_at=excluded.done_at, updated_at=excluded.updated_at",
            params![
                id,
                t["text"].as_str().unwrap_or(""),
                t["repo"].as_str(),
                t["number"].as_i64(),
                t["done"].as_i64().unwrap_or(0),
                t["priority"].as_i64().unwrap_or(0),
                t["created_at"].as_str(),
                t["done_at"].as_str(),
                t["updated_at"].as_str(),
            ],
        )?;
        Ok(())
    }

    // ── 列表查询 ──
    /// 列表行：prs + 最近 review 的 score/体量（与插件 list_prs 同语义）。
    /// `sources` 非空时只返回这些来源的行（sync 模式传 ["server","inbox"]，本地模式传 &[]）。
    /// 显式列名（不用 p.*）：source 列在新库（CREATE 内）与老库（ALTER 追加）位置不同。
    pub fn list_prs(&self, sources: &[&str]) -> Result<Vec<serde_json::Value>> {
        let where_clause = if sources.is_empty() {
            String::new()
        } else {
            let ph = (1..=sources.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("WHERE p.source IN ({ph})")
        };
        let sql = format!(
            "SELECT p.repo, p.number, p.title, p.author, p.url, p.status, p.tags, p.role,
                p.on_board, p.priority, p.pinned, p.ci_status, p.review_decision, p.turn,
                p.updated_at, p.recent_commits, p.extras_detail, p.source,
                r.score, r.reviewed_at, r.size_label, r.additions, r.deletions
             FROM prs p
             LEFT JOIN reviews r ON r.rowid = (
                SELECT r2.rowid FROM reviews r2
                WHERE r2.repo=p.repo AND r2.number=p.number
                ORDER BY r2.reviewed_at DESC LIMIT 1)
             {where_clause}
             ORDER BY p.pinned DESC, p.updated_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(sources.iter()), |row| {
                Ok(serde_json::json!({
                    "repo": row.get::<_, String>(0)?,
                    "number": row.get::<_, i64>(1)?,
                    "title": row.get::<_, Option<String>>(2)?,
                    "author": row.get::<_, Option<String>>(3)?,
                    "url": row.get::<_, Option<String>>(4)?,
                    "status": row.get::<_, String>(5)?,
                    "tags": row.get::<_, String>(6)?,
                    "role": row.get::<_, String>(7)?,
                    "on_board": row.get::<_, i64>(8)?,
                    "priority": row.get::<_, i64>(9)?,
                    "pinned": row.get::<_, i64>(10)?,
                    "ci_status": row.get::<_, Option<String>>(11)?,
                    "review_decision": row.get::<_, Option<String>>(12)?,
                    "turn": row.get::<_, Option<String>>(13)?,
                    "updated_at": row.get::<_, Option<String>>(14)?,
                    "recent_commits": row.get::<_, Option<String>>(15)?,
                    "extras_detail": row.get::<_, Option<String>>(16)?,
                    "source": row.get::<_, String>(17)?,
                    "score": row.get::<_, Option<f64>>(18)?,
                    "size_label": row.get::<_, Option<String>>(20)?,
                    "additions": row.get::<_, Option<i64>>(21)?,
                    "deletions": row.get::<_, Option<i64>>(22)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_review(&self, repo: &str, number: i64) -> Result<Option<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT score, dimensions, summary, flagged_files, reviewed_at, size_label,
                    files_changed, additions, deletions, comments
             FROM reviews WHERE repo=?1 AND number=?2 ORDER BY reviewed_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![repo, number])?;
        if let Some(r) = rows.next()? {
            // comments：TEXT 解析回 JSON 数组，失败/缺失给 null
            let comments: serde_json::Value = r
                .get::<_, Option<String>>(9)?
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .filter(|v| v.is_array())
                .unwrap_or(serde_json::Value::Null);
            return Ok(Some(serde_json::json!({
                "score": r.get::<_, Option<f64>>(0)?,
                "dimensions": r.get::<_, Option<String>>(1)?,
                "summary": r.get::<_, Option<String>>(2)?,
                "flagged_files": r.get::<_, Option<String>>(3)?,
                "reviewed_at": r.get::<_, Option<String>>(4)?,
                "size_label": r.get::<_, Option<String>>(5)?,
                "files_changed": r.get::<_, Option<i64>>(6)?,
                "additions": r.get::<_, Option<i64>>(7)?,
                "deletions": r.get::<_, Option<i64>>(8)?,
                "comments": comments,
            })));
        }
        Ok(None)
    }

    /// PR 是否已在本地库（Inbox「+ 预评审」的去重判断：避免 upsert 把已有行字段覆盖成空）。
    pub fn pr_exists(&self, repo: &str, number: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM prs WHERE repo=?1 AND number=?2",
            params![repo, number],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    // ── 本地改动（立即生效 + 入队 outbox 待推送） ──
    pub fn local_pr_update(&self, repo: &str, number: i64, field: &str, value: &serde_json::Value) -> Result<()> {
        // 立即应用到本地
        match field {
            "status" => {
                self.conn.execute(
                    "UPDATE prs SET status=?1, updated_at=?2 WHERE repo=?3 AND number=?4",
                    params![value.as_str().unwrap_or("pending"), now(), repo, number],
                )?;
            }
            "tags" => {
                self.conn.execute(
                    "UPDATE prs SET tags=?1, updated_at=?2 WHERE repo=?3 AND number=?4",
                    params![serde_json::to_string(value)?, now(), repo, number],
                )?;
            }
            "priority" | "on_board" | "pinned" => {
                self.conn.execute(
                    &format!("UPDATE prs SET {field}=?1, updated_at=?2 WHERE repo=?3 AND number=?4"),
                    params![value.as_i64().unwrap_or(0), now(), repo, number],
                )?;
            }
            _ => anyhow::bail!("unknown field {field}"),
        }
        // 入队待推送
        self.conn.execute(
            "INSERT INTO outbox(kind, payload, created_at) VALUES ('pr', ?1, ?2)",
            params![
                serde_json::to_string(&serde_json::json!({
                    "repo": repo, "number": number, field: value,
                    "client_updated_at": now(),
                }))?,
                now(),
            ],
        )?;
        Ok(())
    }

    pub fn outbox_drain(&self) -> Result<Vec<(i64, serde_json::Value)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, payload FROM outbox ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, p)| (id, serde_json::from_str(&p).unwrap_or_default()))
            .collect())
    }

    pub fn outbox_remove(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM outbox WHERE id=?1", params![id])?;
        Ok(())
    }

    // ── diff 缓存 ──
    pub fn get_diff(&self, repo: &str, number: i64) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT diff FROM diff_cache WHERE repo=?1 AND number=?2")?;
        let mut rows = stmt.query(params![repo, number])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    pub fn put_diff(&self, repo: &str, number: i64, diff: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO diff_cache(repo, number, diff, fetched_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(repo, number) DO UPDATE SET diff=excluded.diff, fetched_at=excluded.fetched_at",
            params![repo, number, diff, now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_json(repo: &str, n: i64, title: &str) -> serde_json::Value {
        serde_json::json!({
            "repo": repo, "number": n, "title": title, "author": "a", "url": "u",
            "head_sha": "s", "status": "pending", "updated_at": "2026-01-01T00:00:00",
        })
    }

    #[test]
    fn upsert_pr_idempotent() {
        let db = Db::open_memory().unwrap();
        db.upsert_pr(&pr_json("o/r", 1, "t")).unwrap();
        db.upsert_pr(&pr_json("o/r", 1, "t")).unwrap();
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        // 更新标题 → 覆盖而非新增
        db.upsert_pr(&pr_json("o/r", 1, "t2")).unwrap();
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["title"], "t2");
        // 未带 source 字段 → 默认 'github'
        assert_eq!(rows[0]["source"], "github");
    }

    fn pr_json_src(repo: &str, n: i64, title: &str, source: &str) -> serde_json::Value {
        let mut v = pr_json(repo, n, title);
        v["source"] = serde_json::json!(source);
        v
    }

    fn source_of(db: &Db, n: i64) -> String {
        let rows = db.list_prs(&[]).unwrap();
        rows.iter()
            .find(|r| r["number"] == n)
            .unwrap()["source"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn source_merge_rules() {
        let db = Db::open_memory().unwrap();
        // server 赢 github（后到的 server 覆盖；先到的 server 不被 github 降级）
        db.upsert_pr(&pr_json_src("o/r", 1, "t", "github")).unwrap();
        db.upsert_pr(&pr_json_src("o/r", 1, "t", "server")).unwrap();
        assert_eq!(source_of(&db, 1), "server");
        db.upsert_pr(&pr_json_src("o/r", 1, "t", "github")).unwrap();
        assert_eq!(source_of(&db, 1), "server");
        // inbox 保留：github 不降级 inbox，后到的 inbox 升级 github
        db.upsert_pr(&pr_json_src("o/r", 2, "t", "inbox")).unwrap();
        db.upsert_pr(&pr_json_src("o/r", 2, "t", "github")).unwrap();
        assert_eq!(source_of(&db, 2), "inbox");
        db.upsert_pr(&pr_json_src("o/r", 3, "t", "github")).unwrap();
        db.upsert_pr(&pr_json_src("o/r", 3, "t", "inbox")).unwrap();
        assert_eq!(source_of(&db, 3), "inbox");
        // server 赢 inbox
        db.upsert_pr(&pr_json_src("o/r", 3, "t", "server")).unwrap();
        assert_eq!(source_of(&db, 3), "server");
        // 缺省 source 字段（老调用方）写入 'github'，且不降级已有 server
        db.upsert_pr(&pr_json("o/r", 3, "t")).unwrap();
        assert_eq!(source_of(&db, 3), "server");
        db.upsert_pr(&pr_json("o/r", 4, "t")).unwrap();
        assert_eq!(source_of(&db, 4), "github");
    }

    #[test]
    fn list_prs_filter_by_source() {
        let db = Db::open_memory().unwrap();
        db.upsert_pr(&pr_json_src("o/r", 1, "srv", "server")).unwrap();
        db.upsert_pr(&pr_json_src("o/r", 2, "inbox", "inbox")).unwrap();
        db.upsert_pr(&pr_json_src("o/r", 3, "gh", "github")).unwrap();
        // 空过滤 = 全部（本地模式）
        assert_eq!(db.list_prs(&[]).unwrap().len(), 3);
        // sync 模式：server + inbox
        let rows = db.list_prs(&["server", "inbox"]).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r["number"] == 1 || r["number"] == 2));
        // 单一来源
        let rows = db.list_prs(&["server"]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["title"], "srv");
        // 无匹配来源
        assert_eq!(db.list_prs(&["nope"]).unwrap().len(), 0);
    }

    #[test]
    fn migrate_adds_source_column_to_old_db() {
        // 模拟老库：prs 表无 source 列（含 v2 extras 列）→ Db::open 迁移后默认 'github'
        let dir = std::env::temp_dir().join(format!("prdesk-db-mig-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE prs (
                    repo TEXT NOT NULL, number INTEGER NOT NULL, title TEXT, author TEXT,
                    url TEXT, head_sha TEXT, status TEXT NOT NULL DEFAULT 'pending',
                    tags TEXT NOT NULL DEFAULT '[]', role TEXT NOT NULL DEFAULT 'reviewer',
                    on_board INTEGER NOT NULL DEFAULT 0, priority INTEGER NOT NULL DEFAULT 0,
                    pinned INTEGER NOT NULL DEFAULT 0, ci_status TEXT, review_decision TEXT,
                    turn TEXT, updated_at TEXT,
                    recent_commits TEXT NOT NULL DEFAULT '[]',
                    extras_detail TEXT, extras_updated_at TEXT,
                    PRIMARY KEY (repo, number)
                );
                -- reviews 表需存在（migrate 里的 reviews ALTER 假设表已建好）
                CREATE TABLE reviews (
                    repo TEXT NOT NULL, number INTEGER NOT NULL, head_sha TEXT NOT NULL,
                    score REAL, dimensions TEXT, summary TEXT, flagged_files TEXT,
                    reviewed_at TEXT, high_value INTEGER NOT NULL DEFAULT 0,
                    oversized INTEGER NOT NULL DEFAULT 0, files_changed INTEGER,
                    additions INTEGER, deletions INTEGER, size_label TEXT,
                    pushed INTEGER NOT NULL DEFAULT 0, comments TEXT,
                    PRIMARY KEY (repo, number, head_sha)
                );
                INSERT INTO prs(repo, number, title, status) VALUES ('o/r', 1, 'old', 'pending');
                PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        // 老行迁移后 source='github'
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["source"], "github");
        // sync 过滤下不可见；可被 server upsert 升级
        assert_eq!(db.list_prs(&["server", "inbox"]).unwrap().len(), 0);
        db.upsert_pr(&pr_json_src("o/r", 1, "srv", "server")).unwrap();
        assert_eq!(db.list_prs(&["server", "inbox"]).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_roundtrip() {
        let db = Db::open_memory().unwrap();
        assert!(db.get_cursor().unwrap().is_none());
        db.set_cursor("2026-01-01T00:00:00").unwrap();
        assert_eq!(db.get_cursor().unwrap().as_deref(), Some("2026-01-01T00:00:00"));
        db.set_cursor("2026-01-02T00:00:00").unwrap();
        assert_eq!(db.get_cursor().unwrap().as_deref(), Some("2026-01-02T00:00:00"));
    }

    #[test]
    fn outbox_enqueue_drain_remove() {
        let db = Db::open_memory().unwrap();
        db.local_pr_update("o/r", 1, "pinned", &serde_json::json!(1)).unwrap();
        db.local_pr_update("o/r", 2, "priority", &serde_json::json!(2)).unwrap();
        let items = db.outbox_drain().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].1["pinned"], 1);
        db.outbox_remove(items[0].0).unwrap();
        assert_eq!(db.outbox_drain().unwrap().len(), 1);
        // 本地已立即生效
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows.len(), 0); // 未 upsert 过的 PR 不存在（本测试只验证 outbox）
    }

    #[test]
    fn diff_cache_roundtrip() {
        let db = Db::open_memory().unwrap();
        assert!(db.get_diff("o/r", 1).unwrap().is_none());
        db.put_diff("o/r", 1, "diff text").unwrap();
        assert_eq!(db.get_diff("o/r", 1).unwrap().as_deref(), Some("diff text"));
        db.put_diff("o/r", 1, "v2").unwrap();
        assert_eq!(db.get_diff("o/r", 1).unwrap().as_deref(), Some("v2"));
    }

    fn review_json(repo: &str, n: i64, sha: &str, score: f64) -> serde_json::Value {
        serde_json::json!({
            "repo": repo, "number": n, "head_sha": sha, "score": score,
            "dimensions": "{\"correctness\":9}", "summary": "ok",
            "flagged_files": "[\"src/a.rs\"]", "reviewed_at": "2026-01-01T00:00:00",
            "high_value": 1, "oversized": 0, "files_changed": 3,
            "additions": 10, "deletions": 2, "size_label": "S",
            "comments": [{"path": "src/a.rs", "content": "越界风险", "start_line": 10,
                "end_line": 12, "category": "bug", "severity": "high"}],
        })
    }

    #[test]
    fn unpushed_reviews_roundtrip() {
        let db = Db::open_memory().unwrap();
        assert!(db.unpushed_reviews().unwrap().is_empty());
        db.upsert_review(&review_json("o/r", 1, "s1", 8.5)).unwrap();
        let rows = db.unpushed_reviews().unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r["repo"], "o/r");
        assert_eq!(r["number"], 1);
        assert_eq!(r["head_sha"], "s1");
        assert_eq!(r["score"], 8.5);
        assert_eq!(r["dimensions"], serde_json::json!({"correctness": 9}));
        assert_eq!(r["flagged_files"], serde_json::json!(["src/a.rs"]));
        assert_eq!(r["high_value"], 1);
        assert_eq!(r["oversized"], 0);
        assert_eq!(r["files_changed"], 3);
        assert_eq!(r["additions"], 10);
        assert_eq!(r["deletions"], 2);
        assert_eq!(r["size_label"], "S");
        // comments 数组落库后解析回数组
        let comments = r["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["path"], "src/a.rs");
        assert_eq!(comments[0]["severity"], "high");
    }

    #[test]
    fn get_review_comments_roundtrip() {
        let db = Db::open_memory().unwrap();
        // 无评论（键缺失）→ NULL → 读出 null
        let mut bare = review_json("o/r", 1, "s1", 8.5);
        bare.as_object_mut().unwrap().remove("comments");
        db.upsert_review(&bare).unwrap();
        let r = db.get_review("o/r", 1).unwrap().unwrap();
        assert!(r["comments"].is_null());

        // 数组 → 序列化落库 → 解析回数组
        db.upsert_review(&review_json("o/r", 2, "s2", 7.0)).unwrap();
        let r = db.get_review("o/r", 2).unwrap().unwrap();
        let comments = r["comments"].as_array().unwrap();
        assert_eq!(comments[0]["content"], "越界风险");

        // 字符串透传（本身是 JSON 数组字符串）→ 解析回数组
        let mut s = review_json("o/r", 3, "s3", 6.0);
        s["comments"] = serde_json::json!("[{\"path\":\"x.rs\",\"severity\":\"low\"}]");
        db.upsert_review(&s).unwrap();
        let r = db.get_review("o/r", 3).unwrap().unwrap();
        assert_eq!(r["comments"][0]["path"], "x.rs");

        // 字符串透传但不是合法 JSON → 解析失败给 null
        let mut bad = review_json("o/r", 4, "s4", 5.0);
        bad["comments"] = serde_json::json!("自由文本评论");
        db.upsert_review(&bad).unwrap();
        let r = db.get_review("o/r", 4).unwrap().unwrap();
        assert!(r["comments"].is_null());

        // 未评审过的 PR → None
        assert!(db.get_review("o/r", 99).unwrap().is_none());
    }

    #[test]
    fn migrate_adds_comments_column_to_old_db() {
        // 模拟老库：reviews 表无 comments 列 → Db::open 迁移后可用
        let dir = std::env::temp_dir().join(format!("prdesk-db-mig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE reviews (
                    repo TEXT NOT NULL, number INTEGER NOT NULL, head_sha TEXT NOT NULL,
                    score REAL, dimensions TEXT, summary TEXT, flagged_files TEXT,
                    reviewed_at TEXT, high_value INTEGER NOT NULL DEFAULT 0,
                    oversized INTEGER NOT NULL DEFAULT 0, files_changed INTEGER,
                    additions INTEGER, deletions INTEGER, size_label TEXT,
                    pushed INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (repo, number, head_sha)
                );
                PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        db.upsert_review(&review_json("o/r", 1, "s1", 8.5)).unwrap();
        let r = db.get_review("o/r", 1).unwrap().unwrap();
        assert_eq!(r["comments"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_reviews_pushed_then_upsert_resets() {
        let db = Db::open_memory().unwrap();
        db.upsert_review(&review_json("o/r", 1, "s1", 8.5)).unwrap();
        db.upsert_review(&review_json("o/r", 2, "s2", 7.0)).unwrap();
        db.mark_reviews_pushed(&[("o/r".to_string(), 1, "s1".to_string())])
            .unwrap();
        let rows = db.unpushed_reviews().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["number"], 2);
        // 已推送的报告再次被 upsert（本地重新评审）→ pushed 重置为 0
        db.upsert_review(&review_json("o/r", 1, "s1", 9.0)).unwrap();
        let rows = db.unpushed_reviews().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r["number"] == 1 && r["score"] == 9.0));
    }
}
