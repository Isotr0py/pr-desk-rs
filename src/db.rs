//! 本地 SQLite 层：schema 对齐插件（prs/reviews/todos）+ sync_state 游标 + outbox 改动队列
//! + diff_cache。迁移用 PRAGMA user_version。

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

pub const SCHEMA_VERSION: i32 = 8;

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

const MIGRATE_V8: &str = r#"
DROP TABLE IF EXISTS diff_cache;
CREATE TABLE diff_cache (
    repo TEXT NOT NULL, number INTEGER NOT NULL, head_sha TEXT NOT NULL,
    diff TEXT NOT NULL, fetched_at TEXT,
    PRIMARY KEY (repo, number, head_sha)
);
CREATE TABLE agent_reviews (
    repo TEXT NOT NULL, number INTEGER NOT NULL,
    session_id TEXT, workspace TEXT,
    requested_base_sha TEXT, requested_head_sha TEXT,
    reviewed_base_sha TEXT, reviewed_head_sha TEXT,
    active_run_id INTEGER, status TEXT, verdict TEXT, summary TEXT, validation TEXT,
    last_ci_status TEXT, acknowledged_at TEXT, error TEXT,
    created_at TEXT, updated_at TEXT,
    PRIMARY KEY (repo, number)
);
CREATE TABLE agent_review_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL, number INTEGER NOT NULL, kind TEXT NOT NULL, status TEXT NOT NULL,
    base_sha TEXT, head_sha TEXT NOT NULL, parent_head_sha TEXT,
    session_id TEXT, workspace TEXT, scope_complete INTEGER,
    report_json TEXT, error TEXT, created_at TEXT, started_at TEXT, finished_at TEXT
);
CREATE TABLE agent_findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL, number INTEGER NOT NULL, fingerprint TEXT NOT NULL,
    severity TEXT, category TEXT, path TEXT, start_line INTEGER, end_line INTEGER,
    title TEXT, evidence TEXT, suggestion TEXT, status TEXT, human_status TEXT,
    first_head_sha TEXT, last_head_sha TEXT, resolution_note TEXT, updated_at TEXT,
    UNIQUE (repo, number, fingerprint)
);
CREATE TABLE agent_decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL, number INTEGER NOT NULL, run_id INTEGER,
    kind TEXT, title TEXT, context TEXT, options_json TEXT,
    review_event TEXT, merge_method TEXT, status TEXT, answer TEXT, result TEXT,
    created_at TEXT, resolved_at TEXT
);
CREATE INDEX idx_agent_review_runs_queue ON agent_review_runs(status, created_at, id);
CREATE INDEX idx_agent_review_runs_pr ON agent_review_runs(repo, number, id);
CREATE UNIQUE INDEX idx_agent_review_runs_active ON agent_review_runs(repo, number)
    WHERE status IN ('queued', 'running', 'preparing', 'validating', 'waiting_decision');
CREATE INDEX idx_agent_findings_pr ON agent_findings(repo, number, status);
CREATE INDEX idx_agent_decisions_pr ON agent_decisions(repo, number, status);
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertPrOutcome {
    Inserted,
    AlreadyExists,
    Tombstoned,
}

pub struct Db {
    pub conn: Connection,
}

/// UTC RFC3339（"YYYY-MM-DDTHH:MM:SSZ"）：与 GitHub meta 落库格式一致，
/// 字符串字典序即时间序（list_prs / 最新评审靠 ORDER BY 文本排序）。
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn validate_database(path: &std::path::Path) -> Result<()> {
    anyhow::ensure!(path.is_file(), "数据库文件不存在：{}", path.display());
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    anyhow::ensure!(
        integrity.eq_ignore_ascii_case("ok"),
        "SQLite 完整性检查失败：{integrity}"
    );
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    anyhow::ensure!(
        version <= SCHEMA_VERSION,
        "数据库版本 {version} 高于当前支持的版本 {SCHEMA_VERSION}"
    );
    let has_prs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='prs'",
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(has_prs == 1, "不是有效的 PR Desk 数据库：缺少 prs 表");
    Ok(())
}

/// 用 SQLite 在线备份生成包含 WAL 内容的单文件快照，目标已存在时绝不覆盖。
pub fn backup_database(source: &std::path::Path, destination: &std::path::Path) -> Result<()> {
    anyhow::ensure!(source.is_file(), "数据库文件不存在：{}", source.display());
    anyhow::ensure!(
        !destination.exists(),
        "目标文件已存在：{}",
        destination.display()
    );
    if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let result = (|| -> Result<()> {
        let conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.execute("VACUUM INTO ?1", params![destination.to_string_lossy()])?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
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
            conn.execute_batch(
                "ALTER TABLE prs ADD COLUMN author_orgs TEXT NOT NULL DEFAULT '[]'",
            )?;
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
        // v2 → v3：IF NOT EXISTS 天然幂等
        conn.execute_batch(MIGRATE_V3)?;
        if version < 8 {
            // diff 是按 head 派生的缓存，旧键无法可靠补齐 head_sha，迁移时直接清空重建。
            conn.execute_batch(MIGRATE_V8)?;
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
                let dims: Option<serde_json::Value> = r
                    .get::<_, Option<String>>(4)?
                    .and_then(|s| serde_json::from_str(&s).ok());
                let flagged: Option<serde_json::Value> = r
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str(&s).ok());
                let comments: Option<serde_json::Value> = r
                    .get::<_, Option<String>>(14)?
                    .and_then(|s| serde_json::from_str(&s).ok());
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
                .flat_map(|(r, n)| [r as &dyn rusqlite::ToSql, n as &dyn rusqlite::ToSql])
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

    /// 全部 diff 缓存（repo, number, head_sha, diff 文本）：回填 prs.additions/deletions 用。
    pub fn list_diff_cache(&self) -> Result<Vec<(String, i64, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT repo, number, head_sha, diff FROM diff_cache")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 回填 PR 的增删行数（只填空值，已有值不动——GitHub files API 的真值优先）。
    pub fn fill_diff_sums_if_empty(
        &self,
        repo: &str,
        number: i64,
        adds: i64,
        dels: i64,
    ) -> Result<()> {
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
    pub fn local_pr_update(
        &self,
        repo: &str,
        number: i64,
        field: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        // 未知字段在动库之前拒绝（不参与事务）
        if !matches!(
            field,
            "status" | "tags" | "priority" | "on_board" | "pinned"
        ) {
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
                    &format!(
                        "UPDATE prs SET {field}=?1, updated_at=?2 WHERE repo=?3 AND number=?4"
                    ),
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
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
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
        self.conn
            .execute("DELETE FROM outbox WHERE id=?1", params![id])?;
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
        let Some(r) = rows.next()? else {
            return Ok(None);
        };
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

    /// 跨档案复制：keys 指定的 PR 行 + 各自全部普通评审和 agent 当前快照写入 dst。
    /// prs 走 insert_pr_if_absent（目标已有行整行跳过，本地策展状态不动）。
    /// agent run 历史不跨档案复制；进行中的 review 仅复制当前报告状态并标为 stale，
    /// 不携带 active_run_id。decision 的 run_id 同样置空，避免引用目标库不存在的 run。
    /// 返回 (新插入 PR 数, 已存在跳过数, 复制普通评审数)。
    pub fn copy_prs_to(
        src: &Db,
        dst: &Db,
        keys: &[(String, i64)],
    ) -> Result<(usize, usize, usize)> {
        let mut prs = Vec::new();
        let mut reviews = Vec::new();
        let mut agent_snapshots = Vec::new();
        for (repo, number) in keys {
            if let Some(p) = src.get_pr_full(repo, *number)? {
                prs.push(p);
            }
            reviews.extend(src.list_reviews_of(repo, *number)?);
            if let Some(review) = src.load_agent_review(repo, *number)? {
                let findings = src.list_agent_findings(repo, *number)?;
                let decisions = src.list_agent_decisions(repo, *number)?;
                agent_snapshots.push((review, findings, decisions));
            }
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
        for (review, findings, decisions) in &agent_snapshots {
            let repo = review["repo"].as_str().unwrap_or("");
            let number = review["number"].as_i64().unwrap_or(0);
            let was_active = matches!(
                review["status"].as_str(),
                Some("queued")
                    | Some("running")
                    | Some("preparing")
                    | Some("validating")
                    | Some("waiting_decision")
            );
            let status = if was_active {
                Some("stale")
            } else {
                review["status"].as_str()
            };
            dst.conn.execute(
                "INSERT INTO agent_reviews(
                    repo, number, session_id, workspace, requested_base_sha, requested_head_sha,
                    reviewed_base_sha, reviewed_head_sha, active_run_id, status, verdict, summary,
                    validation, last_ci_status, acknowledged_at, error, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                 ON CONFLICT(repo,number) DO UPDATE SET
                    session_id=excluded.session_id, workspace=excluded.workspace,
                    requested_base_sha=excluded.requested_base_sha,
                    requested_head_sha=excluded.requested_head_sha,
                    reviewed_base_sha=excluded.reviewed_base_sha,
                    reviewed_head_sha=excluded.reviewed_head_sha,
                    active_run_id=NULL, status=excluded.status, verdict=excluded.verdict,
                    summary=excluded.summary, validation=excluded.validation,
                    last_ci_status=excluded.last_ci_status,
                    acknowledged_at=excluded.acknowledged_at, error=excluded.error,
                    created_at=excluded.created_at, updated_at=excluded.updated_at",
                params![
                    repo,
                    number,
                    review["session_id"].as_str(),
                    review["workspace"].as_str(),
                    review["requested_base_sha"].as_str(),
                    review["requested_head_sha"].as_str(),
                    review["reviewed_base_sha"].as_str(),
                    review["reviewed_head_sha"].as_str(),
                    status,
                    review["verdict"].as_str(),
                    review["summary"].as_str(),
                    review["validation"].as_str(),
                    review["last_ci_status"].as_str(),
                    review["acknowledged_at"].as_str(),
                    review["error"].as_str(),
                    review["created_at"].as_str(),
                    review["updated_at"].as_str(),
                ],
            )?;
            dst.conn.execute(
                "DELETE FROM agent_findings WHERE repo=?1 AND number=?2",
                params![repo, number],
            )?;
            for finding in findings {
                dst.conn.execute(
                    "INSERT INTO agent_findings(
                        repo, number, fingerprint, severity, category, path, start_line, end_line,
                        title, evidence, suggestion, status, human_status, first_head_sha,
                        last_head_sha, resolution_note, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                    params![
                        repo,
                        number,
                        finding["fingerprint"].as_str().unwrap_or(""),
                        finding["severity"].as_str(),
                        finding["category"].as_str(),
                        finding["path"].as_str(),
                        finding["start_line"].as_i64(),
                        finding["end_line"].as_i64(),
                        finding["title"].as_str(),
                        finding["evidence"].as_str(),
                        finding["suggestion"].as_str(),
                        finding["status"].as_str(),
                        finding["human_status"].as_str(),
                        finding["first_head_sha"].as_str(),
                        finding["last_head_sha"].as_str(),
                        finding["resolution_note"].as_str(),
                        finding["updated_at"].as_str(),
                    ],
                )?;
            }
            dst.conn.execute(
                "DELETE FROM agent_decisions WHERE repo=?1 AND number=?2",
                params![repo, number],
            )?;
            for decision in decisions {
                dst.conn.execute(
                    "INSERT INTO agent_decisions(
                        repo, number, run_id, kind, title, context, options_json,
                        review_event, merge_method, status, answer, result, created_at, resolved_at)
                     VALUES (?1,?2,NULL,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        repo,
                        number,
                        decision["kind"].as_str(),
                        decision["title"].as_str(),
                        decision["context"].as_str(),
                        serde_json::to_string(&decision["options"])?,
                        decision["review_event"].as_str(),
                        decision["merge_method"].as_str(),
                        decision["status"].as_str(),
                        decision["answer"].as_str(),
                        decision["result"].as_str(),
                        decision["created_at"].as_str(),
                        decision["resolved_at"].as_str(),
                    ],
                )?;
            }
        }
        _tx.commit()?;
        Ok((inserted, prs.len() - inserted, reviews.len()))
    }

    // ── 删除 / 墓碑（local 模式） ──

    /// 批量删除 PR：墓碑 + prs + reviews + diff_cache 一个事务。
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
            for table in [
                "agent_decisions",
                "agent_findings",
                "agent_review_runs",
                "agent_reviews",
            ] {
                self.conn.execute(
                    &format!("DELETE FROM {table} WHERE repo=?1 AND number=?2"),
                    params![repo, number],
                )?;
            }
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
        if self.is_tombstoned(
            p["repo"].as_str().unwrap_or(""),
            p["number"].as_i64().unwrap_or(0),
        )? {
            return Ok(false);
        }
        self.upsert_pr(p)?;
        Ok(true)
    }

    /// Inbox / URL 入库：墓碑检查与“不存在才插入”放在同一事务，返回真实新增结果。
    pub fn insert_pr_from_inbox(&self, p: &serde_json::Value) -> Result<InsertPrOutcome> {
        let tx = self.conn.unchecked_transaction()?;
        if self.is_tombstoned(
            p["repo"].as_str().unwrap_or(""),
            p["number"].as_i64().unwrap_or(0),
        )? {
            return Ok(InsertPrOutcome::Tombstoned);
        }
        let inserted = self.insert_pr_if_absent(p)?;
        tx.commit()?;
        Ok(if inserted {
            InsertPrOutcome::Inserted
        } else {
            InsertPrOutcome::AlreadyExists
        })
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

    // ── 持久 agent review ──

    fn agent_run_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "id": r.get::<_, i64>(0)?,
            "repo": r.get::<_, String>(1)?,
            "number": r.get::<_, i64>(2)?,
            "kind": r.get::<_, String>(3)?,
            "status": r.get::<_, String>(4)?,
            "base_sha": r.get::<_, Option<String>>(5)?,
            "head_sha": r.get::<_, String>(6)?,
            "parent_head_sha": r.get::<_, Option<String>>(7)?,
            "session_id": r.get::<_, Option<String>>(8)?,
            "workspace": r.get::<_, Option<String>>(9)?,
            "scope_complete": r.get::<_, Option<i64>>(10)?.map(|v| v != 0),
            "report": r.get::<_, Option<String>>(11)?
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
            "error": r.get::<_, Option<String>>(12)?,
            "created_at": r.get::<_, Option<String>>(13)?,
            "started_at": r.get::<_, Option<String>>(14)?,
            "finished_at": r.get::<_, Option<String>>(15)?,
        }))
    }

    const AGENT_RUN_COLUMNS: &'static str =
        "id, repo, number, kind, status, base_sha, head_sha, parent_head_sha,
         session_id, workspace, scope_complete, report_json, error,
         created_at, started_at, finished_at";

    fn agent_run_by_id(conn: &Connection, id: i64) -> Result<Option<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM agent_review_runs WHERE id=?1",
            Self::AGENT_RUN_COLUMNS
        );
        Ok(conn
            .query_row(&sql, params![id], Self::agent_run_from_row)
            .optional()?)
    }

    fn agent_review_from_row(
        r: &rusqlite::Row<'_>,
        offset: usize,
    ) -> rusqlite::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "repo": r.get::<_, String>(offset)?,
            "number": r.get::<_, i64>(offset + 1)?,
            "session_id": r.get::<_, Option<String>>(offset + 2)?,
            "workspace": r.get::<_, Option<String>>(offset + 3)?,
            "requested_base_sha": r.get::<_, Option<String>>(offset + 4)?,
            "requested_head_sha": r.get::<_, Option<String>>(offset + 5)?,
            "reviewed_base_sha": r.get::<_, Option<String>>(offset + 6)?,
            "reviewed_head_sha": r.get::<_, Option<String>>(offset + 7)?,
            "active_run_id": r.get::<_, Option<i64>>(offset + 8)?,
            "status": r.get::<_, Option<String>>(offset + 9)?,
            "verdict": r.get::<_, Option<String>>(offset + 10)?,
            "summary": r.get::<_, Option<String>>(offset + 11)?,
            "validation": r.get::<_, Option<String>>(offset + 12)?,
            "last_ci_status": r.get::<_, Option<String>>(offset + 13)?,
            "acknowledged_at": r.get::<_, Option<String>>(offset + 14)?,
            "error": r.get::<_, Option<String>>(offset + 15)?,
            "created_at": r.get::<_, Option<String>>(offset + 16)?,
            "updated_at": r.get::<_, Option<String>>(offset + 17)?,
        }))
    }

    const AGENT_REVIEW_COLUMNS: &'static str =
        "repo, number, session_id, workspace, requested_base_sha, requested_head_sha,
         reviewed_base_sha, reviewed_head_sha, active_run_id, status, verdict, summary,
         validation, last_ci_status, acknowledged_at, error, created_at, updated_at";

    fn agent_review_on(
        conn: &Connection,
        repo: &str,
        number: i64,
    ) -> Result<Option<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM agent_reviews WHERE repo=?1 AND number=?2",
            Self::AGENT_REVIEW_COLUMNS
        );
        Ok(conn
            .query_row(&sql, params![repo, number], |r| {
                Self::agent_review_from_row(r, 0)
            })
            .optional()?)
    }

    fn enqueue_agent_review_inner(
        &self,
        repo: &str,
        number: i64,
        kind: &str,
        base: &str,
        head: &str,
        parent: Option<&str>,
        workspace: &str,
    ) -> Result<i64> {
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM agent_review_runs
                 WHERE repo=?1 AND number=?2
                   AND status IN ('queued','running','preparing','validating','waiting_decision')
                 ORDER BY id DESC LIMIT 1",
                params![repo, number],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            self.conn.execute(
                "UPDATE agent_reviews SET active_run_id=?1, updated_at=?2
                 WHERE repo=?3 AND number=?4",
                params![id, now(), repo, number],
            )?;
            return Ok(id);
        }

        let previous = Self::agent_review_on(&self.conn, repo, number)?;
        let inherited_parent = if kind == "initial" && parent.is_none() {
            previous
                .as_ref()
                .and_then(|r| r["reviewed_head_sha"].as_str())
        } else {
            None
        };
        let parent = parent.or(inherited_parent);
        let session_id = previous.as_ref().and_then(|r| r["session_id"].as_str());
        let ts = now();
        self.conn.execute(
            "INSERT INTO agent_review_runs(
                repo, number, kind, status, base_sha, head_sha, parent_head_sha,
                session_id, workspace, created_at)
             VALUES (?1,?2,?3,'queued',?4,?5,?6,?7,?8,?9)",
            params![
                repo, number, kind, base, head, parent, session_id, workspace, ts
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO agent_reviews(
                repo, number, session_id, workspace, requested_base_sha, requested_head_sha,
                active_run_id, status, error, acknowledged_at, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'queued',NULL,NULL,?8,?8)
             ON CONFLICT(repo,number) DO UPDATE SET
                session_id=agent_reviews.session_id,
                workspace=excluded.workspace,
                requested_base_sha=excluded.requested_base_sha,
                requested_head_sha=excluded.requested_head_sha,
                active_run_id=excluded.active_run_id,
                status='queued', error=NULL, acknowledged_at=NULL, updated_at=excluded.updated_at",
            params![repo, number, session_id, workspace, base, head, id, ts],
        )?;
        Ok(id)
    }

    pub fn enqueue_agent_review(
        &self,
        repo: &str,
        number: i64,
        kind: &str,
        base: &str,
        head: &str,
        parent: Option<&str>,
        workspace: &str,
    ) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let id =
            self.enqueue_agent_review_inner(repo, number, kind, base, head, parent, workspace)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn get_agent_run(&self, run_id: i64) -> Result<Option<serde_json::Value>> {
        Self::agent_run_by_id(&self.conn, run_id)
    }

    pub fn next_agent_run(&self) -> Result<Option<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM agent_review_runs WHERE status='queued'
             ORDER BY created_at, id LIMIT 1",
            Self::AGENT_RUN_COLUMNS
        );
        Ok(self
            .conn
            .query_row(&sql, [], Self::agent_run_from_row)
            .optional()?)
    }

    pub fn mark_agent_run_running(&self, run_id: i64) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let ts = now();
        let n = self.conn.execute(
            "UPDATE agent_review_runs
             SET status='running', started_at=?1, finished_at=NULL, error=NULL
             WHERE id=?2 AND status='queued'",
            params![ts, run_id],
        )?;
        if n > 0 {
            if let Some(run) = Self::agent_run_by_id(&self.conn, run_id)? {
                self.conn.execute(
                    "UPDATE agent_reviews SET status='running', active_run_id=?1, updated_at=?2
                     WHERE repo=?3 AND number=?4 AND active_run_id=?1",
                    params![
                        run_id,
                        ts,
                        run["repo"].as_str().unwrap_or(""),
                        run["number"].as_i64().unwrap_or(0)
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn apply_agent_report(
        &self,
        run_id: i64,
        report: &crate::agent_review::AgentReport,
    ) -> Result<bool> {
        let report = serde_json::to_value(report)?;
        let report_json = serde_json::to_string(&report)?;
        let tx = self.conn.unchecked_transaction()?;
        let Some(run) = Self::agent_run_by_id(&self.conn, run_id)? else {
            return Ok(false);
        };
        if !matches!(
            run["status"].as_str(),
            Some("running") | Some("preparing") | Some("validating")
        ) {
            return Ok(false);
        }
        let repo = run["repo"].as_str().unwrap_or("");
        let number = run["number"].as_i64().unwrap_or(0);
        let review = Self::agent_review_on(&self.conn, repo, number)?;
        let pr_head = self
            .conn
            .query_row(
                "SELECT head_sha FROM prs WHERE repo=?1 AND number=?2",
                params![repo, number],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let valid = review.as_ref().is_some_and(|review| {
            let parent_matches = match run["parent_head_sha"].as_str() {
                Some(parent) => review["reviewed_head_sha"].as_str() == Some(parent),
                None => true,
            };
            review["active_run_id"].as_i64() == Some(run_id)
                && review["requested_head_sha"].as_str() == run["head_sha"].as_str()
                && review["requested_base_sha"].as_str() == run["base_sha"].as_str()
                && parent_matches
                && report["repo"].as_str() == Some(repo)
                && report["number"].as_i64() == Some(number)
                && report["base_sha"].as_str() == run["base_sha"].as_str()
                && report["head_sha"].as_str() == run["head_sha"].as_str()
                && report["parent_head_sha"].as_str() == run["parent_head_sha"].as_str()
                && pr_head
                    .as_deref()
                    .is_none_or(|head| head.is_empty() || Some(head) == run["head_sha"].as_str())
        });
        if !valid {
            let ts = now();
            self.conn.execute(
                "UPDATE agent_review_runs
                 SET status='superseded', report_json=?1, finished_at=?2,
                     error='superseded by a newer baseline'
                 WHERE id=?3",
                params![report_json, ts, run_id],
            )?;
            self.conn.execute(
                "UPDATE agent_reviews
                 SET active_run_id=NULL, status='failed', error='agent run superseded', updated_at=?1
                 WHERE repo=?2 AND number=?3 AND active_run_id=?4",
                params![ts, repo, number, run_id],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        let scope_complete = report["scope_complete"].as_bool().unwrap_or(false);
        let text = |value: &serde_json::Value| -> Option<String> {
            match value {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some(s.clone()),
                other => serde_json::to_string(other).ok(),
            }
        };
        let verdict = text(&report["verdict"]);
        let summary = text(&report["summary"]);
        let validation = text(&report["validation"]);
        let ts = now();

        let mut decisions = Vec::new();
        if let Some(decision) = report.get("decision").filter(|d| !d.is_null()) {
            decisions.push(decision.clone());
        } else if let Some(decision) = report.get("decision_request").filter(|d| !d.is_null()) {
            decisions.push(decision.clone());
        }
        if let Some(array) = report["decisions"].as_array() {
            decisions.extend(array.iter().cloned());
        }
        let clarification = decisions.iter().any(|d| {
            d["kind"]
                .as_str()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("clarification"))
        });
        for decision in &decisions {
            let options = decision
                .get("options")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            self.conn.execute(
                "INSERT INTO agent_decisions(
                    repo, number, run_id, kind, title, context, options_json,
                    review_event, merge_method, status, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'open',?10)",
                params![
                    repo,
                    number,
                    run_id,
                    text(&decision["kind"]),
                    text(&decision["title"]),
                    text(&decision["context"]),
                    serde_json::to_string(&options)?,
                    text(&decision["review_event"]),
                    text(&decision["merge_method"]),
                    ts,
                ],
            )?;
        }

        let incomplete_without_clarification = !scope_complete && !clarification;
        let mut reconciliation_error = incomplete_without_clarification
            .then(|| "scope incomplete without clarification; retry required".to_owned());
        let reconcile_findings =
            scope_complete && !matches!(run["kind"].as_str(), Some("ci_recheck") | Some("edit"));
        if reconcile_findings {
            let mut stmt = self.conn.prepare(
                "SELECT fingerprint FROM agent_findings
                 WHERE repo=?1 AND number=?2 AND status='open'",
            )?;
            let expected = stmt
                .query_map(params![repo, number], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
            drop(stmt);
            let mut seen = std::collections::HashSet::new();
            let empty = Vec::new();
            let findings = report["findings"].as_array().unwrap_or(&empty);
            for finding in findings {
                let candidate_fingerprint = finding["fingerprint"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!(
                            "{}:{}:{}:{}",
                            finding["category"].as_str().unwrap_or(""),
                            finding["path"].as_str().unwrap_or(""),
                            finding["start_line"].as_i64().unwrap_or(0),
                            finding["title"].as_str().unwrap_or("")
                        )
                    });
                let reported_id = finding["id"].as_str().filter(|id| !id.is_empty());
                let existing = match reported_id {
                    Some(id) => self
                        .conn
                        .query_row(
                            "SELECT fingerprint FROM agent_findings
                         WHERE repo=?1 AND number=?2 AND CAST(id AS TEXT)=?3",
                            params![repo, number, id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?,
                    None => None,
                };
                let existing = match existing {
                    Some(fingerprint) => Some(fingerprint),
                    None => self
                        .conn
                        .query_row(
                            "SELECT fingerprint FROM agent_findings
                         WHERE repo=?1 AND number=?2 AND fingerprint=?3",
                            params![repo, number, candidate_fingerprint],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?,
                };
                let fingerprint = existing.unwrap_or(candidate_fingerprint);
                seen.insert(fingerprint.clone());
                let reported_status = finding["status"]
                    .as_str()
                    .unwrap_or("open")
                    .to_ascii_lowercase();
                let note =
                    text(&finding["resolution_note"]).or_else(|| text(&finding["resolution"]));
                let obsolete = reported_status == "obsolete"
                    || note.as_deref().is_some_and(|note| {
                        let note = note.to_ascii_lowercase();
                        note.contains("obsolete") || note.contains("not an issue")
                    });
                let status = if obsolete {
                    "obsolete"
                } else if matches!(reported_status.as_str(), "resolved" | "fixed") {
                    "fixed"
                } else {
                    "open"
                };
                self.conn.execute(
                    "INSERT INTO agent_findings(
                        repo, number, fingerprint, severity, category, path, start_line, end_line,
                        title, evidence, suggestion, status, first_head_sha, last_head_sha,
                        resolution_note, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13,?14,?15)
                     ON CONFLICT(repo,number,fingerprint) DO UPDATE SET
                        severity=excluded.severity, category=excluded.category, path=excluded.path,
                        start_line=excluded.start_line, end_line=excluded.end_line,
                        title=excluded.title, evidence=excluded.evidence,
                        suggestion=excluded.suggestion, status=excluded.status,
                        last_head_sha=excluded.last_head_sha,
                        resolution_note=excluded.resolution_note, updated_at=excluded.updated_at",
                    params![
                        repo,
                        number,
                        fingerprint,
                        text(&finding["severity"]),
                        text(&finding["category"]),
                        text(&finding["path"]),
                        finding["start_line"].as_i64(),
                        finding["end_line"].as_i64(),
                        text(&finding["title"]),
                        text(&finding["evidence"]),
                        text(&finding["suggestion"]),
                        status,
                        run["head_sha"].as_str().unwrap_or(""),
                        note,
                        ts,
                    ],
                )?;
            }
            let resolved_findings = report["resolved_findings"].as_array().unwrap_or(&empty);
            for finding in resolved_findings {
                let identifier = finding["fingerprint"].as_str().unwrap_or("");
                let existing = self
                    .conn
                    .query_row(
                        "SELECT fingerprint FROM agent_findings
                     WHERE repo=?1 AND number=?2 AND (fingerprint=?3 OR CAST(id AS TEXT)=?3)",
                        params![repo, number, identifier],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?;
                let fingerprint = existing.unwrap_or_else(|| identifier.to_owned());
                seen.insert(fingerprint.clone());
                let status = if finding["status"].as_str() == Some("obsolete") {
                    "obsolete"
                } else {
                    "fixed"
                };
                self.conn.execute(
                    "UPDATE agent_findings
                     SET status=?1, last_head_sha=?2, resolution_note=?3, updated_at=?4
                     WHERE repo=?5 AND number=?6 AND fingerprint=?7",
                    params![
                        status,
                        run["head_sha"].as_str().unwrap_or(""),
                        text(&finding["note"]),
                        ts,
                        repo,
                        number,
                        fingerprint,
                    ],
                )?;
            }
            let missing = expected.difference(&seen).cloned().collect::<Vec<_>>();
            if !missing.is_empty() {
                let mut stmt = self.conn.prepare(
                    "UPDATE agent_findings
                     SET status='stale', last_head_sha=?1,
                         resolution_note='missing from re-review', updated_at=?2
                     WHERE repo=?3 AND number=?4 AND fingerprint=?5 AND status='open'",
                )?;
                for fingerprint in &missing {
                    stmt.execute(params![
                        run["head_sha"].as_str().unwrap_or(""),
                        ts,
                        repo,
                        number,
                        fingerprint
                    ])?;
                }
                reconciliation_error = Some(format!(
                    "re-review omitted {} previously open finding(s)",
                    missing.len()
                ));
            }
        }

        let waiting = clarification;
        let failed = reconciliation_error.is_some();
        let advance_baseline = !failed
            && !waiting
            && !matches!(run["kind"].as_str(), Some("ci_recheck") | Some("edit"));
        let run_status = if failed {
            "failed"
        } else if waiting {
            "waiting_decision"
        } else {
            "completed"
        };
        self.conn.execute(
            "UPDATE agent_review_runs
             SET scope_complete=?1, report_json=?2, status=?3, error=?4,
                 finished_at=CASE WHEN ?3 IN ('completed','failed') THEN ?5 ELSE NULL END
             WHERE id=?6",
            params![
                scope_complete,
                report_json,
                run_status,
                reconciliation_error,
                ts,
                run_id
            ],
        )?;
        if failed || !waiting {
            self.conn.execute(
                "UPDATE agent_reviews SET
                    session_id=COALESCE(?1, session_id), workspace=?2,
                    reviewed_base_sha=CASE WHEN ?3 THEN ?4 ELSE reviewed_base_sha END,
                    reviewed_head_sha=CASE WHEN ?3 THEN ?5 ELSE reviewed_head_sha END,
                    active_run_id=NULL, status=?6, verdict=?7, summary=?8, validation=?9,
                    acknowledged_at=CASE WHEN ?3 THEN NULL ELSE acknowledged_at END,
                    error=?10, updated_at=?11
                 WHERE repo=?12 AND number=?13",
                params![
                    run["session_id"].as_str(),
                    run["workspace"].as_str(),
                    advance_baseline,
                    run["base_sha"].as_str(),
                    run["head_sha"].as_str(),
                    run_status,
                    verdict,
                    summary,
                    validation,
                    reconciliation_error,
                    ts,
                    repo,
                    number,
                ],
            )?;
        } else {
            self.conn.execute(
                "UPDATE agent_reviews SET
                    session_id=COALESCE(?1, session_id), workspace=?2,
                    status='waiting_decision', verdict=?3, summary=?4, validation=?5,
                    error=NULL, updated_at=?6
                 WHERE repo=?7 AND number=?8 AND active_run_id=?9",
                params![
                    run["session_id"].as_str(),
                    run["workspace"].as_str(),
                    verdict,
                    summary,
                    validation,
                    ts,
                    repo,
                    number,
                    run_id,
                ],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    fn finish_agent_run(&self, run_id: i64, status: &str, error: Option<&str>) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(run) = Self::agent_run_by_id(&self.conn, run_id)? else {
            return Ok(false);
        };
        let ts = now();
        let n = self.conn.execute(
            "UPDATE agent_review_runs SET status=?1, error=?2, finished_at=?3
             WHERE id=?4 AND status IN ('queued','running','preparing','validating','waiting_decision')",
            params![status, error, ts, run_id],
        )?;
        if n > 0 {
            self.conn.execute(
                "UPDATE agent_reviews SET status=?1, error=?2, active_run_id=NULL, updated_at=?3
                 WHERE repo=?4 AND number=?5 AND active_run_id=?6",
                params![
                    status,
                    error,
                    ts,
                    run["repo"].as_str().unwrap_or(""),
                    run["number"].as_i64().unwrap_or(0),
                    run_id,
                ],
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn fail_agent_run(&self, run_id: i64, error: &str) -> Result<bool> {
        self.finish_agent_run(run_id, "failed", Some(error))
    }

    pub fn supersede_agent_run(&self, run_id: i64, error: &str) -> Result<bool> {
        self.finish_agent_run(run_id, "superseded", Some(error))
    }

    pub fn cancel_agent_run(&self, run_id: i64) -> Result<bool> {
        self.finish_agent_run(run_id, "cancelled", None)
    }

    pub fn recover_agent_runs(&self, active_run_ids: &[i64]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let candidates = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM agent_review_runs
                 WHERE status IN ('running','preparing','validating') ORDER BY id",
            )?;
            stmt.query_map([], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let ts = now();
        let mut recovered = 0;
        for run_id in candidates {
            if active_run_ids.contains(&run_id) {
                continue;
            }
            let Some(run) = Self::agent_run_by_id(&self.conn, run_id)? else {
                continue;
            };
            let n = self.conn.execute(
                "UPDATE agent_review_runs
                 SET status='queued', started_at=NULL, finished_at=NULL, error=NULL
                 WHERE id=?1 AND status IN ('running','preparing','validating')",
                params![run_id],
            )?;
            if n > 0 {
                self.conn.execute(
                    "UPDATE agent_reviews SET status='queued', error=NULL, updated_at=?1
                     WHERE repo=?2 AND number=?3 AND active_run_id=?4",
                    params![
                        ts,
                        run["repo"].as_str().unwrap_or(""),
                        run["number"].as_i64().unwrap_or(0),
                        run_id
                    ],
                )?;
                recovered += n;
            }
        }
        tx.commit()?;
        Ok(recovered)
    }

    pub fn load_agent_review(&self, repo: &str, number: i64) -> Result<Option<serde_json::Value>> {
        Self::agent_review_on(&self.conn, repo, number)
    }

    pub fn list_agent_reviews(&self) -> Result<Vec<serde_json::Value>> {
        let sql = format!("SELECT {} FROM agent_reviews", Self::AGENT_REVIEW_COLUMNS);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| Self::agent_review_from_row(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_agent_findings(&self, repo: &str, number: i64) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, repo, number, fingerprint, severity, category, path, start_line, end_line,
                    title, evidence, suggestion, status, human_status, first_head_sha,
                    last_head_sha, resolution_note, updated_at
             FROM agent_findings WHERE repo=?1 AND number=?2 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![repo, number], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "repo": r.get::<_, String>(1)?,
                    "number": r.get::<_, i64>(2)?,
                    "fingerprint": r.get::<_, String>(3)?,
                    "severity": r.get::<_, Option<String>>(4)?,
                    "category": r.get::<_, Option<String>>(5)?,
                    "path": r.get::<_, Option<String>>(6)?,
                    "start_line": r.get::<_, Option<i64>>(7)?,
                    "end_line": r.get::<_, Option<i64>>(8)?,
                    "title": r.get::<_, Option<String>>(9)?,
                    "evidence": r.get::<_, Option<String>>(10)?,
                    "suggestion": r.get::<_, Option<String>>(11)?,
                    "status": r.get::<_, Option<String>>(12)?,
                    "human_status": r.get::<_, Option<String>>(13)?,
                    "first_head_sha": r.get::<_, Option<String>>(14)?,
                    "last_head_sha": r.get::<_, Option<String>>(15)?,
                    "resolution_note": r.get::<_, Option<String>>(16)?,
                    "updated_at": r.get::<_, Option<String>>(17)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn agent_decision_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "id": r.get::<_, i64>(0)?,
            "repo": r.get::<_, String>(1)?,
            "number": r.get::<_, i64>(2)?,
            "run_id": r.get::<_, Option<i64>>(3)?,
            "kind": r.get::<_, Option<String>>(4)?,
            "title": r.get::<_, Option<String>>(5)?,
            "context": r.get::<_, Option<String>>(6)?,
            "options": r.get::<_, Option<String>>(7)?
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .unwrap_or_else(|| serde_json::json!([])),
            "review_event": r.get::<_, Option<String>>(8)?,
            "merge_method": r.get::<_, Option<String>>(9)?,
            "status": r.get::<_, Option<String>>(10)?,
            "answer": r.get::<_, Option<String>>(11)?,
            "result": r.get::<_, Option<String>>(12)?,
            "created_at": r.get::<_, Option<String>>(13)?,
            "resolved_at": r.get::<_, Option<String>>(14)?,
        }))
    }

    const AGENT_DECISION_COLUMNS: &'static str =
        "id, repo, number, run_id, kind, title, context, options_json, review_event,
         merge_method, status, answer, result, created_at, resolved_at";

    fn agent_decision_by_id(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM agent_decisions WHERE id=?1",
            Self::AGENT_DECISION_COLUMNS
        );
        Ok(self
            .conn
            .query_row(&sql, params![id], Self::agent_decision_from_row)
            .optional()?)
    }

    pub fn list_agent_decisions(&self, repo: &str, number: i64) -> Result<Vec<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM agent_decisions WHERE repo=?1 AND number=?2 ORDER BY id",
            Self::AGENT_DECISION_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![repo, number], Self::agent_decision_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_needs_you(&self) -> Result<Vec<serde_json::Value>> {
        let review_columns = Self::AGENT_REVIEW_COLUMNS
            .split(',')
            .map(|column| format!("ar.{}", column.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {review_columns}, p.head_sha, COALESCE(ar.last_ci_status, p.ci_status),
                (SELECT COUNT(*) FROM agent_decisions d
                 WHERE d.repo=ar.repo AND d.number=ar.number AND d.status='open')
             FROM agent_reviews ar LEFT JOIN prs p
               ON p.repo=ar.repo AND p.number=ar.number
             ORDER BY ar.updated_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |r| {
                let mut review = Self::agent_review_from_row(r, 0)?;
                let pr_head = r.get::<_, Option<String>>(18)?;
                let ci = r.get::<_, Option<String>>(19)?;
                let open_decisions = r.get::<_, i64>(20)?;
                let reviewed = review["reviewed_head_sha"].as_str();
                let stale = reviewed.is_some_and(|head| {
                    !head.is_empty() && pr_head.as_deref().is_some_and(|current| current != head)
                });
                let ci_failure = reviewed.is_some()
                    && reviewed == pr_head.as_deref()
                    && ci.as_deref().is_some_and(|ci| {
                        matches!(
                            ci.to_ascii_lowercase().as_str(),
                            "failure" | "failed" | "error"
                        )
                    });
                let mut reasons = Vec::new();
                if open_decisions > 0 {
                    reasons.push("open_decision");
                }
                if review["status"].as_str() == Some("completed")
                    && review["acknowledged_at"].is_null()
                {
                    reasons.push("unacknowledged");
                }
                if review["status"].as_str() == Some("failed") {
                    reasons.push("failed");
                }
                if stale {
                    reasons.push("stale");
                }
                if ci_failure {
                    reasons.push("ci_failure");
                }
                review["open_decisions"] = serde_json::json!(open_decisions);
                review["reason"] = reasons.first().copied().into();
                review["reasons"] = serde_json::json!(reasons);
                Ok(review)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|review| {
                review["reasons"]
                    .as_array()
                    .is_some_and(|reasons| !reasons.is_empty())
            })
            .collect();
        Ok(rows)
    }

    pub fn acknowledge_agent_review(&self, repo: &str, number: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agent_reviews SET acknowledged_at=?1, updated_at=?1
             WHERE repo=?2 AND number=?3",
            params![now(), repo, number],
        )? > 0)
    }

    pub fn answer_agent_decision(
        &self,
        id: i64,
        answer: &str,
    ) -> Result<Option<serde_json::Value>> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(decision) = self.agent_decision_by_id(id)? else {
            return Ok(None);
        };
        if decision["status"].as_str() != Some("open") {
            return Ok(None);
        }
        let ts = now();
        let answered = self.conn.execute(
            "UPDATE agent_decisions
             SET status='answered', answer=?1, resolved_at=?2 WHERE id=?3 AND status='open'",
            params![answer, ts, id],
        )?;
        if answered == 0 {
            return Ok(None);
        }
        if let Some(run_id) = decision["run_id"].as_i64() {
            let n = self.conn.execute(
                "UPDATE agent_review_runs
                 SET status='queued', started_at=NULL, finished_at=NULL, error=NULL
                 WHERE id=?1 AND status='waiting_decision'",
                params![run_id],
            )?;
            if n > 0 {
                self.conn.execute(
                    "UPDATE agent_reviews SET status='queued', updated_at=?1
                     WHERE repo=?2 AND number=?3 AND active_run_id=?4",
                    params![
                        ts,
                        decision["repo"].as_str().unwrap_or(""),
                        decision["number"].as_i64().unwrap_or(0),
                        run_id
                    ],
                )?;
            }
        }
        let decision = self.agent_decision_by_id(id)?;
        tx.commit()?;
        Ok(decision)
    }

    pub fn observe_agent_pr(
        &self,
        repo: &str,
        number: i64,
        current_head: &str,
        ci: Option<&str>,
        closed: bool,
        auto_rereview: bool,
    ) -> Result<Option<i64>> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(review) = Self::agent_review_on(&self.conn, repo, number)? else {
            return Ok(None);
        };
        let ts = now();
        if closed {
            self.conn.execute(
                "UPDATE agent_review_runs SET status='cancelled', finished_at=?1
                 WHERE repo=?2 AND number=?3
                   AND status IN ('queued','running','preparing','validating','waiting_decision')",
                params![ts, repo, number],
            )?;
            self.conn.execute(
                "UPDATE agent_reviews
                 SET status='closed', active_run_id=NULL, last_ci_status=?1, updated_at=?2
                 WHERE repo=?3 AND number=?4",
                params![ci, ts, repo, number],
            )?;
            tx.commit()?;
            return Ok(None);
        }

        let reviewed = review["reviewed_head_sha"].as_str();
        let stale = !current_head.is_empty()
            && reviewed.is_some_and(|reviewed| !reviewed.is_empty() && reviewed != current_head);
        if stale {
            self.conn.execute(
                "UPDATE agent_review_runs SET status='superseded', finished_at=?1,
                        error='superseded by a newer PR head'
                 WHERE repo=?2 AND number=?3 AND head_sha<>?4
                   AND status IN ('queued','running','preparing','validating','waiting_decision')",
                params![ts, repo, number, current_head],
            )?;
            self.conn.execute(
                "UPDATE agent_decisions SET status='superseded', resolved_at=?1
                 WHERE repo=?2 AND number=?3 AND status='open'",
                params![ts, repo, number],
            )?;
            self.conn.execute(
                "UPDATE agent_reviews
                 SET status='stale', active_run_id=NULL, acknowledged_at=NULL, updated_at=?1
                 WHERE repo=?2 AND number=?3",
                params![ts, repo, number],
            )?;
        } else if !current_head.is_empty()
            && reviewed == Some(current_head)
            && review["status"].as_str() == Some("stale")
        {
            self.conn.execute(
                "UPDATE agent_reviews SET status='completed', updated_at=?1
                 WHERE repo=?2 AND number=?3",
                params![ts, repo, number],
            )?;
        }

        let ci_changed = review["last_ci_status"].as_str() != ci;
        let ci_failure = ci.is_some_and(|ci| {
            matches!(
                ci.to_ascii_lowercase().as_str(),
                "failure" | "failed" | "error"
            )
        });
        if ci_changed {
            self.conn.execute(
                "UPDATE agent_reviews SET last_ci_status=?1,
                    acknowledged_at=CASE WHEN ?2 THEN NULL ELSE acknowledged_at END,
                    updated_at=?3
                 WHERE repo=?4 AND number=?5",
                params![ci, ci_failure, ts, repo, number],
            )?;
        }

        let mut queued = None;
        if stale && auto_rereview {
            let already_attempted: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM agent_review_runs
                 WHERE repo=?1 AND number=?2 AND kind='rereview' AND head_sha=?3
                   AND status IN ('failed','cancelled')",
                params![repo, number, current_head],
                |r| r.get(0),
            )?;
            if already_attempted == 0 {
                let base = review["requested_base_sha"]
                    .as_str()
                    .or_else(|| review["reviewed_base_sha"].as_str())
                    .unwrap_or("");
                let workspace = review["workspace"].as_str().unwrap_or("");
                queued = Some(self.enqueue_agent_review_inner(
                    repo,
                    number,
                    "rereview",
                    base,
                    current_head,
                    reviewed,
                    workspace,
                )?);
            }
        } else if ci_changed && ci_failure && reviewed == Some(current_head) && auto_rereview {
            let base = review["requested_base_sha"]
                .as_str()
                .or_else(|| review["reviewed_base_sha"].as_str())
                .unwrap_or("");
            let workspace = review["workspace"].as_str().unwrap_or("");
            queued = Some(self.enqueue_agent_review_inner(
                repo,
                number,
                "ci_recheck",
                base,
                current_head,
                None,
                workspace,
            )?);
        }
        tx.commit()?;
        Ok(queued)
    }

    pub fn prepare_agent_run_snapshot(
        &self,
        run_id: i64,
        base_sha: &str,
        head_sha: &str,
        workspace: &str,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(run) = Self::agent_run_by_id(&self.conn, run_id)? else {
            return Ok(false);
        };
        if run["status"].as_str() != Some("queued") {
            return Ok(false);
        }
        let repo = run["repo"].as_str().unwrap_or("");
        let number = run["number"].as_i64().unwrap_or(0);
        let review = Self::agent_review_on(&self.conn, repo, number)?;
        let reviewed = review
            .as_ref()
            .and_then(|review| review["reviewed_head_sha"].as_str())
            .filter(|head| !head.is_empty());
        let special_kind = matches!(
            run["kind"].as_str(),
            Some("ci_recheck") | Some("edit") | Some("followup")
        );
        let parent = if special_kind {
            run["parent_head_sha"].as_str()
        } else {
            reviewed.filter(|reviewed| Some(*reviewed) != Some(head_sha))
        };
        let kind = if special_kind {
            run["kind"].as_str().unwrap_or("initial")
        } else if parent.is_some() {
            "rereview"
        } else {
            "initial"
        };
        let n = self.conn.execute(
            "UPDATE agent_review_runs
             SET kind=?1, base_sha=?2, head_sha=?3, parent_head_sha=?4, workspace=?5
             WHERE id=?6 AND status='queued'",
            params![kind, base_sha, head_sha, parent, workspace, run_id],
        )?;
        self.conn.execute(
            "UPDATE agent_reviews
             SET requested_base_sha=?1, requested_head_sha=?2, workspace=?3, updated_at=?4
             WHERE repo=?5 AND number=?6 AND active_run_id=?7",
            params![base_sha, head_sha, workspace, now(), repo, number, run_id],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn set_agent_run_parent(&self, run_id: i64, parent_head_sha: Option<&str>) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agent_review_runs SET parent_head_sha=?1
             WHERE id=?2 AND status IN ('queued','running','preparing')",
            params![parent_head_sha, run_id],
        )? > 0)
    }

    pub fn set_agent_run_session(&self, run_id: i64, session_id: &str) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(run) = Self::agent_run_by_id(&self.conn, run_id)? else {
            return Ok(false);
        };
        let n = self.conn.execute(
            "UPDATE agent_review_runs SET session_id=?1 WHERE id=?2",
            params![session_id, run_id],
        )?;
        self.conn.execute(
            "UPDATE agent_reviews SET session_id=?1, updated_at=?2
             WHERE repo=?3 AND number=?4 AND active_run_id=?5",
            params![
                session_id,
                now(),
                run["repo"].as_str().unwrap_or(""),
                run["number"].as_i64().unwrap_or(0),
                run_id
            ],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn record_agent_decision_result(
        &self,
        decision_id: i64,
        result: &str,
        success: bool,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agent_decisions SET result=?1, status=?2, resolved_at=?3 WHERE id=?4",
            params![
                result,
                if success { "executed" } else { "failed" },
                now(),
                decision_id
            ],
        )? > 0)
    }

    pub fn set_finding_human_status(&self, id: i64, status: &str) -> Result<bool> {
        let status = (!status.is_empty()).then_some(status);
        Ok(self.conn.execute(
            "UPDATE agent_findings SET human_status=?1, updated_at=?2 WHERE id=?3",
            params![status, now(), id],
        )? > 0)
    }

    pub fn mark_local_review_success(&self, repo: &str, number: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE prs SET status='reviewed', updated_at=?1
             WHERE repo=?2 AND number=?3 AND status IN ('pending','failed')",
            params![now(), repo, number],
        )? > 0)
    }

    // ── diff 缓存 ──
    pub fn get_diff(&self, repo: &str, number: i64, head_sha: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT diff FROM diff_cache WHERE repo=?1 AND number=?2 AND head_sha=?3")?;
        let mut rows = stmt.query(params![repo, number, head_sha])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    pub fn put_diff(&self, repo: &str, number: i64, head_sha: &str, diff: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO diff_cache(repo, number, head_sha, diff, fetched_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(repo, number, head_sha) DO UPDATE SET
                diff=excluded.diff, fetched_at=excluded.fetched_at",
            params![repo, number, head_sha, diff, now()],
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
        rows.iter().find(|r| r["number"] == n).unwrap()["source"]
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
        db.upsert_pr(&pr_json_src("o/r", 1, "srv", "server"))
            .unwrap();
        db.upsert_pr(&pr_json_src("o/r", 2, "inbox", "inbox"))
            .unwrap();
        db.upsert_pr(&pr_json_src("o/r", 3, "gh", "github"))
            .unwrap();
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
    fn backup_captures_wal_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("prdesk-db-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("source.db");
        let backup_path = dir.join("nested").join("backup.db");
        let source = Db::open(&source_path).unwrap();
        source.upsert_pr(&pr_json("o/r", 1, "wal row")).unwrap();
        source
            .upsert_review(&review_json("o/r", 1, "h1", 8.0))
            .unwrap();

        backup_database(&source_path, &backup_path).unwrap();
        validate_database(&backup_path).unwrap();
        let backup = Db::open(&backup_path).unwrap();
        assert!(backup.pr_exists("o/r", 1).unwrap());
        assert_eq!(
            backup.list_reviews_of("o/r", 1).unwrap()[0]["head_sha"],
            "h1"
        );
        let integrity: String = backup
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");

        source
            .upsert_pr(&pr_json("o/r", 2, "after backup"))
            .unwrap();
        assert!(!backup.pr_exists("o/r", 2).unwrap());
        assert!(backup_database(&source_path, &backup_path).is_err());
        assert!(backup.pr_exists("o/r", 1).unwrap());
        drop(backup);
        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_invalid_future_and_non_pr_desk_databases() {
        let dir = std::env::temp_dir().join(format!("prdesk-db-validate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let text_path = dir.join("not-sqlite.db");
        std::fs::write(&text_path, "not a database").unwrap();
        assert!(validate_database(&text_path).is_err());

        let future_path = dir.join("future.db");
        Connection::open(&future_path)
            .unwrap()
            .execute_batch("PRAGMA user_version=999")
            .unwrap();
        assert!(validate_database(&future_path).is_err());

        let empty_path = dir.join("empty.db");
        Connection::open(&empty_path).unwrap();
        assert!(validate_database(&empty_path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_adds_source_column_to_old_db() {
        // 模拟老库：prs 表无 source 列（含 v2 extras 列）→ Db::open 迁移后默认 'github'
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("prdesk-db-mig-src-{}-{unique}", std::process::id()));
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
        db.upsert_pr(&pr_json_src("o/r", 1, "srv", "server"))
            .unwrap();
        assert_eq!(db.list_prs(&["server", "inbox"]).unwrap().len(), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_roundtrip() {
        let db = Db::open_memory().unwrap();
        assert!(db.get_cursor().unwrap().is_none());
        db.set_cursor("2026-01-01T00:00:00").unwrap();
        assert_eq!(
            db.get_cursor().unwrap().as_deref(),
            Some("2026-01-01T00:00:00")
        );
        db.set_cursor("2026-01-02T00:00:00").unwrap();
        assert_eq!(
            db.get_cursor().unwrap().as_deref(),
            Some("2026-01-02T00:00:00")
        );
    }

    #[test]
    fn outbox_enqueue_drain_remove() {
        let db = Db::open_memory().unwrap();
        db.local_pr_update("o/r", 1, "pinned", &serde_json::json!(1))
            .unwrap();
        db.local_pr_update("o/r", 2, "priority", &serde_json::json!(2))
            .unwrap();
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
            db.upsert_pr(&pr_json_src("o/r", n, &format!("t{n}"), "github"))
                .unwrap();
        }
        let keys: Vec<(String, i64)> = (1..=3).map(|n| ("o/r".to_string(), n)).collect();
        for (repo, number) in &keys {
            db.local_pr_update(repo, *number, "priority", &serde_json::json!(2))
                .unwrap();
        }
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r["priority"] == 2));
        let items = db.outbox_drain().unwrap();
        assert_eq!(items.len(), 3, "每行一条 outbox，待推送数 = 批量大小");
        assert!(items.iter().all(|(_, p)| p["priority"] == 2));
        // 白名单：未知字段拒绝且不进 outbox
        assert!(
            db.local_pr_update("o/r", 1, "evil_field", &serde_json::json!(1))
                .is_err()
        );
        assert_eq!(db.outbox_drain().unwrap().len(), 3);
    }

    #[test]
    fn copy_prs_to_carries_prs_and_reviews_without_clobbering_target() {
        // 跨档案复制：prs（全部字段，含 head_sha）+ 全部评审记录（一个 PR 可多 sha 多条）。
        // 目标已有行整行跳过（本地策展状态不动）；重复复制幂等。
        let src = Db::open_memory().unwrap();
        let dst = Db::open_memory().unwrap();
        src.upsert_pr(&pr_json_src("o/r", 1, "src pr1", "server"))
            .unwrap();
        src.upsert_pr(&pr_json_src("o/r", 2, "src pr2", "server"))
            .unwrap();
        src.upsert_review(&review_json("o/r", 1, "sha-a", 8.0))
            .unwrap();
        src.upsert_review(&review_json("o/r", 1, "sha-b", 6.5))
            .unwrap();
        // 目标档案已有 o/r#2，且带本地策展状态（pinned/priority/status 是本地改的）
        dst.upsert_pr(&pr_json_src("o/r", 2, "dst pr2", "github"))
            .unwrap();
        dst.local_pr_update("o/r", 2, "pinned", &serde_json::json!(1))
            .unwrap();
        dst.local_pr_update("o/r", 2, "priority", &serde_json::json!(3))
            .unwrap();
        dst.local_pr_update("o/r", 2, "status", &serde_json::json!("reviewed"))
            .unwrap();

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
        assert!(
            rvs.iter()
                .any(|r| r["head_sha"] == "sha-a" && r["score"] == 8.0)
        );
        assert!(
            rvs.iter()
                .any(|r| r["head_sha"] == "sha-b" && r["score"] == 6.5)
        );

        // 重复复制幂等：PR 全部跳过，评审 upsert 同内容不翻倍
        let (inserted2, skipped2, n_reviews2) = Db::copy_prs_to(&src, &dst, &keys).unwrap();
        assert_eq!((inserted2, skipped2, n_reviews2), (0, 2, 2));
        assert_eq!(dst.list_reviews_of("o/r", 1).unwrap().len(), 2);
    }

    #[test]
    fn inbox_insert_reports_inserted_existing_and_tombstoned() {
        let db = Db::open_memory().unwrap();
        assert_eq!(
            db.insert_pr_from_inbox(&pr_json("o/r", 1, "first"))
                .unwrap(),
            InsertPrOutcome::Inserted
        );
        assert_eq!(
            db.insert_pr_from_inbox(&pr_json("o/r", 1, "overwrite"))
                .unwrap(),
            InsertPrOutcome::AlreadyExists
        );
        assert_eq!(db.get_pr_full("o/r", 1).unwrap().unwrap()["title"], "first");
        db.delete_prs(&[("o/r".to_string(), 1)]).unwrap();
        assert_eq!(
            db.insert_pr_from_inbox(&pr_json("o/r", 1, "blocked"))
                .unwrap(),
            InsertPrOutcome::Tombstoned
        );
    }

    #[test]
    fn delete_prs_removes_row_reviews_diff_and_blocks_resurrection() {
        // 删除 = 墓碑 + prs/reviews/diff_cache 一个事务清掉；
        // 防复活：删除后走墓碑守卫的插入路径（Inbox + 预评审 / 未来刷新回插）不得复活；
        // 清空墓碑后可正常回库。
        let db = Db::open_memory().unwrap();
        db.upsert_pr(&pr_json("o/r", 1, "t1")).unwrap();
        db.upsert_pr(&pr_json("o/r", 2, "t2")).unwrap();
        db.upsert_review(&review_json("o/r", 1, "sha-a", 8.0))
            .unwrap();
        db.put_diff("o/r", 1, "sha-a", "diff text").unwrap();
        db.enqueue_agent_review("o/r", 1, "initial", "b", "s", None, "w")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO agent_findings(repo,number,fingerprint) VALUES ('o/r',1,'f')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO agent_decisions(repo,number,kind) VALUES ('o/r',1,'clarification')",
                [],
            )
            .unwrap();

        let keys = vec![("o/r".to_string(), 1)];
        let deleted = db.delete_prs(&keys).unwrap();
        assert_eq!(deleted, 1);
        assert!(!db.pr_exists("o/r", 1).unwrap());
        assert!(db.pr_exists("o/r", 2).unwrap(), "未删除的行不受影响");
        assert_eq!(db.list_reviews_of("o/r", 1).unwrap().len(), 0, "评审随删");
        assert!(
            db.get_diff("o/r", 1, "sha-a").unwrap().is_none(),
            "diff 缓存随删"
        );
        assert!(db.load_agent_review("o/r", 1).unwrap().is_none());
        assert!(db.list_agent_findings("o/r", 1).unwrap().is_empty());
        assert!(db.list_agent_decisions("o/r", 1).unwrap().is_empty());
        assert!(db.is_tombstoned("o/r", 1).unwrap());
        assert!(!db.is_tombstoned("o/r", 2).unwrap());
        assert_eq!(db.tombstone_count().unwrap(), 1);

        // 模拟刷新/误加回插：守卫路径拒绝复活，普通 upsert 也先查墓碑（worker 侧模式）
        assert!(
            !db.upsert_pr_unless_tombstoned(&pr_json("o/r", 1, "resurrected"))
                .unwrap()
        );
        assert!(!db.pr_exists("o/r", 1).unwrap(), "墓碑行不得复活");
        // 幂等：重复删除不炸，墓碑不翻倍
        assert_eq!(db.delete_prs(&keys).unwrap(), 0);
        assert_eq!(db.tombstone_count().unwrap(), 1);

        // 清空删除记录后可恢复
        assert_eq!(db.clear_tombstones().unwrap(), 1);
        assert_eq!(db.tombstone_count().unwrap(), 0);
        assert!(
            db.upsert_pr_unless_tombstoned(&pr_json("o/r", 1, "back"))
                .unwrap()
        );
        assert!(db.pr_exists("o/r", 1).unwrap());
    }

    #[test]
    fn diff_cache_roundtrip_is_scoped_by_head() {
        let db = Db::open_memory().unwrap();
        assert!(db.get_diff("o/r", 1, "h1").unwrap().is_none());
        db.put_diff("o/r", 1, "h1", "diff text").unwrap();
        db.put_diff("o/r", 1, "h2", "new head").unwrap();
        assert_eq!(
            db.get_diff("o/r", 1, "h1").unwrap().as_deref(),
            Some("diff text")
        );
        assert_eq!(
            db.get_diff("o/r", 1, "h2").unwrap().as_deref(),
            Some("new head")
        );
        db.put_diff("o/r", 1, "h1", "v2").unwrap();
        assert_eq!(db.get_diff("o/r", 1, "h1").unwrap().as_deref(), Some("v2"));
        let cached = db.list_diff_cache().unwrap();
        assert_eq!(cached.len(), 2);
        assert!(
            cached
                .iter()
                .any(|(_, _, head, diff)| head == "h2" && diff == "new head")
        );
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
        let flagged = r["flagged_files"]
            .as_str()
            .expect("flagged_files 不应为 NULL");
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
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("prdesk-db-mig-{}-{unique}", std::process::id()));
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
        drop(db);
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
    fn fill_pr_meta_only_fills_empty_fields() {
        let db = Db::open_memory().unwrap();
        // Inbox 入库的残缺行：author/head_sha 为空
        db.upsert_pr(&serde_json::json!({
            "repo": "o/r", "number": 1, "title": "t", "author": "", "url": "u1",
            "head_sha": "", "status": "pending", "updated_at": "t", "source": "inbox",
        }))
        .unwrap();
        assert!(
            db.fill_pr_meta_if_empty("o/r", 1, "alice", "u2", "sha1", "t2")
                .unwrap()
        );
        let row = &db.list_prs(&[]).unwrap()[0];
        assert_eq!(row["author"], "alice"); // 空字段被补齐
        assert_eq!(row["url"], "u1"); // 已有值不动
        assert_eq!(row["title"], "t");
        // head_sha 不在 list_prs 的选择列里，直接查表验证
        let sha: String = db
            .conn
            .query_row(
                "SELECT head_sha FROM prs WHERE repo='o/r' AND number=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sha, "sha1");
        // 再补一次：全部已有值 → 什么都不变
        assert!(
            db.fill_pr_meta_if_empty("o/r", 1, "bob", "u3", "sha2", "t3")
                .unwrap()
        );
        let row = &db.list_prs(&[]).unwrap()[0];
        assert_eq!(row["author"], "alice");
        let sha: String = db
            .conn
            .query_row(
                "SELECT head_sha FROM prs WHERE repo='o/r' AND number=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sha, "sha1");
        // 不存在的行：false
        assert!(
            !db.fill_pr_meta_if_empty("o/r", 99, "x", "u", "s", "t")
                .unwrap()
        );
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
        let comments = r["comments"]
            .as_array()
            .expect("旧 comments 不应被 NULL 覆盖");
        assert_eq!(comments[0]["path"], "src/a.rs");
        assert_eq!(r["score"], 9.0); // 其余字段正常被新评审覆盖
    }

    #[test]
    fn local_pr_update_rolls_back_on_outbox_failure() {
        let db = Db::open_memory().unwrap();
        db.upsert_pr(&pr_json("o/r", 1, "t")).unwrap();
        // 拆掉 outbox 表让 INSERT 失败 → 事务整体回滚，prs.status 不变
        db.conn.execute_batch("DROP TABLE outbox").unwrap();
        assert!(
            db.local_pr_update("o/r", 1, "status", &serde_json::json!("reviewed"))
                .is_err()
        );
        let rows = db.list_prs(&[]).unwrap();
        assert_eq!(rows[0]["status"], "pending");
        // 未知字段：动库前拒绝
        assert!(
            db.local_pr_update("o/r", 1, "bogus", &serde_json::json!(1))
                .is_err()
        );
        assert_eq!(db.list_prs(&[]).unwrap()[0]["status"], "pending");
    }

    #[test]
    fn outbox_drain_skips_corrupt_payload() {
        let db = Db::open_memory().unwrap();
        db.local_pr_update("o/r", 1, "pinned", &serde_json::json!(1))
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO outbox(kind, payload, created_at) VALUES ('pr', '{bad', 't')",
                [],
            )
            .unwrap();
        db.local_pr_update("o/r", 2, "priority", &serde_json::json!(2))
            .unwrap();
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

    #[test]
    fn legacy_deep_tables_are_preserved_but_not_created_for_new_databases() {
        let dir =
            std::env::temp_dir().join(format!("prdesk-db-legacy-deep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE deep_reviews (
                    repo TEXT NOT NULL,
                    number INTEGER NOT NULL,
                    head_sha TEXT NOT NULL,
                    base_sha TEXT NOT NULL,
                    title TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    body TEXT NOT NULL DEFAULT '',
                    diff TEXT NOT NULL DEFAULT '',
                    qa_json TEXT NOT NULL DEFAULT '[]',
                    agent_session_id TEXT,
                    agent_session_cwd TEXT,
                    PRIMARY KEY (repo, number, head_sha)
                );
                CREATE TABLE deep_findings (
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
                INSERT INTO deep_reviews(repo, number, head_sha, base_sha, title, created_at)
                VALUES ('o/r', 7, 'h1', 'base', 'legacy workspace', '2026-01-01T00:00:00Z');
                PRAGMA user_version = 7;
                "#,
            )
            .unwrap();
        }

        validate_database(&path).unwrap();
        let db = Db::open(&path).unwrap();
        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let title: String = db
            .conn
            .query_row("SELECT title FROM deep_reviews", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "legacy workspace");
        let legacy_tables: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN ('deep_reviews', 'deep_findings')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_tables, 2);
        drop(db);

        let memory = Db::open_memory().unwrap();
        let new_tables: i64 = memory
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN ('deep_reviews', 'deep_findings')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new_tables, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn agent_pr(db: &Db, number: i64, head: &str) {
        let mut pr = pr_json("o/r", number, "agent");
        pr["head_sha"] = serde_json::json!(head);
        db.upsert_pr(&pr).unwrap();
    }

    fn agent_report_json(head: &str, parent: Option<&str>, complete: bool) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "repo": "o/r",
            "number": 1,
            "base_sha": "b",
            "head_sha": head,
            "parent_head_sha": parent,
            "scope_complete": complete,
            "verdict": "ready",
            "summary": "summary",
            "validation": "cargo test",
            "findings": [],
            "resolved_findings": [],
            "decision": null
        })
    }

    fn agent_finding(fingerprint: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "id": null,
            "fingerprint": fingerprint,
            "severity": "high",
            "category": "bug",
            "path": "src/lib.rs",
            "start_line": 10,
            "end_line": 12,
            "title": title,
            "evidence": "evidence",
            "suggestion": "suggestion"
        })
    }

    fn apply_report(db: &Db, run_id: i64, value: serde_json::Value) -> bool {
        let report: crate::agent_review::AgentReport = serde_json::from_value(value).unwrap();
        db.apply_agent_report(run_id, &report).unwrap()
    }

    fn start_run(db: &Db, run_id: i64) {
        assert!(db.mark_agent_run_running(run_id).unwrap());
    }

    fn complete_initial(db: &Db, number: i64, head: &str) -> i64 {
        agent_pr(db, number, head);
        let run_id = db
            .enqueue_agent_review("o/r", number, "initial", "b", head, None, "workspace")
            .unwrap();
        start_run(db, run_id);
        let mut report = agent_report_json(head, None, true);
        report["number"] = serde_json::json!(number);
        assert!(apply_report(db, run_id, report));
        run_id
    }

    #[test]
    fn migrate_v8_clears_legacy_diff_and_creates_agent_tables() {
        let dir = std::env::temp_dir().join(format!("prdesk-db-v8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute(
                "INSERT INTO diff_cache(repo,number,diff,fetched_at) VALUES ('o/r',1,'old','t')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 7).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(db.list_diff_cache().unwrap().is_empty());
        db.put_diff("o/r", 1, "h1", "one").unwrap();
        db.put_diff("o/r", 1, "h2", "two").unwrap();
        assert_eq!(db.list_diff_cache().unwrap().len(), 2);
        for table in [
            "agent_reviews",
            "agent_review_runs",
            "agent_findings",
            "agent_decisions",
        ] {
            let count: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing {table}");
        }
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_queue_is_idempotent_and_recovers_interrupted_runs() {
        let db = Db::open_memory().unwrap();
        for number in 1..=3 {
            agent_pr(&db, number, "h1");
        }
        let first = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        assert_eq!(
            db.enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
                .unwrap(),
            first
        );
        let second = db
            .enqueue_agent_review("o/r", 2, "initial", "b", "h1", None, "w")
            .unwrap();
        let third = db
            .enqueue_agent_review("o/r", 3, "initial", "b", "h1", None, "w")
            .unwrap();
        assert_eq!(db.next_agent_run().unwrap().unwrap()["id"], first);
        for id in [first, second, third] {
            start_run(&db, id);
        }
        db.conn
            .execute(
                "UPDATE agent_review_runs SET status='preparing' WHERE id=?1",
                params![second],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE agent_review_runs SET status='validating' WHERE id=?1",
                params![third],
            )
            .unwrap();
        assert_eq!(db.recover_agent_runs(&[first]).unwrap(), 2);
        assert_eq!(db.next_agent_run().unwrap().unwrap()["id"], second);
        assert_eq!(
            db.get_agent_run(first).unwrap().unwrap()["status"],
            "running"
        );
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["status"],
            "running",
            "仍在活跃进程中的 run 及其 review 不得被恢复"
        );
        assert_eq!(
            db.load_agent_review("o/r", 2).unwrap().unwrap()["status"],
            "queued"
        );
        assert!(!db.mark_agent_run_running(first).unwrap());
        start_run(&db, second);
        assert_eq!(db.next_agent_run().unwrap().unwrap()["id"], third);
    }

    #[test]
    fn partial_and_failed_runs_never_advance_reviewed_head() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        let run_id = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&db, run_id);
        let mut partial = agent_report_json("h1", None, false);
        partial["findings"] = serde_json::json!([agent_finding("partial", "not committed")]);
        assert!(apply_report(&db, run_id, partial));
        let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(review["status"], "failed");
        assert!(
            review["error"]
                .as_str()
                .unwrap()
                .contains("scope incomplete")
        );
        assert!(review["active_run_id"].is_null());
        assert!(review["reviewed_head_sha"].is_null());
        assert!(db.list_agent_findings("o/r", 1).unwrap().is_empty());
        let failed_run = db.get_agent_run(run_id).unwrap().unwrap();
        assert_eq!(failed_run["status"], "failed");
        assert!(failed_run["report"].is_object());
        assert!(
            failed_run["error"]
                .as_str()
                .unwrap()
                .contains("retry required")
        );
        assert!(db.load_agent_review("o/r", 1).unwrap().unwrap()["reviewed_head_sha"].is_null());
        assert!(
            db.list_needs_you()
                .unwrap()
                .iter()
                .any(|r| r["number"] == 1 && r["reason"] == "failed")
        );

        let complete = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&db, complete);
        assert!(apply_report(
            &db,
            complete,
            agent_report_json("h1", None, true)
        ));
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["reviewed_head_sha"],
            "h1"
        );
    }

    #[test]
    fn findings_reconcile_open_fixed_obsolete_and_missing() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        let first = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&db, first);
        let mut initial = agent_report_json("h1", None, true);
        initial["findings"] = serde_json::json!([
            agent_finding("f1", "old title"),
            agent_finding("f2", "fixed"),
            agent_finding("f3", "obsolete")
        ]);
        assert!(apply_report(&db, first, initial));
        let f1 = db
            .list_agent_findings("o/r", 1)
            .unwrap()
            .into_iter()
            .find(|f| f["fingerprint"] == "f1")
            .unwrap();
        assert!(
            db.set_finding_human_status(f1["id"].as_i64().unwrap(), "confirmed")
                .unwrap()
        );

        agent_pr(&db, 1, "h2");
        let second = db
            .enqueue_agent_review("o/r", 1, "rereview", "b", "h2", Some("h1"), "w")
            .unwrap();
        start_run(&db, second);
        let mut re_review = agent_report_json("h2", Some("h1"), true);
        re_review["findings"] = serde_json::json!([agent_finding("f1", "new title")]);
        re_review["resolved_findings"] = serde_json::json!([
            {"fingerprint":"f2", "status":"fixed", "note":"fixed in h2"},
            {"fingerprint":"f3", "status":"obsolete", "note":"code removed"}
        ]);
        assert!(apply_report(&db, second, re_review));
        let findings = db.list_agent_findings("o/r", 1).unwrap();
        let f1 = findings.iter().find(|f| f["fingerprint"] == "f1").unwrap();
        assert_eq!(f1["title"], "new title");
        assert_eq!(f1["human_status"], "confirmed");
        assert_eq!(f1["first_head_sha"], "h1");
        assert_eq!(f1["last_head_sha"], "h2");
        assert_eq!(
            findings.iter().find(|f| f["fingerprint"] == "f2").unwrap()["status"],
            "fixed"
        );
        assert_eq!(
            findings.iter().find(|f| f["fingerprint"] == "f3").unwrap()["status"],
            "obsolete"
        );
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["reviewed_head_sha"],
            "h2"
        );

        agent_pr(&db, 1, "h3");
        let third = db
            .enqueue_agent_review("o/r", 1, "rereview", "b", "h3", Some("h2"), "w")
            .unwrap();
        start_run(&db, third);
        assert!(apply_report(
            &db,
            third,
            agent_report_json("h3", Some("h2"), true)
        ));
        let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(review["status"], "failed");
        assert_eq!(review["reviewed_head_sha"], "h2");
        assert_eq!(
            db.list_agent_findings("o/r", 1)
                .unwrap()
                .iter()
                .find(|f| f["fingerprint"] == "f1")
                .unwrap()["status"],
            "stale"
        );
    }

    #[test]
    fn late_run_is_superseded_without_overwriting_new_baseline() {
        let db = Db::open_memory().unwrap();
        complete_initial(&db, 1, "h0");
        agent_pr(&db, 1, "h1");
        let old = db
            .enqueue_agent_review("o/r", 1, "rereview", "b", "h1", Some("h0"), "w")
            .unwrap();
        start_run(&db, old);
        agent_pr(&db, 1, "h2");
        let new = db
            .observe_agent_pr("o/r", 1, "h2", None, false, true)
            .unwrap()
            .unwrap();
        assert_ne!(new, old);
        assert_eq!(db.next_agent_run().unwrap().unwrap()["id"], new);
        assert!(!apply_report(
            &db,
            old,
            agent_report_json("h1", Some("h0"), true)
        ));
        let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(review["active_run_id"], new);
        assert_eq!(review["requested_head_sha"], "h2");
        assert_eq!(review["reviewed_head_sha"], "h0");
        start_run(&db, new);
        assert!(apply_report(
            &db,
            new,
            agent_report_json("h2", Some("h0"), true)
        ));
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["reviewed_head_sha"],
            "h2"
        );
    }

    fn decision_json(kind: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": kind,
            "title": "choose",
            "context": "context",
            "options": [
                {"id":"yes", "label":"Yes", "detail":"continue", "recommended":true},
                {"id":"no", "label":"No", "detail":"stop", "recommended":false}
            ],
            "review_event": if kind == "submit_review" { Some("approve") } else { None },
            "merge_method": null
        })
    }

    #[test]
    fn decisions_acknowledgement_ci_and_needs_you_follow_lifecycle() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        let run = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&db, run);
        let mut report = agent_report_json("h1", None, true);
        report["decision"] = decision_json("submit_review");
        assert!(apply_report(&db, run, report));
        let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(review["status"], "completed");
        assert_eq!(review["reviewed_head_sha"], "h1");
        let decisions = db.list_agent_decisions("o/r", 1).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["status"], "open");
        let needs = db.list_needs_you().unwrap();
        assert!(
            needs[0]["reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("open_decision"))
        );
        assert!(
            needs[0]["reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("unacknowledged"))
        );
        assert!(db.acknowledge_agent_review("o/r", 1).unwrap());
        assert_eq!(db.list_needs_you().unwrap()[0]["reason"], "open_decision");
        let answered = db
            .answer_agent_decision(decisions[0]["id"].as_i64().unwrap(), "yes")
            .unwrap()
            .unwrap();
        assert_eq!(answered["status"], "answered");
        assert_eq!(answered["answer"], "yes");
        assert_eq!(db.next_agent_run().unwrap(), None);
        assert!(db.list_needs_you().unwrap().is_empty());

        db.observe_agent_pr("o/r", 1, "h1", Some("failure"), false, false)
            .unwrap();
        assert!(
            db.list_needs_you().unwrap()[0]["reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("ci_failure"))
        );
        db.acknowledge_agent_review("o/r", 1).unwrap();
        let acknowledged =
            db.load_agent_review("o/r", 1).unwrap().unwrap()["acknowledged_at"].clone();
        db.observe_agent_pr("o/r", 1, "h1", Some("failure"), false, false)
            .unwrap();
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["acknowledged_at"],
            acknowledged
        );
        db.observe_agent_pr("o/r", 1, "h1", Some("success"), false, false)
            .unwrap();
        db.observe_agent_pr("o/r", 1, "h1", Some("failure"), false, false)
            .unwrap();
        assert!(db.load_agent_review("o/r", 1).unwrap().unwrap()["acknowledged_at"].is_null());
    }

    #[test]
    fn clarification_waits_then_answer_requeues_same_run() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        let run = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&db, run);
        let mut report = agent_report_json("h1", None, false);
        report["findings"] = serde_json::json!([agent_finding("partial", "not committed")]);
        report["decision"] = decision_json("clarification");
        assert!(apply_report(&db, run, report));
        let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(review["status"], "waiting_decision");
        assert!(review["reviewed_head_sha"].is_null());
        assert!(db.list_agent_findings("o/r", 1).unwrap().is_empty());
        let decision = db.list_agent_decisions("o/r", 1).unwrap().remove(0);
        db.answer_agent_decision(decision["id"].as_i64().unwrap(), "more context")
            .unwrap();
        assert_eq!(db.next_agent_run().unwrap().unwrap()["id"], run);
        assert!(
            db.answer_agent_decision(decision["id"].as_i64().unwrap(), "duplicate")
                .unwrap()
                .is_none(),
            "重复回答不得再次触发后续执行"
        );
        start_run(&db, run);
        assert!(apply_report(&db, run, agent_report_json("h1", None, true)));
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["reviewed_head_sha"],
            "h1"
        );
    }

    #[test]
    fn observe_rereview_is_idempotent_and_initial_inherits_parent() {
        let db = Db::open_memory().unwrap();
        complete_initial(&db, 1, "h0");
        agent_pr(&db, 1, "h1");
        let first = db
            .observe_agent_pr("o/r", 1, "h1", Some("success"), false, true)
            .unwrap()
            .unwrap();
        let second = db
            .observe_agent_pr("o/r", 1, "h1", Some("success"), false, true)
            .unwrap()
            .unwrap();
        assert_eq!(first, second);
        let queued = db.next_agent_run().unwrap().unwrap();
        assert_eq!(queued["kind"], "rereview");
        assert_eq!(queued["parent_head_sha"], "h0");
        assert!(db.fail_agent_run(first, "failed").unwrap());
        assert_eq!(
            db.observe_agent_pr("o/r", 1, "h1", Some("success"), false, true)
                .unwrap(),
            None
        );

        let manual = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        assert_eq!(
            db.next_agent_run().unwrap().unwrap()["parent_head_sha"],
            "h0"
        );
        assert!(db.cancel_agent_run(manual).unwrap());
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["status"],
            "cancelled"
        );
    }

    #[test]
    fn copy_carries_agent_snapshot_without_run_history() {
        let src = Db::open_memory().unwrap();
        let dst = Db::open_memory().unwrap();
        agent_pr(&src, 1, "h1");
        let run = src
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&src, run);
        let mut report = agent_report_json("h1", None, true);
        report["findings"] = serde_json::json!([agent_finding("f1", "finding")]);
        report["decision"] = decision_json("submit_review");
        assert!(apply_report(&src, run, report));
        let keys = [("o/r".to_string(), 1)];
        Db::copy_prs_to(&src, &dst, &keys).unwrap();
        Db::copy_prs_to(&src, &dst, &keys).unwrap();
        assert_eq!(
            dst.load_agent_review("o/r", 1).unwrap().unwrap()["reviewed_head_sha"],
            "h1"
        );
        assert_eq!(dst.list_agent_findings("o/r", 1).unwrap().len(), 1);
        let decisions = dst.list_agent_decisions("o/r", 1).unwrap();
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0]["run_id"].is_null());
        let runs: i64 = dst
            .conn
            .query_row("SELECT COUNT(*) FROM agent_review_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs, 0);

        src.enqueue_agent_review("o/r", 1, "rereview", "b", "h2", Some("h1"), "w")
            .unwrap();
        Db::copy_prs_to(&src, &dst, &keys).unwrap();
        let copied = dst.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(copied["status"], "stale");
        assert!(copied["active_run_id"].is_null());
    }

    #[test]
    fn queued_snapshot_preparation_selects_incremental_parent() {
        let db = Db::open_memory().unwrap();
        complete_initial(&db, 1, "h1");
        let run_id = db
            .enqueue_agent_review("o/r", 1, "initial", "", "", None, "")
            .unwrap();
        assert!(
            db.prepare_agent_run_snapshot(run_id, "b2", "h2", "w2")
                .unwrap()
        );
        let run = db.get_agent_run(run_id).unwrap().unwrap();
        assert_eq!(run["kind"], "rereview");
        assert_eq!(run["parent_head_sha"], "h1");
        assert_eq!(run["base_sha"], "b2");
        assert_eq!(run["head_sha"], "h2");
        assert_eq!(run["workspace"], "w2");
        let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(review["requested_head_sha"], "h2");
        assert_eq!(review["reviewed_head_sha"], "h1");
    }

    #[test]
    fn ci_and_edit_runs_never_advance_reviewed_head() {
        for (kind, parent) in [("ci_recheck", None), ("edit", Some("h1"))] {
            let db = Db::open_memory().unwrap();
            complete_initial(&db, 1, "h1");
            agent_pr(&db, 1, "h2");
            let run_id = db
                .enqueue_agent_review("o/r", 1, kind, "b", "h2", parent, "w")
                .unwrap();
            start_run(&db, run_id);
            let mut report = agent_report_json("h2", parent, true);
            report["findings"] =
                serde_json::json!([agent_finding("ci-or-edit-finding", "must not be merged")]);
            assert!(apply_report(&db, run_id, report));
            let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
            assert_eq!(review["reviewed_head_sha"], "h1", "{kind}");
            assert_eq!(review["requested_head_sha"], "h2", "{kind}");
            assert!(
                db.list_agent_findings("o/r", 1).unwrap().is_empty(),
                "{kind} 不得合并代码 findings"
            );
        }
    }

    #[test]
    fn set_agent_run_parent_accepts_queued_running_and_preparing() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        let run_id = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        for status in ["queued", "running", "preparing"] {
            db.conn
                .execute(
                    "UPDATE agent_review_runs SET status=?1 WHERE id=?2",
                    params![status, run_id],
                )
                .unwrap();
            assert!(db.set_agent_run_parent(run_id, Some(status)).unwrap());
            assert_eq!(
                db.get_agent_run(run_id).unwrap().unwrap()["parent_head_sha"],
                status
            );
        }
        db.conn
            .execute(
                "UPDATE agent_review_runs SET status='validating' WHERE id=?1",
                params![run_id],
            )
            .unwrap();
        assert!(!db.set_agent_run_parent(run_id, Some("too-late")).unwrap());
        assert_eq!(
            db.get_agent_run(run_id).unwrap().unwrap()["parent_head_sha"],
            "preparing"
        );
    }

    #[test]
    fn finding_with_same_id_inherits_fingerprint_after_title_and_location_change() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        let first = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&db, first);
        let mut initial = agent_report_json("h1", None, true);
        initial["findings"] = serde_json::json!([agent_finding("old-fingerprint", "old title")]);
        assert!(apply_report(&db, first, initial));
        let old = db.list_agent_findings("o/r", 1).unwrap().remove(0);
        let old_id = old["id"].as_i64().unwrap();

        agent_pr(&db, 1, "h2");
        let second = db
            .enqueue_agent_review("o/r", 1, "rereview", "b", "h2", Some("h1"), "w")
            .unwrap();
        start_run(&db, second);
        let mut retained = agent_finding("new-fingerprint", "new title");
        retained["id"] = serde_json::json!(old_id.to_string());
        retained["path"] = serde_json::json!("src/moved.rs");
        retained["start_line"] = serde_json::json!(99);
        retained["end_line"] = serde_json::json!(100);
        let mut report = agent_report_json("h2", Some("h1"), true);
        report["findings"] = serde_json::json!([retained]);
        assert!(apply_report(&db, second, report));

        let review = db.load_agent_review("o/r", 1).unwrap().unwrap();
        assert_eq!(review["status"], "completed");
        assert_eq!(review["reviewed_head_sha"], "h2");
        let findings = db.list_agent_findings("o/r", 1).unwrap();
        assert_eq!(findings.len(), 1, "同 ID 不得新增 finding 或误标 missing");
        assert_eq!(findings[0]["id"], old_id);
        assert_eq!(findings[0]["fingerprint"], "old-fingerprint");
        assert_eq!(findings[0]["title"], "new title");
        assert_eq!(findings[0]["path"], "src/moved.rs");
        assert_eq!(findings[0]["start_line"], 99);
        assert_eq!(findings[0]["status"], "open");
    }

    #[test]
    fn ci_failure_queues_once_and_stale_supersedes_open_decisions() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        let initial = db
            .enqueue_agent_review("o/r", 1, "initial", "b", "h1", None, "w")
            .unwrap();
        start_run(&db, initial);
        let mut report = agent_report_json("h1", None, true);
        report["decision"] = decision_json("submit_review");
        assert!(apply_report(&db, initial, report));

        let ci_run = db
            .observe_agent_pr("o/r", 1, "h1", Some("failure"), false, true)
            .unwrap()
            .unwrap();
        let queued = db.get_agent_run(ci_run).unwrap().unwrap();
        assert_eq!(queued["kind"], "ci_recheck");
        assert_eq!(queued["status"], "queued");
        assert!(queued["parent_head_sha"].is_null());
        assert_eq!(
            db.observe_agent_pr("o/r", 1, "h1", Some("failure"), false, true)
                .unwrap(),
            None,
            "相同 CI 状态的轮询不得重复唤醒 run"
        );
        let ci_runs: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_review_runs WHERE kind='ci_recheck'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ci_runs, 1);

        start_run(&db, ci_run);
        let mut ci_report = agent_report_json("h1", None, true);
        ci_report["findings"] = serde_json::json!([agent_finding(
            "ci-finding",
            "CI recheck must not merge code findings"
        )]);
        assert!(apply_report(&db, ci_run, ci_report));
        assert!(db.list_agent_findings("o/r", 1).unwrap().is_empty());
        assert_eq!(
            db.load_agent_review("o/r", 1).unwrap().unwrap()["reviewed_head_sha"],
            "h1"
        );
        assert_eq!(
            db.list_agent_decisions("o/r", 1).unwrap()[0]["status"],
            "open"
        );

        agent_pr(&db, 1, "h2");
        db.observe_agent_pr("o/r", 1, "h2", Some("failure"), false, false)
            .unwrap();
        let decision = db.list_agent_decisions("o/r", 1).unwrap().remove(0);
        assert_eq!(decision["status"], "superseded");
        assert!(!decision["resolved_at"].is_null());
        assert!(
            db.answer_agent_decision(decision["id"].as_i64().unwrap(), "too-late")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn local_review_success_does_not_write_outbox() {
        let db = Db::open_memory().unwrap();
        agent_pr(&db, 1, "h1");
        assert!(db.mark_local_review_success("o/r", 1).unwrap());
        assert_eq!(
            db.get_pr_full("o/r", 1).unwrap().unwrap()["status"],
            "reviewed"
        );
        assert!(db.outbox_drain().unwrap().is_empty());
        assert!(!db.mark_local_review_success("o/r", 1).unwrap());
        db.conn
            .execute(
                "UPDATE prs SET status='failed' WHERE repo='o/r' AND number=1",
                [],
            )
            .unwrap();
        assert!(db.mark_local_review_success("o/r", 1).unwrap());
        assert!(db.outbox_drain().unwrap().is_empty());
    }
}
