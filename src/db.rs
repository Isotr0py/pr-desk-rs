//! 本地 SQLite 层：schema 对齐插件（prs/reviews/todos）+ sync_state 游标 + outbox 改动队列
//! + diff_cache + 仅本地的 Deep Review 工作区。迁移用 PRAGMA user_version。

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::deep_review::{DeepFindingData, DeepQaMessage, DeepWorkspaceData};

pub const SCHEMA_VERSION: i32 = 5;

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

// v3：deleted_prs 墓碑表（local 模式删除 PR 后防刷新/误加复活）。
// 表随库文件走：每个档案各自的 db 里独立一份，删除只影响本档案。
const MIGRATE_V3: &str = r#"
CREATE TABLE IF NOT EXISTS deleted_prs (
    repo TEXT NOT NULL, number INTEGER NOT NULL, deleted_at TEXT,
    PRIMARY KEY (repo, number)
);
"#;

// v4：Deep Review 是本地工作区，不进入 reviews/outbox，也不随 AstrBot 同步。
const MIGRATE_V4: &str = r#"
CREATE TABLE IF NOT EXISTS deep_reviews (
    repo TEXT NOT NULL,
    number INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    base_sha TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (repo, number, head_sha)
);
CREATE TABLE IF NOT EXISTS deep_findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    number INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    finding_json TEXT NOT NULL,
    suggested_text TEXT NOT NULL,
    draft_text TEXT NOT NULL,
    discarded INTEGER NOT NULL DEFAULT 0,
    qa_json TEXT NOT NULL DEFAULT '[]',
    revision INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    UNIQUE (repo, number, head_sha, fingerprint)
);
"#;

#[derive(Debug)]
pub struct DeepMutation {
    pub finding: Option<DeepFindingData>,
    pub conflict: bool,
}

pub struct Db {
    pub conn: Connection,
}

/// UTC RFC3339（"YYYY-MM-DDTHH:MM:SSZ"）：与 GitHub meta 落库格式一致，
/// 字符串字典序即时间序（list_prs / 最新评审靠 ORDER BY 文本排序）。
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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
        // 外部进程（插件等）持锁时写等 3s，避免直接 SQLITE_BUSY
        conn.pragma_update(None, "busy_timeout", 3000)?;
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
        // prs 增加 author_orgs 列（作者公开组织成员关系 JSON 数组；org 徽章数据源。
        // 本地模式由 refresh 从 /users/{author}/orgs 补齐；sync 行以服务端为准不覆盖）
        if !prs_cols.is_empty() && !prs_cols.iter().any(|c| c == "author_orgs") {
            conn.execute_batch("ALTER TABLE prs ADD COLUMN author_orgs TEXT NOT NULL DEFAULT '[]'")?;
        }
        // prs 增加 additions/deletions 列：Files 标签页拉到文件清单时把增删行数落库，
        // 列表 +/- 列用它兜底（评审记录的 r.additions 优先，见 list_prs 的 COALESCE）
        if !prs_cols.is_empty() && !prs_cols.iter().any(|c| c == "additions") {
            conn.execute_batch(
                "ALTER TABLE prs ADD COLUMN additions INTEGER;
                 ALTER TABLE prs ADD COLUMN deletions INTEGER;",
            )?;
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
        // v2 → v3 / v3 → v4：IF NOT EXISTS 天然幂等
        conn.execute_batch(MIGRATE_V3)?;
        conn.execute_batch(MIGRATE_V4)?;
        // v4 → v5：PR 级问答上下文和历史；逐列检查保证重复打开幂等。
        let deep_cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(deep_reviews)")?;
            stmt.query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (column, ddl) in [
            ("body", "ALTER TABLE deep_reviews ADD COLUMN body TEXT NOT NULL DEFAULT ''"),
            ("diff", "ALTER TABLE deep_reviews ADD COLUMN diff TEXT NOT NULL DEFAULT ''"),
            ("qa_json", "ALTER TABLE deep_reviews ADD COLUMN qa_json TEXT NOT NULL DEFAULT '[]'"),
        ] {
            if !deep_cols.iter().any(|c| c == column) {
                conn.execute_batch(ddl)?;
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
        // dimensions/flagged_files 同理：本地评审产出的是对象/数组（需序列化），
        // 服务器同步来的是字符串（透传）。之前用 as_str() 导致本地评审的这两列存成 NULL。
        fn json_or_str(v: &serde_json::Value) -> Option<String> {
            match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                    serde_json::to_string(v).ok()
                }
                _ => None,
            }
        }
        let dimensions = json_or_str(&r["dimensions"]);
        let flagged_files = json_or_str(&r["flagged_files"]);
        // comments 用 COALESCE 保留旧值：重复评审时 OCR 失败（comments=NULL）不清空已有行级评论。
        // 其余字段有意整体覆盖——新一轮评审结果取代旧结果；score 等为空只发生在
        // 「diff 为空不调 LLM」的合法路径（reviewer.rs run_review），不是数据丢失。
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
                size_label=excluded.size_label, pushed=0,
                comments=COALESCE(excluded.comments, reviews.comments)",
            params![
                r["repo"].as_str().unwrap_or(""),
                r["number"].as_i64().unwrap_or(0),
                r["head_sha"].as_str().unwrap_or(""),
                r["score"].as_f64(),
                dimensions,
                r["summary"].as_str(),
                flagged_files,
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
                p.updated_at, p.recent_commits, p.extras_detail, p.source, p.author_orgs,
                r.score, r.reviewed_at, r.size_label, r.additions, r.deletions,
                p.additions, p.deletions
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
                    "author_orgs": row.get::<_, String>(18)?,
                    "score": row.get::<_, Option<f64>>(19)?,
                    "size_label": row.get::<_, Option<String>>(21)?,
                    // +/- 列：评审记录的优先，否则用 Files 拉取落库的 PR 级增删（没评审过不再是 0）
                    "additions": row.get::<_, Option<i64>>(22)?.or(row.get::<_, Option<i64>>(24)?),
                    "deletions": row.get::<_, Option<i64>>(23)?.or(row.get::<_, Option<i64>>(25)?),
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

    // ── Deep Review：仅本地、按 head SHA 隔离的工作区 ──

    fn deep_finding_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeepFindingData> {
        let finding_json = row.get::<_, String>(0)?;
        let qa_json = row.get::<_, String>(7)?;
        let json_err = |index, e: serde_json::Error| {
            rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
        };
        let mut finding: DeepFindingData =
            serde_json::from_str(&finding_json).map_err(|e| json_err(0, e))?;
        finding.id = row.get(1)?;
        finding.fingerprint = row.get(2)?;
        finding.ordinal = row.get(3)?;
        finding.suggested_text = row.get(4)?;
        finding.draft_text = row.get(5)?;
        finding.discarded = row.get::<_, i64>(6)? != 0;
        finding.qa = serde_json::from_str(&qa_json).map_err(|e| json_err(7, e))?;
        finding.revision = row.get(8)?;
        finding.updated_at = row.get(9)?;
        Ok(finding)
    }

    pub fn pr_head_sha(&self, repo: &str, number: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT head_sha FROM prs WHERE repo=?1 AND number=?2",
                params![repo, number],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn get_deep_finding(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        finding_id: i64,
    ) -> Result<Option<DeepFindingData>> {
        Ok(self
            .conn
            .query_row(
                "SELECT finding_json, id, fingerprint, ordinal, suggested_text, draft_text,
                        discarded, qa_json, revision, updated_at
                 FROM deep_findings
                 WHERE repo=?1 AND number=?2 AND head_sha=?3 AND id=?4",
                params![repo, number, head_sha, finding_id],
                Self::deep_finding_from_row,
            )
            .optional()?)
    }

    pub fn get_deep_workspace(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
    ) -> Result<Option<DeepWorkspaceData>> {
        let run = self
            .conn
            .query_row(
                "SELECT base_sha, title, created_at, body, diff, qa_json FROM deep_reviews
                 WHERE repo=?1 AND number=?2 AND head_sha=?3",
                params![repo, number, head_sha],
                |r| {
                    let qa_json = r.get::<_, String>(5)?;
                    let qa = serde_json::from_str(&qa_json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        qa,
                    ))
                },
            )
            .optional()?;
        let Some((base_sha, title, created_at, body, diff, qa)) = run else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT finding_json, id, fingerprint, ordinal, suggested_text, draft_text,
                    discarded, qa_json, revision, updated_at
             FROM deep_findings
             WHERE repo=?1 AND number=?2 AND head_sha=?3
             ORDER BY ordinal, id",
        )?;
        let findings = stmt
            .query_map(params![repo, number, head_sha], Self::deep_finding_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(DeepWorkspaceData {
            repo: repo.to_string(),
            number,
            base_sha,
            head_sha: head_sha.to_string(),
            title,
            created_at,
            body,
            diff,
            qa,
            findings,
        }))
    }

    pub fn get_latest_deep_workspace(
        &self,
        repo: &str,
        number: i64,
    ) -> Result<Option<DeepWorkspaceData>> {
        let head_sha = self
            .conn
            .query_row(
                "SELECT head_sha FROM deep_reviews
                 WHERE repo=?1 AND number=?2
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![repo, number],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        match head_sha {
            Some(head_sha) => self.get_deep_workspace(repo, number, &head_sha),
            None => Ok(None),
        }
    }

    /// 创建完整快照；同 SHA 重入只读取已有数据，绝不覆盖用户草稿。
    pub fn create_deep_workspace(
        &self,
        workspace: &DeepWorkspaceData,
    ) -> Result<DeepWorkspaceData> {
        let tx = self.conn.unchecked_transaction()?;
        self.conn.execute(
            "INSERT OR IGNORE INTO deep_reviews(
                repo, number, head_sha, base_sha, title, created_at, body, diff, qa_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                workspace.repo,
                workspace.number,
                workspace.head_sha,
                workspace.base_sha,
                workspace.title,
                workspace.created_at,
                workspace.body,
                workspace.diff,
                serde_json::to_string(&workspace.qa)?,
            ],
        )?;
        for finding in &workspace.findings {
            self.conn.execute(
                "INSERT OR IGNORE INTO deep_findings(
                    repo, number, head_sha, fingerprint, ordinal, finding_json,
                    suggested_text, draft_text, discarded, qa_json, revision, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'[]',0,?10)",
                params![
                    workspace.repo,
                    workspace.number,
                    workspace.head_sha,
                    finding.fingerprint,
                    finding.ordinal,
                    serde_json::to_string(finding)?,
                    finding.suggested_text,
                    finding.draft_text,
                    finding.discarded as i64,
                    finding.updated_at,
                ],
            )?;
        }
        tx.commit()?;
        self.get_deep_workspace(&workspace.repo, workspace.number, &workspace.head_sha)?
            .ok_or_else(|| anyhow::anyhow!("Deep Review 工作区创建后读取失败"))
    }

    /// 用刷新合并后的集合替换 Findings；匹配项 ID 保持不变，PR 上下文不动。
    pub fn replace_deep_findings(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        findings: &[DeepFindingData],
    ) -> Result<DeepWorkspaceData> {
        let tx = self.conn.unchecked_transaction()?;
        self.conn.execute(
            "DELETE FROM deep_findings WHERE repo=?1 AND number=?2 AND head_sha=?3",
            params![repo, number, head_sha],
        )?;
        for finding in findings {
            let id = (finding.id > 0).then_some(finding.id);
            self.conn.execute(
                "INSERT INTO deep_findings(
                    id, repo, number, head_sha, fingerprint, ordinal, finding_json,
                    suggested_text, draft_text, discarded, qa_json, revision, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    id,
                    repo,
                    number,
                    head_sha,
                    finding.fingerprint,
                    finding.ordinal,
                    serde_json::to_string(finding)?,
                    finding.suggested_text,
                    finding.draft_text,
                    finding.discarded as i64,
                    serde_json::to_string(&finding.qa)?,
                    finding.revision,
                    finding.updated_at,
                ],
            )?;
        }
        tx.commit()?;
        self.get_deep_workspace(repo, number, head_sha)?
            .ok_or_else(|| anyhow::anyhow!("Deep Review 刷新后读取工作区失败"))
    }

    /// 旧工作区只补齐缺失的 PR 上下文，不覆盖已有内容或 Finding 草稿。
    pub fn fill_deep_context_if_empty(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        body: &str,
        diff: &str,
    ) -> Result<Option<DeepWorkspaceData>> {
        self.conn.execute(
            "UPDATE deep_reviews
             SET body=CASE WHEN body='' THEN ?4 ELSE body END,
                 diff=CASE WHEN diff='' THEN ?5 ELSE diff END
             WHERE repo=?1 AND number=?2 AND head_sha=?3 AND diff=''",
            params![repo, number, head_sha, body, diff],
        )?;
        self.get_deep_workspace(repo, number, head_sha)
    }

    /// PR 级问答与 Finding QA 分离，保留最近三轮往返。
    pub fn append_deep_pr_qa(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        messages: &[DeepQaMessage],
    ) -> Result<Option<Vec<DeepQaMessage>>> {
        let Some(mut workspace) = self.get_deep_workspace(repo, number, head_sha)? else {
            return Ok(None);
        };
        workspace.qa.extend_from_slice(messages);
        if workspace.qa.len() > 6 {
            let drop = workspace.qa.len() - 6;
            workspace.qa.drain(..drop);
        }
        self.conn.execute(
            "UPDATE deep_reviews SET qa_json=?4 WHERE repo=?1 AND number=?2 AND head_sha=?3",
            params![repo, number, head_sha, serde_json::to_string(&workspace.qa)?],
        )?;
        Ok(Some(workspace.qa))
    }

    fn mutate_deep_finding(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        finding_id: i64,
        expected_revision: i64,
        draft_text: Option<&str>,
        discarded: Option<bool>,
    ) -> Result<DeepMutation> {
        let Some(current) = self.get_deep_finding(repo, number, head_sha, finding_id)? else {
            return Ok(DeepMutation { finding: None, conflict: false });
        };
        let mut updated = current.clone();
        if let Some(text) = draft_text {
            updated.draft_text = text.to_string();
        }
        if let Some(value) = discarded {
            updated.discarded = value;
            updated.resolution = if value { "manual".into() } else { String::new() };
        }
        updated.updated_at = now();
        let changed = self.conn.execute(
            "UPDATE deep_findings
             SET finding_json=?5, draft_text=?6, discarded=?7,
                 revision=revision+1, updated_at=?8
             WHERE id=?1 AND repo=?2 AND number=?3 AND head_sha=?4 AND revision=?9",
            params![
                finding_id,
                repo,
                number,
                head_sha,
                serde_json::to_string(&updated)?,
                updated.draft_text,
                updated.discarded as i64,
                updated.updated_at,
                expected_revision,
            ],
        )?;
        let finding = self.get_deep_finding(repo, number, head_sha, finding_id)?;
        Ok(DeepMutation {
            finding,
            conflict: changed == 0,
        })
    }

    pub fn update_deep_draft(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        finding_id: i64,
        expected_revision: i64,
        draft_text: &str,
    ) -> Result<DeepMutation> {
        self.mutate_deep_finding(
            repo,
            number,
            head_sha,
            finding_id,
            expected_revision,
            Some(draft_text),
            None,
        )
    }

    pub fn set_deep_finding_discarded(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        finding_id: i64,
        expected_revision: i64,
        discarded: bool,
    ) -> Result<DeepMutation> {
        self.mutate_deep_finding(
            repo,
            number,
            head_sha,
            finding_id,
            expected_revision,
            None,
            Some(discarded),
        )
    }

    /// 问答独立追加，不改变草稿 revision；仅保留最近三条问答往返。
    pub fn append_deep_qa(
        &self,
        repo: &str,
        number: i64,
        head_sha: &str,
        finding_id: i64,
        messages: &[DeepQaMessage],
    ) -> Result<Option<DeepFindingData>> {
        let Some(mut finding) = self.get_deep_finding(repo, number, head_sha, finding_id)? else {
            return Ok(None);
        };
        finding.qa.extend_from_slice(messages);
        if finding.qa.len() > 6 {
            let drop = finding.qa.len() - 6;
            finding.qa.drain(..drop);
        }
        self.conn.execute(
            "UPDATE deep_findings SET qa_json=?5, updated_at=?6
             WHERE id=?1 AND repo=?2 AND number=?3 AND head_sha=?4",
            params![
                finding_id,
                repo,
                number,
                head_sha,
                serde_json::to_string(&finding.qa)?,
                now(),
            ],
        )?;
        self.get_deep_finding(repo, number, head_sha, finding_id)
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

    /// 批量存在性检查（替代 Inbox 的 N+1 逐行 pr_exists）：返回已存在的 (repo, number) 集合。
    /// 每批 400 键（800 个绑定参数，低于 SQLite 默认 999 上限）。
    pub fn existing_pr_keys(
        &self,
        keys: &[(String, i64)],
    ) -> rusqlite::Result<std::collections::HashSet<(String, i64)>> {
        let mut out = std::collections::HashSet::new();
        for chunk in keys.chunks(400) {
            let ph = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| format!("(?{},?{})", 2 * i + 1, 2 * i + 2))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT repo, number FROM prs WHERE (repo, number) IN ({ph})");
            let mut stmt = self.conn.prepare(&sql)?;
            let flat: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .flat_map(|(r, n)| {
                    [
                        r as &dyn rusqlite::ToSql,
                        n as &dyn rusqlite::ToSql,
                    ]
                })
                .collect();
            let rows = stmt
                .query_map(&*flat, |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// 全部 PR 的 (repo, number)（本地模式手动刷新时逐个刷新 GitHub 状态用）。
    pub fn list_pr_keys(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT repo, number FROM prs")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 全部 diff 缓存（repo, number, diff 文本）：回填 prs.additions/deletions 用。
    pub fn list_diff_cache(&self) -> Result<Vec<(String, i64, String)>> {
        let mut stmt = self.conn.prepare("SELECT repo, number, diff FROM diff_cache")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 回填 PR 的增删行数（只填空值，已有值不动——GitHub files API 的真值优先）。
    pub fn fill_diff_sums_if_empty(&self, repo: &str, number: i64, adds: i64, dels: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE prs SET additions=?1, deletions=?2
             WHERE repo=?3 AND number=?4 AND additions IS NULL",
            params![adds, dels, repo, number],
        )?;
        Ok(())
    }

    /// 全部非空作者名（去重）：刷新时批量补 author_orgs 用。
    pub fn list_authors(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT author FROM prs WHERE author != ''")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 按作者回写公开组织成员关系（JSON 数组字符串；直改 DB，不进 outbox）。
    pub fn set_author_orgs(&self, author: &str, orgs: &[String]) -> Result<()> {
        self.conn.execute(
            "UPDATE prs SET author_orgs=?1 WHERE author=?2",
            params![serde_json::to_string(orgs)?, author],
        )?;
        Ok(())
    }

    /// 只补空缺字段（Inbox「+ 预评审」入库的行缺 author/head_sha 等）：
    /// 空/NULL 才写入，已有值一律不动；行不存在返回 false。
    pub fn fill_pr_meta_if_empty(
        &self,
        repo: &str,
        number: i64,
        author: &str,
        url: &str,
        head_sha: &str,
        title: &str,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE prs SET
                author   = CASE WHEN author   IS NULL OR author   = '' THEN ?1 ELSE author   END,
                url      = CASE WHEN url      IS NULL OR url      = '' THEN ?2 ELSE url      END,
                head_sha = CASE WHEN head_sha IS NULL OR head_sha = '' THEN ?3 ELSE head_sha END,
                title    = CASE WHEN title    IS NULL OR title    = '' THEN ?4 ELSE title    END
             WHERE repo=?5 AND number=?6",
            params![author, url, head_sha, title, repo, number],
        )?;
        Ok(n > 0)
    }

    // ── 本地改动（立即生效 + 入队 outbox 待推送） ──
    pub fn local_pr_update(&self, repo: &str, number: i64, field: &str, value: &serde_json::Value) -> Result<()> {
        // 未知字段在动库之前拒绝（不参与事务）
        if !matches!(field, "status" | "tags" | "priority" | "on_board" | "pinned") {
            anyhow::bail!("unknown field {field}");
        }
        // UPDATE prs + INSERT outbox 必须同生共死（unchecked_transaction：
        // &self 语义，本函数内无嵌套事务/无并发借用同一连接的其他事务）
        let tx = self.conn.unchecked_transaction()?;
        match field {
            "status" => {
                tx.execute(
                    "UPDATE prs SET status=?1, updated_at=?2 WHERE repo=?3 AND number=?4",
                    params![value.as_str().unwrap_or("pending"), now(), repo, number],
                )?;
            }
            "tags" => {
                tx.execute(
                    "UPDATE prs SET tags=?1, updated_at=?2 WHERE repo=?3 AND number=?4",
                    params![serde_json::to_string(value)?, now(), repo, number],
                )?;
            }
            _ => {
                // priority | on_board | pinned（上面已校验白名单，format! 无注入面）
                tx.execute(
                    &format!("UPDATE prs SET {field}=?1, updated_at=?2 WHERE repo=?3 AND number=?4"),
                    params![value.as_i64().unwrap_or(0), now(), repo, number],
                )?;
            }
        }
        // 入队待推送
        tx.execute(
            "INSERT INTO outbox(kind, payload, created_at) VALUES ('pr', ?1, ?2)",
            params![
                serde_json::to_string(&serde_json::json!({
                    "repo": repo, "number": number, field: value,
                    "client_updated_at": now(),
                }))?,
                now(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 读取 outbox 全部待推送条目（id 升序）。损坏的 payload（非合法 JSON）
    /// 跳过并告警——unwrap_or_default 的 Null 会推送 repo=""/number=0 的垃圾。
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
            .filter_map(|(id, p)| match serde_json::from_str(&p) {
                Ok(v) => Some((id, v)),
                Err(e) => {
                    tracing::warn!("outbox #{id} payload 损坏，跳过推送：{e}");
                    None
                }
            })
            .collect())
    }

    pub fn outbox_remove(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM outbox WHERE id=?1", params![id])?;
        Ok(())
    }

    // ── 跨档案复制 ──

    /// 单个 PR 的全部原始字段（显式列名；跨档案复制的数据源——list_prs 的联表视图
    /// 缺 head_sha/extras_updated_at，不能用来复制）。产出形状与 upsert 系输入兼容。
    pub fn get_pr_full(&self, repo: &str, number: i64) -> Result<Option<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo, number, title, author, url, head_sha, status, tags, role,
                on_board, priority, pinned, ci_status, review_decision, turn, updated_at,
                recent_commits, extras_detail, extras_updated_at, source, author_orgs,
                additions, deletions
             FROM prs WHERE repo=?1 AND number=?2",
        )?;
        let mut rows = stmt.query(params![repo, number])?;
        let Some(r) = rows.next()? else { return Ok(None) };
        Ok(Some(serde_json::json!({
            "repo": r.get::<_, String>(0)?,
            "number": r.get::<_, i64>(1)?,
            "title": r.get::<_, Option<String>>(2)?,
            "author": r.get::<_, Option<String>>(3)?,
            "url": r.get::<_, Option<String>>(4)?,
            "head_sha": r.get::<_, Option<String>>(5)?,
            "status": r.get::<_, String>(6)?,
            "tags": r.get::<_, String>(7)?,
            "role": r.get::<_, String>(8)?,
            "on_board": r.get::<_, i64>(9)?,
            "priority": r.get::<_, i64>(10)?,
            "pinned": r.get::<_, i64>(11)?,
            "ci_status": r.get::<_, Option<String>>(12)?,
            "review_decision": r.get::<_, Option<String>>(13)?,
            "turn": r.get::<_, Option<String>>(14)?,
            "updated_at": r.get::<_, Option<String>>(15)?,
            "recent_commits": r.get::<_, String>(16)?,
            "extras_detail": r.get::<_, Option<String>>(17)?,
            "extras_updated_at": r.get::<_, Option<String>>(18)?,
            "source": r.get::<_, String>(19)?,
            "author_orgs": r.get::<_, String>(20)?,
            "additions": r.get::<_, Option<i64>>(21)?,
            "deletions": r.get::<_, Option<i64>>(22)?,
        })))
    }

    /// 一个 PR 的全部评审记录（reviews 主键含 head_sha，同一 PR 可能多条）。
    /// 产出形状与 upsert_review 输入兼容（字符串列透传，json_or_str/COALESCE 路径已兼容）。
    pub fn list_reviews_of(&self, repo: &str, number: i64) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo, number, head_sha, score, dimensions, summary, flagged_files,
                reviewed_at, high_value, oversized, files_changed, additions, deletions,
                size_label, comments
             FROM reviews WHERE repo=?1 AND number=?2 ORDER BY reviewed_at",
        )?;
        let rows = stmt
            .query_map(params![repo, number], |r| {
                Ok(serde_json::json!({
                    "repo": r.get::<_, String>(0)?,
                    "number": r.get::<_, i64>(1)?,
                    "head_sha": r.get::<_, String>(2)?,
                    "score": r.get::<_, Option<f64>>(3)?,
                    "dimensions": r.get::<_, Option<String>>(4)?,
                    "summary": r.get::<_, Option<String>>(5)?,
                    "flagged_files": r.get::<_, Option<String>>(6)?,
                    "reviewed_at": r.get::<_, Option<String>>(7)?,
                    "high_value": r.get::<_, i64>(8)?,
                    "oversized": r.get::<_, i64>(9)?,
                    "files_changed": r.get::<_, Option<i64>>(10)?,
                    "additions": r.get::<_, Option<i64>>(11)?,
                    "deletions": r.get::<_, Option<i64>>(12)?,
                    "size_label": r.get::<_, Option<String>>(13)?,
                    "comments": r.get::<_, Option<String>>(14)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 仅当目标库不存在该 PR 时插入（跨档案复制用）。已存在则整行不动——
    /// 目标档案的 pinned/priority/on_board/status 等本地策展状态绝不能被源覆盖。
    /// 返回 true = 本次新插入。
    pub fn insert_pr_if_absent(&self, p: &serde_json::Value) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO prs(repo, number, title, author, url, head_sha, status, tags, role,
                on_board, priority, pinned, ci_status, review_decision, turn, updated_at,
                recent_commits, extras_detail, extras_updated_at, source, author_orgs,
                additions, deletions)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
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
                p["author_orgs"].as_str().unwrap_or("[]"),
                p["additions"].as_i64(),
                p["deletions"].as_i64(),
            ],
        )?;
        Ok(n > 0)
    }

    /// 跨档案复制：keys 指定的 PR 行 + 各自全部评审记录写入 dst。
    /// prs 走 insert_pr_if_absent（目标已有行整行跳过，本地策展状态不动）；
    /// 评审走 upsert_review（同 head_sha 幂等覆盖同内容，comments COALESCE 安全）。
    /// 源库只读；目标库写入包在一个事务里（unchecked_transaction 是连接级事务，
    /// 后续经 self.conn 的写入都在事务内——与 local_pr_update 同款）。
    /// 返回 (新插入 PR 数, 已存在跳过数, 复制评审数)。
    pub fn copy_prs_to(src: &Db, dst: &Db, keys: &[(String, i64)]) -> Result<(usize, usize, usize)> {
        // 先全部从源库读出（纯读，无需事务；复制中途源库变化最多拿到稍旧的行，可接受）
        let mut prs = Vec::new();
        let mut reviews = Vec::new();
        for (repo, number) in keys {
            if let Some(p) = src.get_pr_full(repo, *number)? {
                prs.push(p);
            }
            reviews.extend(src.list_reviews_of(repo, *number)?);
        }
        let _tx = dst.conn.unchecked_transaction()?;
        let mut inserted = 0usize;
        for p in &prs {
            if dst.insert_pr_if_absent(p)? {
                inserted += 1;
            }
        }
        for r in &reviews {
            dst.upsert_review(r)?;
        }
        _tx.commit()?;
        Ok((inserted, prs.len() - inserted, reviews.len()))
    }

    // ── 删除 / 墓碑（local 模式） ──

    /// 批量删除 PR：墓碑 + prs + reviews + diff_cache + Deep Review 本地工作区一个事务。
    /// 返回实际删除的 PR 行数（墓碑对全部 keys 记账——已不存在的键删了也安全）。
    pub fn delete_prs(&self, keys: &[(String, i64)]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut deleted = 0usize;
        for (repo, number) in keys {
            self.conn.execute(
                "INSERT OR REPLACE INTO deleted_prs(repo, number, deleted_at) VALUES (?1,?2,?3)",
                params![repo, number, now()],
            )?;
            deleted += self.conn.execute(
                "DELETE FROM prs WHERE repo=?1 AND number=?2",
                params![repo, number],
            )?;
            self.conn.execute(
                "DELETE FROM reviews WHERE repo=?1 AND number=?2",
                params![repo, number],
            )?;
            self.conn.execute(
                "DELETE FROM diff_cache WHERE repo=?1 AND number=?2",
                params![repo, number],
            )?;
            self.conn.execute(
                "DELETE FROM deep_findings WHERE repo=?1 AND number=?2",
                params![repo, number],
            )?;
            self.conn.execute(
                "DELETE FROM deep_reviews WHERE repo=?1 AND number=?2",
                params![repo, number],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// 墓碑查询：该 (repo, number) 是否已被本地删除。
    /// 所有「自动/半自动」插入 prs 的路径（如 Inbox + 预评审）必须先过这关。
    pub fn is_tombstoned(&self, repo: &str, number: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM deleted_prs WHERE repo=?1 AND number=?2",
            params![repo, number],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// 带墓碑守卫的 upsert：已删除的 PR 返回 Ok(false) 不插入（显式复制 copy_prs_to
    /// 不走这里——跨档案复制是明确的用户意图，允许复活）。
    pub fn upsert_pr_unless_tombstoned(&self, p: &serde_json::Value) -> Result<bool> {
        if self.is_tombstoned(p["repo"].as_str().unwrap_or(""), p["number"].as_i64().unwrap_or(0))? {
            return Ok(false);
        }
        self.upsert_pr(p)?;
        Ok(true)
    }

    pub fn tombstone_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM deleted_prs", [], |r| r.get(0))?)
    }

    /// 清空墓碑（「清空删除记录」入口）：之后刷新/手动加入可正常回库。
    /// 返回清掉的条数。
    pub fn clear_tombstones(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM deleted_prs", [])?)
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
    fn local_pr_update_batch_loop_writes_all_and_enqueues_all() {
        // 批量操作的底层语义：对选中集逐行 local_pr_update —— 每行 UPDATE 立即生效
        // 且每行进一条 outbox（同事务）；白名单外字段整批拒绝。
        let db = Db::open_memory().unwrap();
        for n in [1, 2, 3] {
            db.upsert_pr(&pr_json_src("o/r", n, &format!("t{n}"), "github")).unwrap();
        }
        let keys: Vec<(String, i64)> = (1..=3).map(|n| ("o/r".to_string(), n)).collect();
        for (repo, number) in &keys {
            db.local_pr_update(repo, *number, "priority", &serde_json::json!(2)).unwrap();
        }
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r["priority"] == 2));
        let items = db.outbox_drain().unwrap();
        assert_eq!(items.len(), 3, "每行一条 outbox，待推送数 = 批量大小");
        assert!(items.iter().all(|(_, p)| p["priority"] == 2));
        // 白名单：未知字段拒绝且不进 outbox
        assert!(db.local_pr_update("o/r", 1, "evil_field", &serde_json::json!(1)).is_err());
        assert_eq!(db.outbox_drain().unwrap().len(), 3);
    }

    #[test]
    fn copy_prs_to_carries_prs_and_reviews_without_clobbering_target() {
        // 跨档案复制：prs（全部字段，含 head_sha）+ 全部评审记录（一个 PR 可多 sha 多条）。
        // 目标已有行整行跳过（本地策展状态不动）；重复复制幂等。
        let src = Db::open_memory().unwrap();
        let dst = Db::open_memory().unwrap();
        src.upsert_pr(&pr_json_src("o/r", 1, "src pr1", "server")).unwrap();
        src.upsert_pr(&pr_json_src("o/r", 2, "src pr2", "server")).unwrap();
        src.upsert_review(&review_json("o/r", 1, "sha-a", 8.0)).unwrap();
        src.upsert_review(&review_json("o/r", 1, "sha-b", 6.5)).unwrap();
        // 目标档案已有 o/r#2，且带本地策展状态（pinned/priority/status 是本地改的）
        dst.upsert_pr(&pr_json_src("o/r", 2, "dst pr2", "github")).unwrap();
        dst.local_pr_update("o/r", 2, "pinned", &serde_json::json!(1)).unwrap();
        dst.local_pr_update("o/r", 2, "priority", &serde_json::json!(3)).unwrap();
        dst.local_pr_update("o/r", 2, "status", &serde_json::json!("reviewed")).unwrap();

        let keys = vec![("o/r".to_string(), 1), ("o/r".to_string(), 2)];
        let (inserted, skipped, n_reviews) = Db::copy_prs_to(&src, &dst, &keys).unwrap();
        assert_eq!((inserted, skipped, n_reviews), (1, 1, 2));

        // 新插入行全字段带过来（含 list_prs 视图里没有的 head_sha）
        let r1 = dst.get_pr_full("o/r", 1).unwrap().unwrap();
        assert_eq!(r1["title"], "src pr1");
        assert_eq!(r1["head_sha"], "s");
        assert_eq!(r1["source"], "server");
        // 目标已有行的本地策展状态不被覆盖
        let r2 = dst.get_pr_full("o/r", 2).unwrap().unwrap();
        assert_eq!(r2["pinned"], 1);
        assert_eq!(r2["priority"], 3);
        assert_eq!(r2["status"], "reviewed");
        assert_eq!(r2["title"], "dst pr2"); // 连标题都不动（整行跳过，不是补空缺）
        // 评审带全：同一 PR 两条不同 head_sha
        let rvs = dst.list_reviews_of("o/r", 1).unwrap();
        assert_eq!(rvs.len(), 2);
        assert!(rvs.iter().any(|r| r["head_sha"] == "sha-a" && r["score"] == 8.0));
        assert!(rvs.iter().any(|r| r["head_sha"] == "sha-b" && r["score"] == 6.5));

        // 重复复制幂等：PR 全部跳过，评审 upsert 同内容不翻倍
        let (inserted2, skipped2, n_reviews2) = Db::copy_prs_to(&src, &dst, &keys).unwrap();
        assert_eq!((inserted2, skipped2, n_reviews2), (0, 2, 2));
        assert_eq!(dst.list_reviews_of("o/r", 1).unwrap().len(), 2);
    }

    #[test]
    fn delete_prs_removes_row_reviews_diff_and_blocks_resurrection() {
        // 删除 = 墓碑 + prs/reviews/diff_cache 一个事务清掉；
        // 防复活：删除后走墓碑守卫的插入路径（Inbox + 预评审 / 未来刷新回插）不得复活；
        // 清空墓碑后可正常回库。
        let db = Db::open_memory().unwrap();
        db.upsert_pr(&pr_json("o/r", 1, "t1")).unwrap();
        db.upsert_pr(&pr_json("o/r", 2, "t2")).unwrap();
        db.upsert_review(&review_json("o/r", 1, "sha-a", 8.0)).unwrap();
        db.put_diff("o/r", 1, "diff text").unwrap();

        let keys = vec![("o/r".to_string(), 1)];
        let deleted = db.delete_prs(&keys).unwrap();
        assert_eq!(deleted, 1);
        assert!(!db.pr_exists("o/r", 1).unwrap());
        assert!(db.pr_exists("o/r", 2).unwrap(), "未删除的行不受影响");
        assert_eq!(db.list_reviews_of("o/r", 1).unwrap().len(), 0, "评审随删");
        assert!(db.get_diff("o/r", 1).unwrap().is_none(), "diff 缓存随删");
        assert!(db.is_tombstoned("o/r", 1).unwrap());
        assert!(!db.is_tombstoned("o/r", 2).unwrap());
        assert_eq!(db.tombstone_count().unwrap(), 1);

        // 模拟刷新/误加回插：守卫路径拒绝复活，普通 upsert 也先查墓碑（worker 侧模式）
        assert!(!db.upsert_pr_unless_tombstoned(&pr_json("o/r", 1, "resurrected")).unwrap());
        assert!(!db.pr_exists("o/r", 1).unwrap(), "墓碑行不得复活");
        // 幂等：重复删除不炸，墓碑不翻倍
        assert_eq!(db.delete_prs(&keys).unwrap(), 0);
        assert_eq!(db.tombstone_count().unwrap(), 1);

        // 清空删除记录后可恢复
        assert_eq!(db.clear_tombstones().unwrap(), 1);
        assert_eq!(db.tombstone_count().unwrap(), 0);
        assert!(db.upsert_pr_unless_tombstoned(&pr_json("o/r", 1, "back")).unwrap());
        assert!(db.pr_exists("o/r", 1).unwrap());
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
    fn upsert_review_object_dimensions_survive() {
        // 本地评审产出的 dimensions/flagged_files 是 JSON 对象/数组而非字符串，
        // 落库必须序列化（此前用 as_str() 存成 NULL，本地评审的维度评分和风险文件全丢）
        let db = Db::open_memory().unwrap();
        db.upsert_review(&serde_json::json!({
            "repo": "o/r", "number": 1, "head_sha": "s1", "score": 8.2,
            "dimensions": {"readability": 8, "security": 9},
            "summary": "s", "flagged_files": [{"path": "a.rs", "issue": "x"}],
            "reviewed_at": "t",
        }))
        .unwrap();
        let r = db.get_review("o/r", 1).unwrap().unwrap();
        // get_review 返回 TEXT 原样（UI 侧 json_field 兼容字符串/内联对象两种形态），
        // 断言字符串里是序列化后的对象内容而非 NULL
        let dims = r["dimensions"].as_str().expect("dimensions 不应为 NULL");
        assert!(dims.contains("\"readability\":8"), "dims={dims}");
        let flagged = r["flagged_files"].as_str().expect("flagged_files 不应为 NULL");
        assert!(flagged.contains("a.rs"), "flagged={flagged}");
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

    #[test]
    fn fill_pr_meta_only_fills_empty_fields() {        let db = Db::open_memory().unwrap();
        // Inbox 入库的残缺行：author/head_sha 为空
        db.upsert_pr(&serde_json::json!({
            "repo": "o/r", "number": 1, "title": "t", "author": "", "url": "u1",
            "head_sha": "", "status": "pending", "updated_at": "t", "source": "inbox",
        }))
        .unwrap();
        assert!(db.fill_pr_meta_if_empty("o/r", 1, "alice", "u2", "sha1", "t2").unwrap());
        let row = &db.list_prs(&[]).unwrap()[0];
        assert_eq!(row["author"], "alice");       // 空字段被补齐
        assert_eq!(row["url"], "u1");             // 已有值不动
        assert_eq!(row["title"], "t");
        // head_sha 不在 list_prs 的选择列里，直接查表验证
        let sha: String = db.conn.query_row(
            "SELECT head_sha FROM prs WHERE repo='o/r' AND number=1", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(sha, "sha1");
        // 再补一次：全部已有值 → 什么都不变
        assert!(db.fill_pr_meta_if_empty("o/r", 1, "bob", "u3", "sha2", "t3").unwrap());
        let row = &db.list_prs(&[]).unwrap()[0];
        assert_eq!(row["author"], "alice");
        let sha: String = db.conn.query_row(
            "SELECT head_sha FROM prs WHERE repo='o/r' AND number=1", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(sha, "sha1");
        // 不存在的行：false
        assert!(!db.fill_pr_meta_if_empty("o/r", 99, "x", "u", "s", "t").unwrap());
    }

    #[test]
    fn now_is_utc_rfc3339() {
        // 与 GitHub meta 的 RFC3339（带 Z）同一格式，字典序可直接当时间序
        let ts = now();
        assert!(ts.ends_with('Z'), "ts={ts}");
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok());
    }

    #[test]
    fn upsert_review_keeps_old_comments_when_new_is_null() {
        // 重复评审时 OCR 失败 → 新行 comments=NULL → 不清空已有行级评论
        let db = Db::open_memory().unwrap();
        db.upsert_review(&review_json("o/r", 1, "s1", 8.5)).unwrap();
        let mut bare = review_json("o/r", 1, "s1", 9.0);
        bare.as_object_mut().unwrap().remove("comments");
        db.upsert_review(&bare).unwrap();
        let r = db.get_review("o/r", 1).unwrap().unwrap();
        let comments = r["comments"].as_array().expect("旧 comments 不应被 NULL 覆盖");
        assert_eq!(comments[0]["path"], "src/a.rs");
        assert_eq!(r["score"], 9.0); // 其余字段正常被新评审覆盖
    }

    #[test]
    fn local_pr_update_rolls_back_on_outbox_failure() {
        let db = Db::open_memory().unwrap();
        db.upsert_pr(&pr_json("o/r", 1, "t")).unwrap();
        // 拆掉 outbox 表让 INSERT 失败 → 事务整体回滚，prs.status 不变
        db.conn.execute_batch("DROP TABLE outbox").unwrap();
        assert!(db.local_pr_update("o/r", 1, "status", &serde_json::json!("reviewed")).is_err());
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows[0]["status"], "pending");
        // 未知字段：动库前拒绝
        assert!(db.local_pr_update("o/r", 1, "bogus", &serde_json::json!(1)).is_err());
        assert_eq!(db.list_prs(&[]).unwrap()[0]["status"], "pending");
    }

    #[test]
    fn outbox_drain_skips_corrupt_payload() {
        let db = Db::open_memory().unwrap();
        db.local_pr_update("o/r", 1, "pinned", &serde_json::json!(1)).unwrap();
        db.conn
            .execute("INSERT INTO outbox(kind, payload, created_at) VALUES ('pr', '{bad', 't')", [])
            .unwrap();
        db.local_pr_update("o/r", 2, "priority", &serde_json::json!(2)).unwrap();
        let items = db.outbox_drain().unwrap();
        // 损坏条目跳过，好条目保留（含顺序）
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].1["pinned"], 1);
        assert_eq!(items[1].1["priority"], 2);
    }

    #[test]
    fn existing_pr_keys_batch() {
        let db = Db::open_memory().unwrap();
        db.upsert_pr(&pr_json("o/r", 1, "t")).unwrap();
        db.upsert_pr(&pr_json("o/r", 3, "t")).unwrap();
        let keys = vec![
            ("o/r".to_string(), 1),
            ("o/r".to_string(), 2),
            ("o/r".to_string(), 3),
            ("x/y".to_string(), 1),
        ];
        let got = db.existing_pr_keys(&keys).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&("o/r".to_string(), 1)));
        assert!(got.contains(&("o/r".to_string(), 3)));
        // 空输入 / 超出一批的分块
        assert!(db.existing_pr_keys(&[]).unwrap().is_empty());
        let many: Vec<(String, i64)> = (0..900).map(|i| ("o/r".to_string(), i)).collect();
        let got = db.existing_pr_keys(&many).unwrap();
        assert_eq!(got.len(), 2);
    }

    fn deep_workspace(head_sha: &str) -> DeepWorkspaceData {
        DeepWorkspaceData {
            repo: "o/r".into(),
            number: 7,
            base_sha: "base".into(),
            head_sha: head_sha.into(),
            title: "Deep test".into(),
            created_at: format!("2026-01-01T00:00:0{}Z", if head_sha == "h1" { 1 } else { 2 }),
            body: "whole PR body".into(),
            diff: "diff --git a/src/a.rs b/src/a.rs".into(),
            qa: Vec::new(),
            findings: vec![DeepFindingData {
                fingerprint: format!("fp-{head_sha}"),
                ordinal: 1,
                path: "src/a.rs".into(),
                start_line: 10,
                end_line: 10,
                severity: "high".into(),
                category: "bug".into(),
                content: "problem".into(),
                suggested_text: "initial".into(),
                draft_text: "initial".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn deep_workspace_roundtrip_preserves_drafts_and_rejects_stale_writes() {
        let db = Db::open_memory().unwrap();
        let created = db.create_deep_workspace(&deep_workspace("h1")).unwrap();
        let id = created.findings[0].id;
        assert!(id > 0);

        let saved = db.update_deep_draft("o/r", 7, "h1", id, 0, "edited").unwrap();
        assert!(!saved.conflict);
        assert_eq!(saved.finding.unwrap().revision, 1);
        let stale = db.update_deep_draft("o/r", 7, "h1", id, 0, "overwrite").unwrap();
        assert!(stale.conflict);
        assert_eq!(stale.finding.unwrap().draft_text, "edited");

        let discarded = db.set_deep_finding_discarded("o/r", 7, "h1", id, 1, true).unwrap();
        assert!(!discarded.conflict);
        let current = discarded.finding.unwrap();
        assert!(current.discarded);
        assert_eq!(current.revision, 2);
        let restored = db.set_deep_finding_discarded("o/r", 7, "h1", id, 2, false).unwrap();
        assert!(!restored.finding.unwrap().discarded);

        let messages = (0..8)
            .map(|i| DeepQaMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                content: format!("m{i}"),
                created_at: now(),
            })
            .collect::<Vec<_>>();
        let asked = db.append_deep_qa("o/r", 7, "h1", id, &messages).unwrap().unwrap();
        assert_eq!(asked.qa.len(), 6);
        assert_eq!(asked.qa[0].content, "m2");
        assert_eq!(asked.revision, 3);
        assert_eq!(asked.draft_text, "edited");

        let reopened = db.create_deep_workspace(&deep_workspace("h1")).unwrap();
        assert_eq!(reopened.findings[0].draft_text, "edited");
        assert_eq!(reopened.findings[0].revision, 3);
        assert_eq!(reopened.findings[0].qa.len(), 6);
        assert!(db.unpushed_reviews().unwrap().is_empty());
    }

    #[test]
    fn deep_pr_context_backfill_and_qa_do_not_touch_finding_drafts() {
        let db = Db::open_memory().unwrap();
        let created = db.create_deep_workspace(&deep_workspace("h1")).unwrap();
        let id = created.findings[0].id;
        db.update_deep_draft("o/r", 7, "h1", id, 0, "edited draft").unwrap();
        db.conn.execute(
            "UPDATE deep_reviews SET body='', diff='' WHERE repo='o/r' AND number=7 AND head_sha='h1'",
            [],
        ).unwrap();

        let filled = db
            .fill_deep_context_if_empty("o/r", 7, "h1", "backfilled body", "backfilled diff")
            .unwrap()
            .unwrap();
        assert_eq!(filled.body, "backfilled body");
        assert_eq!(filled.diff, "backfilled diff");
        assert_eq!(filled.findings[0].draft_text, "edited draft");
        assert_eq!(filled.findings[0].revision, 1);
        let refilled = db
            .fill_deep_context_if_empty("o/r", 7, "h1", "replacement", "replacement")
            .unwrap()
            .unwrap();
        assert_eq!(refilled.body, "backfilled body");
        assert_eq!(refilled.diff, "backfilled diff");

        let finding_qa = [DeepQaMessage {
            role: "user".into(),
            content: "finding question".into(),
            created_at: now(),
        }];
        db.append_deep_qa("o/r", 7, "h1", id, &finding_qa).unwrap();
        let pr_qa = [
            DeepQaMessage {
                role: "user".into(),
                content: "PR question".into(),
                created_at: now(),
            },
            DeepQaMessage {
                role: "assistant".into(),
                content: "PR answer".into(),
                created_at: now(),
            },
        ];
        let qa = db.append_deep_pr_qa("o/r", 7, "h1", &pr_qa).unwrap().unwrap();
        assert_eq!(qa.len(), 2);
        let reopened = db.get_deep_workspace("o/r", 7, "h1").unwrap().unwrap();
        assert_eq!(reopened.qa.len(), 2);
        assert_eq!(reopened.findings[0].qa.len(), 1);
        assert_eq!(reopened.findings[0].draft_text, "edited draft");
        assert_eq!(reopened.findings[0].revision, 1);
        assert_eq!(reopened.diff, "backfilled diff");
    }

    #[test]
    fn replace_deep_findings_preserves_identity_history_and_pr_context() {
        let db = Db::open_memory().unwrap();
        let created = db.create_deep_workspace(&deep_workspace("h1")).unwrap();
        let old_id = created.findings[0].id;
        db.update_deep_draft("o/r", 7, "h1", old_id, 0, "keep this draft").unwrap();
        db.append_deep_qa("o/r", 7, "h1", old_id, &[DeepQaMessage {
            role: "user".into(),
            content: "keep this question".into(),
            created_at: now(),
        }]).unwrap();
        db.append_deep_pr_qa("o/r", 7, "h1", &[DeepQaMessage {
            role: "user".into(),
            content: "keep PR context".into(),
            created_at: now(),
        }]).unwrap();
        let current = db.get_deep_workspace("o/r", 7, "h1").unwrap().unwrap();
        let new_finding = DeepFindingData {
            fingerprint: "new-finding".into(),
            ordinal: 1,
            path: "src/new.rs".into(),
            start_line: 4,
            end_line: 4,
            severity: "medium".into(),
            category: "test".into(),
            content: "new issue".into(),
            suggested_text: "new suggestion".into(),
            draft_text: "new suggestion".into(),
            updated_at: now(),
            ..Default::default()
        };
        let merged = crate::deep_review::merge_refreshed_findings(
            &current.findings,
            std::slice::from_ref(&new_finding),
        );
        let refreshed = db.replace_deep_findings("o/r", 7, "h1", &merged).unwrap();
        assert_eq!(refreshed.findings.len(), 2);
        assert_eq!(refreshed.body, current.body);
        assert_eq!(refreshed.diff, current.diff);
        assert_eq!(refreshed.qa.len(), 1);
        let old = refreshed
            .findings
            .iter()
            .find(|finding| finding.id == old_id)
            .unwrap();
        assert_eq!(old.draft_text, "keep this draft");
        assert_eq!(old.qa[0].content, "keep this question");
        assert_eq!(old.resolution, "not_found");
        assert!(old.discarded);
        let new = refreshed
            .findings
            .iter()
            .find(|finding| finding.fingerprint == "new-finding")
            .unwrap();
        assert!(new.id > 0);
        assert!(!new.discarded);
    }

    #[test]
    fn deep_workspace_separates_heads_and_delete_cleans_local_only_data() {
        let source = Db::open_memory().unwrap();
        source.create_deep_workspace(&deep_workspace("h1")).unwrap();
        source.create_deep_workspace(&deep_workspace("h2")).unwrap();
        assert_eq!(
            source.get_latest_deep_workspace("o/r", 7).unwrap().unwrap().head_sha,
            "h2"
        );
        assert_eq!(source.get_deep_workspace("o/r", 7, "h1").unwrap().unwrap().findings.len(), 1);

        let target = Db::open_memory().unwrap();
        Db::copy_prs_to(&source, &target, &[("o/r".into(), 7)]).unwrap();
        assert!(target.get_latest_deep_workspace("o/r", 7).unwrap().is_none());

        source.delete_prs(&[("o/r".into(), 7)]).unwrap();
        assert!(source.get_latest_deep_workspace("o/r", 7).unwrap().is_none());
        assert!(source.get_deep_workspace("o/r", 7, "h1").unwrap().is_none());
    }

    #[test]
    fn migrate_v3_adds_deep_workspace_tables() {
        let dir = std::env::temp_dir().join(format!("prdesk-db-mig-deep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let version: i32 = db.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        for table in ["deep_reviews", "deep_findings"] {
            let count: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        }
        let columns = {
            let mut stmt = db.conn.prepare("PRAGMA table_info(deep_reviews)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        for column in ["body", "diff", "qa_json"] {
            assert!(columns.iter().any(|c| c == column));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
