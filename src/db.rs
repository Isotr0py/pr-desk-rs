//! 本地 SQLite 层：schema 对齐插件（prs/reviews/todos）+ sync_state 游标 + outbox 改动队列
//! + diff_cache。迁移用 PRAGMA user_version。

use anyhow::Result;
use rusqlite::{Connection, params};

pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS prs (
    repo TEXT NOT NULL, number INTEGER NOT NULL, title TEXT, author TEXT, url TEXT,
    head_sha TEXT, status TEXT NOT NULL DEFAULT 'pending', tags TEXT NOT NULL DEFAULT '[]',
    role TEXT NOT NULL DEFAULT 'reviewer', on_board INTEGER NOT NULL DEFAULT 0,
    priority INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
    ci_status TEXT, review_decision TEXT, turn TEXT, updated_at TEXT,
    PRIMARY KEY (repo, number)
);
CREATE TABLE IF NOT EXISTS reviews (
    repo TEXT NOT NULL, number INTEGER NOT NULL, head_sha TEXT NOT NULL, score REAL,
    dimensions TEXT, summary TEXT, flagged_files TEXT, reviewed_at TEXT,
    high_value INTEGER NOT NULL DEFAULT 0, oversized INTEGER NOT NULL DEFAULT 0,
    files_changed INTEGER, additions INTEGER, deletions INTEGER, size_label TEXT,
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
        let version: i32 =
            conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Db { conn })
    }

    pub fn open_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA_V1)?;
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
                on_board, priority, pinned, ci_status, review_decision, turn, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(repo, number) DO UPDATE SET
                title=excluded.title, author=excluded.author, url=excluded.url,
                head_sha=excluded.head_sha, status=excluded.status, tags=excluded.tags,
                role=excluded.role, on_board=excluded.on_board, priority=excluded.priority,
                pinned=excluded.pinned, ci_status=excluded.ci_status,
                review_decision=excluded.review_decision, turn=excluded.turn,
                updated_at=excluded.updated_at",
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
            ],
        )?;
        Ok(())
    }

    pub fn upsert_review(&self, r: &serde_json::Value) -> Result<()> {
        self.conn.execute(
            "INSERT INTO reviews(repo, number, head_sha, score, dimensions, summary,
                flagged_files, reviewed_at, high_value, oversized, files_changed,
                additions, deletions, size_label)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(repo, number, head_sha) DO UPDATE SET
                score=excluded.score, dimensions=excluded.dimensions,
                summary=excluded.summary, flagged_files=excluded.flagged_files,
                reviewed_at=excluded.reviewed_at, high_value=excluded.high_value,
                oversized=excluded.oversized, files_changed=excluded.files_changed,
                additions=excluded.additions, deletions=excluded.deletions,
                size_label=excluded.size_label",
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
            ],
        )?;
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
    /// 列表行：prs + 最近 review 的 score（与插件 list_prs 同语义）。
    pub fn list_prs(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.*, r.score, r.reviewed_at FROM prs p
             LEFT JOIN reviews r ON r.rowid = (
                SELECT r2.rowid FROM reviews r2
                WHERE r2.repo=p.repo AND r2.number=p.number
                ORDER BY r2.reviewed_at DESC LIMIT 1)
             ORDER BY p.pinned DESC, p.updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "repo": row.get::<_, String>(0)?,
                    "number": row.get::<_, i64>(1)?,
                    "title": row.get::<_, Option<String>>(2)?,
                    "author": row.get::<_, Option<String>>(3)?,
                    "url": row.get::<_, Option<String>>(4)?,
                    "status": row.get::<_, String>(6)?,
                    "tags": row.get::<_, String>(7)?,
                    "role": row.get::<_, String>(8)?,
                    "on_board": row.get::<_, i64>(9)?,
                    "priority": row.get::<_, i64>(10)?,
                    "pinned": row.get::<_, i64>(11)?,
                    "ci_status": row.get::<_, Option<String>>(12)?,
                    "review_decision": row.get::<_, Option<String>>(13)?,
                    "turn": row.get::<_, Option<String>>(14)?,
                    "score": row.get::<_, Option<f64>>(16)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_review(&self, repo: &str, number: i64) -> Result<Option<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT score, dimensions, summary, flagged_files, reviewed_at, size_label
             FROM reviews WHERE repo=?1 AND number=?2 ORDER BY reviewed_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![repo, number])?;
        if let Some(r) = rows.next()? {
            return Ok(Some(serde_json::json!({
                "score": r.get::<_, Option<f64>>(0)?,
                "dimensions": r.get::<_, Option<String>>(1)?,
                "summary": r.get::<_, Option<String>>(2)?,
                "flagged_files": r.get::<_, Option<String>>(3)?,
                "reviewed_at": r.get::<_, Option<String>>(4)?,
                "size_label": r.get::<_, Option<String>>(5)?,
            })));
        }
        Ok(None)
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
        let rows = db.list_prs().unwrap();
        assert_eq!(rows.len(), 1);
        // 更新标题 → 覆盖而非新增
        db.upsert_pr(&pr_json("o/r", 1, "t2")).unwrap();
        let rows = db.list_prs().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["title"], "t2");
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
        let rows = db.list_prs().unwrap();
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
}
