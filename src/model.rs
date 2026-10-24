//! 行模型装配：serde_json 行 → PrListRow + PrGroup（Slint 生成类型，经 build_sections 分组），
//! 以及相对时间（age_text/updated_text）、次级活动行（activity_text）、
//! 作者组织（author_orgs）与过滤（filter_rows/row_matches）的派生函数。
//!
//! 集成注意：PrListRow（pr_list.slint）若新增 `activity_text` 字段，
//! 在 to_row 里加一行 `activity_text: activity_text(r).into()` 即可。

use chrono::{Local, NaiveDateTime};
use pr_desk_rs::build_sections;

use crate::avatars;
use crate::{PrGroup, PrListRow};

/// 解析 updated_at：兼容 db::now() 的本地 "YYYY-MM-DDTHH:MM:SS"、带小数秒、
/// 空格分隔以及 RFC3339（带时区）格式。
pub fn parse_ts(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t);
        }
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Local).naive_local())
}

/// 相对时间格式："27m" / "2h" / "3d" / "2mo" / "1y"；未来时间归零为 "0m"，无时间戳返回 ""。
pub fn rel_time_at(ts: Option<NaiveDateTime>, now: NaiveDateTime) -> String {
    let Some(ts) = ts else { return String::new() };
    let mins = (now - ts).num_minutes().max(0);
    if mins < 60 {
        format!("{mins}m")
    } else if mins < 24 * 60 {
        format!("{}h", mins / 60)
    } else if mins < 30 * 24 * 60 {
        format!("{}d", mins / (24 * 60))
    } else if mins < 365 * 24 * 60 {
        format!("{}mo", mins / (30 * 24 * 60))
    } else {
        format!("{}y", mins / (365 * 24 * 60))
    }
}

pub fn rel_time(updated_at: &str) -> String {
    rel_time_at(parse_ts(updated_at), Local::now().naive_local())
}

/// 「距今多久」。prs 表只存 updated_at，age 与 updated 同义，两个名字都给 UI 备用。
pub fn age_text(r: &serde_json::Value) -> String {
    rel_time(r["updated_at"].as_str().unwrap_or(""))
}

// prs 表只存 updated_at，updated 与 age 同义；集成方按需选用
#[allow(dead_code)]
pub fn updated_text(r: &serde_json::Value) -> String {
    age_text(r)
}

/// 行内评审条数：优先 extras_detail.reviews 数组长度，其次 reviews 数字字段。
fn review_count(r: &serde_json::Value) -> i64 {
    if let Some(n) = r["reviews"].as_i64() {
        return n;
    }
    if let Some(detail) = r["extras_detail"].as_str() {
        if let Ok(d) = serde_json::from_str::<serde_json::Value>(detail) {
            if let Some(rvs) = d["reviews"].as_array() {
                return rvs.len() as i64;
            }
        }
    }
    0
}

/// 次级活动行（dim 文本）：「3 条评审 · 更新于 27m 前」/「更新于 27m 前」；
/// 既无评审也无时间戳时返回 ""。
pub fn activity_text(r: &serde_json::Value) -> String {
    let updated = rel_time(r["updated_at"].as_str().unwrap_or(""));
    let reviews = review_count(r);
    let mut parts = Vec::new();
    if reviews > 0 {
        parts.push(format!("{reviews} 条评审"));
    }
    if !updated.is_empty() {
        parts.push(format!("更新于 {updated} 前"));
    }
    parts.join(" · ")
}

/// 大小写不敏感子串匹配：title / number（query 可带 # 前缀）/ author / repo。
/// query_lower 须已转小写；空 query 恒真。
pub fn row_matches(r: &serde_json::Value, query_lower: &str) -> bool {
    let q = query_lower.trim().trim_start_matches('#');
    if q.is_empty() {
        return true;
    }
    let hit = |s: &str| s.to_lowercase().contains(q);
    hit(r["title"].as_str().unwrap_or(""))
        || hit(r["author"].as_str().unwrap_or(""))
        || hit(r["repo"].as_str().unwrap_or(""))
        || r["number"]
            .as_i64()
            .map(|n| n.to_string())
            .is_some_and(|n| n.contains(q))
}

/// 大小写不敏感子串过滤（title/number/author/repo）；空 query 原样返回。
/// 独立过滤入口（集成方 / 测试用）；main.rs 内部用 row_matches 保留下标映射。
#[allow(dead_code)]
pub fn filter_rows(rows: &[serde_json::Value], query: &str) -> Vec<serde_json::Value> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return rows.to_vec();
    }
    rows.iter().filter(|r| row_matches(r, &q)).cloned().collect()
}

/// 作者所属组织列表。GitHub/sync 侧在行数据上提供 `author_orgs` 字段时生效，
/// 兼容 JSON 数组与 JSON 字符串两种形态；没有该字段时返回空 Vec。
pub fn author_orgs(r: &serde_json::Value) -> Vec<String> {
    match &r["author_orgs"] {
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        serde_json::Value::String(s) => serde_json::from_str::<Vec<String>>(s)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// 组标题 → PrGroup.key。
fn group_key(title: &str) -> &'static str {
    if title.contains("Pinned") {
        "pinned"
    } else if title.contains("看板") {
        "board"
    } else if title.contains("待评审") {
        "needs-review"
    } else if title.contains("已评审") {
        "reviewed"
    } else if title.contains("不采纳") {
        "rejected"
    } else {
        "closed"
    }
}

/// 行数据 → (扁平 PrListRow 模型, PrGroup 切片元数据)。
/// PrListView 的 rows 不含组头，groups 以 start/count 切片；selected_index 直接是 rows 下标。
pub fn build_models(rows: &[serde_json::Value]) -> (Vec<PrListRow>, Vec<PrGroup>) {
    let mut out_rows: Vec<PrListRow> = Vec::new();
    let mut groups: Vec<PrGroup> = Vec::new();
    for (title, idxs) in build_sections(rows) {
        let start = out_rows.len() as i32;
        for i in &idxs {
            out_rows.push(to_row(&rows[*i]));
        }
        groups.push(PrGroup {
            key: group_key(&title).into(),
            name: title.into(),
            start,
            count: idxs.len() as i32,
            collapsed: false,
        });
    }
    (out_rows, groups)
}

pub fn to_row(r: &serde_json::Value) -> PrListRow {
    let author = r["author"].as_str().unwrap_or("");
    PrListRow {
        key: format!("{}#{}", r["repo"].as_str().unwrap_or(""), r["number"].as_i64().unwrap_or(0)).into(),
        number: r["number"].as_i64().unwrap_or(0) as i32,
        title: r["title"].as_str().unwrap_or("").into(),
        priority: r["priority"].as_i64().unwrap_or(0) as i32,
        author: author.into(),
        avatar: avatars::get_avatar(author).unwrap_or_default(),
        is_org: !author_orgs(r).is_empty(),
        turn: r["turn"].as_str().unwrap_or("").into(),
        review: r["review_decision"].as_str().unwrap_or("").into(),
        ci: r["ci_status"].as_str().unwrap_or("").into(),
        size: r["size_label"].as_str().unwrap_or("").into(),
        additions: r["additions"].as_i64().unwrap_or(0) as i32,
        deletions: r["deletions"].as_i64().unwrap_or(0) as i32,
        score: r["score"].as_f64().unwrap_or(0.0) as f32,
        age: age_text(r).into(),
        updated: age_text(r).into(),
        activity_text: activity_text(r).into(),
        pinned: r["pinned"].as_i64() == Some(1),
        on_board: r["on_board"].as_i64() == Some(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-02-01T12:00:00", "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    #[test]
    fn rel_time_formats() {
        let n = now();
        let at = |d: Duration| rel_time_at(Some(n - d), n);
        assert_eq!(at(Duration::minutes(27)), "27m");
        assert_eq!(at(Duration::seconds(30)), "0m");
        assert_eq!(at(Duration::hours(2)), "2h");
        assert_eq!(at(Duration::days(3)), "3d");
        assert_eq!(at(Duration::days(60)), "2mo");
        assert_eq!(at(Duration::days(400)), "1y");
        assert_eq!(at(Duration::days(-1)), "0m"); // 未来时间归零
        assert_eq!(rel_time_at(None, n), "");
    }

    #[test]
    fn parse_ts_formats() {
        let expect = NaiveDateTime::parse_from_str("2026-01-02T03:04:05", "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(parse_ts("2026-01-02T03:04:05"), Some(expect));
        assert_eq!(
            parse_ts("2026-01-02T03:04:05.123").map(|t| t.and_utc().timestamp_millis()),
            Some(expect.and_utc().timestamp_millis() + 123)
        );
        assert_eq!(parse_ts("2026-01-02 03:04:05"), Some(expect));
        assert!(parse_ts("2026-01-02T03:04:05Z").is_some());
        assert_eq!(parse_ts(""), None);
        assert_eq!(parse_ts("garbage"), None);
    }

    #[test]
    fn author_orgs_parses_both_shapes() {
        let arr = serde_json::json!({"author_orgs": ["acme", "oss "]});
        assert_eq!(author_orgs(&arr), vec!["acme".to_string(), "oss".to_string()]);
        let s = serde_json::json!({"author_orgs": "[\"acme\"]"});
        assert_eq!(author_orgs(&s), vec!["acme".to_string()]);
        let none = serde_json::json!({});
        assert!(author_orgs(&none).is_empty());
    }

    #[test]
    fn activity_text_combines_reviews_and_updated() {
        // activity_text 基于真实当前时间，测试数据也用 Local::now() 构造
        let recent = (Local::now().naive_local() - Duration::minutes(27))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let r = serde_json::json!({
            "updated_at": recent,
            "extras_detail": serde_json::to_string(&serde_json::json!({
                "reviews": [{"author": "a"}, {"author": "b"}, {"author": "c"}]
            })).unwrap(),
        });
        assert_eq!(activity_text(&r), "3 条评审 · 更新于 27m 前");
        // reviews 数字字段形态
        let r2 = serde_json::json!({"updated_at": recent, "reviews": 2});
        assert_eq!(activity_text(&r2), "2 条评审 · 更新于 27m 前");
        // 只有更新时间
        let r3 = serde_json::json!({"updated_at": recent});
        assert_eq!(activity_text(&r3), "更新于 27m 前");
        // 只有评审数
        let r4 = serde_json::json!({"reviews": 1});
        assert_eq!(activity_text(&r4), "1 条评审");
        // 都没有
        assert_eq!(activity_text(&serde_json::json!({})), "");
    }

    fn frow(repo: &str, number: i64, title: &str, author: &str) -> serde_json::Value {
        serde_json::json!({"repo": repo, "number": number, "title": title, "author": author})
    }

    #[test]
    fn filter_rows_empty_query_passthrough() {
        let rows = vec![frow("o/r", 1, "Fix bug", "alice")];
        assert_eq!(filter_rows(&rows, ""), rows);
        assert_eq!(filter_rows(&rows, "   "), rows);
    }

    #[test]
    fn filter_rows_case_insensitive_substring() {
        let rows = vec![
            frow("acme/web", 12, "Fix Login Bug", "Alice"),
            frow("acme/api", 34, "Add endpoint", "bob"),
            frow("oss/lib", 56, "Refactor parser", "carol"),
        ];
        // title 大小写不敏感
        assert_eq!(filter_rows(&rows, "login").len(), 1);
        assert_eq!(filter_rows(&rows, "LOGIN").len(), 1);
        // author
        assert_eq!(filter_rows(&rows, "ali")[0]["number"], 12);
        // repo
        assert_eq!(filter_rows(&rows, "acme").len(), 2);
        // number 子串 + 可选 # 前缀
        assert_eq!(filter_rows(&rows, "34")[0]["title"], "Add endpoint");
        assert_eq!(filter_rows(&rows, "#56")[0]["author"], "carol");
        // 无匹配
        assert!(filter_rows(&rows, "zzz").is_empty());
    }
}
