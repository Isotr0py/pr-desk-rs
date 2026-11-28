//! 评审与重要性展示模型：将数据库 JSON 转换为界面展示数据和 Markdown。

use serde_json::Value;

use crate::i18n::{tr, trf};

/// OCR 行级评论展示数据。
#[derive(Clone)]
pub struct OcrCommentRow {
    pub severity: String,
    pub category: String,
    pub location: String, // 如 "src/foo.rs:42-58"
    pub content: String,
}

/// 详情面板「评审」tab 的展示数据（从 db.get_review 的 JSON 格式化而来）。
#[derive(Clone, Default)]
pub struct ReviewProps {
    pub has: bool,
    pub score: f32,
    // 结构化分项（中文标签 + 分数）
    pub dimensions: Vec<(String, f32)>,
    pub summary: String,
    // 体量行，如 "XL（+2122/-23，24 个文件）"；缺字段时给空串
    pub size_line: String,
    pub flagged: Vec<String>,
    pub reviewed_at: String,
    pub comments: Vec<OcrCommentRow>,
}

#[derive(Clone, Default)]
pub struct ImportanceFactorProps {
    pub group: String,
    pub label: String,
    pub points: i32,
    pub max_points: u32,
    pub evidence: String,
    pub rule_evidence: String,
}

#[derive(Clone, Default)]
pub struct ImportanceProps {
    pub score: Option<f32>,
    pub factors: Vec<ImportanceFactorProps>,
    pub method: String,
    pub version: String,
    pub updated_at: String,
}

/// 评审分项键名 → 界面语言名（未知键原样展示）。
pub fn dim_label<'a>(key: &'a str, lang: &str) -> std::borrow::Cow<'a, str> {
    match key {
        "readability" => tr(lang, "dim_readability").into(),
        "correctness_risk" => tr(lang, "dim_correctness_risk").into(),
        "test_coverage" => tr(lang, "dim_test_coverage").into(),
        "security" => tr(lang, "dim_security").into(),
        other => other.into(),
    }
}

/// 评审字段可能是 JSON 字符串（旧数据）或内联对象/数组，两种都解析。
pub fn json_field(rv: &Value, key: &str) -> Option<Value> {
    let v = &rv[key];
    if let Some(s) = v.as_str() {
        serde_json::from_str(s).ok()
    } else if v.is_null() {
        None
    } else {
        Some(v.clone())
    }
}

/// OCR 评论严重度排序权重：critical → high → medium → low → 其他。
pub fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

/// 评审 JSON → 可粘贴到 GitHub 评论的 Markdown 全文（评分/维度/摘要/风险文件/行级评论）。
pub fn build_review_markdown(repo: &str, number: i64, rv: &Value, lang: &str) -> String {
    let mut s = format!("## Review: {repo}#{number}\n\n");
    if let Some(score) = rv["score"].as_f64() {
        s.push_str(&format!("**Score: {score:.1}/10**"));
        if let Some(sz) = rv["size_label"].as_str().filter(|x| !x.is_empty()) {
            s.push_str(&format!(" · {sz}"));
            if let (Some(a), Some(d), Some(f)) = (
                rv["additions"].as_i64(),
                rv["deletions"].as_i64(),
                rv["files_changed"].as_i64(),
            ) {
                s.push_str(&format!(" (+{a}/-{d}, {f} files)"));
            }
        }
        s.push_str("\n\n");
    }
    // dimensions 在库里是 JSON 字符串（尺寸小，解析失败就跳过整节）
    if let Some(dims) = rv["dimensions"]
        .as_str()
        .and_then(|d| serde_json::from_str::<serde_json::Map<String, Value>>(d).ok())
        .filter(|d| !d.is_empty())
    {
        for (k, v) in &dims {
            let val = v.as_f64().map(|x| format!("{x}")).unwrap_or_default();
            s.push_str(&format!("- {}: {val}\n", dim_label(k, lang)));
        }
        s.push('\n');
    }
    if let Some(sum) = rv["summary"].as_str().filter(|x| !x.is_empty()) {
        s.push_str("### Summary\n\n");
        s.push_str(sum);
        s.push_str("\n\n");
    }
    if let Some(fl) = rv["flagged_files"]
        .as_str()
        .and_then(|f| serde_json::from_str::<Vec<Value>>(f).ok())
        .filter(|v| !v.is_empty())
    {
        s.push_str("### Flagged files\n\n");
        for f in fl {
            s.push_str(&format!(
                "- `{}` — {}\n",
                f["path"].as_str().unwrap_or("?"),
                f["issue"].as_str().unwrap_or("")
            ));
        }
        s.push('\n');
    }
    if let Some(comments) = rv["comments"].as_array().filter(|c| !c.is_empty()) {
        s.push_str("### Line comments\n\n");
        for c in comments {
            s.push_str(&format!(
                "- **[{}]** `{}`: {}\n",
                c["severity"].as_str().unwrap_or("other"),
                ocr_comment_location(c),
                c["content"].as_str().unwrap_or("")
            ));
        }
    }
    s.trim_end().to_string() + "\n"
}

/// OCR 评论位置格式化：path:start-end；start=end 只一个行号；都是 0 只留 path。
pub fn ocr_comment_location(c: &Value) -> String {
    let path = c["path"].as_str().unwrap_or("");
    let start = c["start_line"].as_i64().unwrap_or(0);
    let end = c["end_line"].as_i64().unwrap_or(0);
    if start == 0 && end == 0 {
        path.to_string()
    } else if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

/// db.get_review 的 "comments" 数组 → 行级评论展示数据（按 severity 分组排序，组内保持原序）。
pub fn ocr_comment_rows(rv: &Value) -> Vec<OcrCommentRow> {
    let mut rows: Vec<OcrCommentRow> = json_field(rv, "comments")
        .and_then(|c| c.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|c| OcrCommentRow {
            severity: c["severity"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("other")
                .to_string(),
            category: c["category"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("other")
                .to_string(),
            location: ocr_comment_location(c),
            content: c["content"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    rows.sort_by_key(|c| severity_rank(&c.severity));
    rows
}

/// 体量行组装："XL（+2122/-23，24 个文件）"；label/增删行/文件数全缺时给空串。
pub fn size_line(rv: &Value, lang: &str) -> String {
    let label = rv["size_label"].as_str().unwrap_or("");
    let adds = rv["additions"].as_i64();
    let dels = rv["deletions"].as_i64();
    let files = rv["files_changed"].as_i64();
    if label.is_empty() && adds.is_none() && dels.is_none() && files.is_none() {
        return String::new();
    }
    let mut parts = Vec::new();
    if adds.is_some() || dels.is_some() {
        parts.push(format!("+{}/-{}", adds.unwrap_or(0), dels.unwrap_or(0)));
    }
    if let Some(f) = files {
        parts.push(trf(lang, "size_files", &[&f.to_string()]));
    }
    if parts.is_empty() {
        label.to_string()
    } else {
        let joined = parts.join(tr(lang, "list_sep"));
        if label.is_empty() {
            trf(lang, "size_wrap_fmt", &[&joined])
        } else {
            trf(lang, "size_full_fmt", &[label, &joined])
        }
    }
}

pub fn review_props(review: Option<&Value>, lang: &str) -> ReviewProps {
    let Some(rv) = review else {
        return ReviewProps::default();
    };
    let dims = json_field(rv, "dimensions")
        .and_then(|d| d.as_object().cloned())
        .unwrap_or_default();
    let dimensions = dims
        .iter()
        .map(|(k, v)| {
            (
                dim_label(k, lang).to_string(),
                v.as_f64().unwrap_or(0.0) as f32,
            )
        })
        .collect();
    let summary = rv["summary"].as_str().unwrap_or("").to_string();
    let flagged = json_field(rv, "flagged_files")
        .and_then(|f| f.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|f| {
            format!(
                "{} — {}",
                f["path"].as_str().unwrap_or("?"),
                f["issue"].as_str().unwrap_or("")
            )
        })
        .collect();
    ReviewProps {
        has: true,
        score: rv["score"].as_f64().unwrap_or(0.0) as f32,
        dimensions,
        summary,
        size_line: size_line(rv, lang),
        flagged,
        reviewed_at: rv["reviewed_at"].as_str().unwrap_or("").to_string(),
        comments: ocr_comment_rows(rv),
    }
}

pub fn importance_props(row: Option<&Value>, lang: &str) -> ImportanceProps {
    let Some(row) = row else {
        return ImportanceProps::default();
    };
    let Some(score) = row["importance_score"].as_f64() else {
        return ImportanceProps::default();
    };
    let factors = json_field(row, "importance_factors")
        .and_then(|factors| factors.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|factor| {
            let key = factor["key"].as_str().unwrap_or("");
            let group = factor["group"].as_str().unwrap_or(key);
            let group = match group {
                "severity" => "how_bad",
                "reach" => "community",
                other => other,
            };
            let label_key = match group {
                "how_bad" => "importance_group_how_bad",
                "who_it_hits" => "importance_group_who_it_hits",
                "release" => "importance_group_release",
                "evidence" => "importance_group_evidence",
                "community" => "importance_group_community",
                "pr_signals" => "importance_group_pr_signals",
                "penalty" => "importance_group_penalty",
                _ => "importance_factor_unknown",
            };
            let evidence = factor["evidence"].as_str().unwrap_or("");
            let evidence = evidence
                .strip_prefix("智能校准: ")
                .or_else(|| evidence.strip_prefix("K3: "))
                .unwrap_or(evidence);
            let (reason, rule_evidence) = evidence
                .split_once(" | Rule evidence: ")
                .map(|(reason, rule)| (reason.to_string(), rule.to_string()))
                .unwrap_or_else(|| (evidence.to_string(), evidence.to_string()));
            ImportanceFactorProps {
                group: group.to_string(),
                label: tr(lang, label_key).to_string(),
                points: factor["points"].as_i64().unwrap_or(0) as i32,
                max_points: factor["max_points"].as_u64().unwrap_or(0) as u32,
                evidence: reason,
                rule_evidence,
            }
        })
        .collect();
    ImportanceProps {
        score: Some(score as f32),
        factors,
        method: row["importance_method"].as_str().unwrap_or("").to_string(),
        version: row["importance_version"].as_str().unwrap_or("").to_string(),
        updated_at: row["importance_updated_at"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_line_formats_and_handles_missing_fields() {
        let rv = serde_json::json!({
            "size_label": "XL", "additions": 2122, "deletions": 23, "files_changed": 24,
        });
        assert_eq!(size_line(&rv, "zh"), "XL（+2122/-23，24 个文件）");
        assert_eq!(size_line(&rv, "en"), "XL (+2122/-23, 24 files)");
        assert_eq!(size_line(&serde_json::json!({}), "zh"), "");
        assert_eq!(
            size_line(&serde_json::json!({"size_label": "S"}), "zh"),
            "S"
        );
        assert_eq!(
            size_line(
                &serde_json::json!({"additions": 10, "deletions": 2, "files_changed": 3}),
                "zh"
            ),
            "（+10/-2，3 个文件）"
        );
    }

    #[test]
    fn review_props_exposes_structured_dimensions_and_size() {
        let rv = serde_json::json!({
            "score": 8.0,
            "dimensions": "{\"readability\": 8, \"security\": 6}",
            "summary": "## 结论\n整体可用",
            "size_label": "M", "additions": 100, "deletions": 5, "files_changed": 4,
        });
        let p = review_props(Some(&rv), "zh");
        assert!(p.has);
        assert_eq!(p.size_line, "M（+100/-5，4 个文件）");
        assert!(p.dimensions.contains(&("可读性".to_string(), 8.0)));
        assert!(p.dimensions.contains(&("安全".to_string(), 6.0)));
        assert!(p.summary.contains("结论"));
        let pe = review_props(Some(&rv), "en");
        assert_eq!(pe.size_line, "M (+100/-5, 4 files)");
        assert!(pe.dimensions.contains(&("Readability".to_string(), 8.0)));
        assert!(pe.dimensions.contains(&("Security".to_string(), 6.0)));
        let p0 = review_props(None, "zh");
        assert!(!p0.has);
        assert!(p0.dimensions.is_empty());
        assert_eq!(p0.size_line, "");
    }

    #[test]
    fn importance_props_distinguishes_missing_and_zero_scores() {
        assert!(importance_props(None, "zh").score.is_none());
        assert!(
            importance_props(Some(&serde_json::json!({})), "zh")
                .score
                .is_none()
        );
        let row = serde_json::json!({
            "importance_score": 0.0,
            "importance_factors": [
                {"key": "severity", "points": 0, "max_points": 50, "evidence": "K3: calibrated"},
                {"key": "reach", "points": 0, "max_points": 30, "evidence": "No issue"},
                {"group": "penalty", "key": "change_penalties", "points": -5, "max_points": 15, "evidence": "Model rationale | Rule evidence: raw evidence"}
            ],
            "importance_version": "v1",
            "importance_updated_at": "2026-11-27T00:00:00Z"
        });
        let props = importance_props(Some(&row), "zh");
        assert_eq!(props.score, Some(0.0));
        assert_eq!(props.factors.len(), 3);
        assert_eq!(props.factors[0].label, "缺陷严重度");
        assert_eq!(props.factors[0].evidence, "calibrated");
        assert_eq!(props.factors[0].rule_evidence, "calibrated");
        assert_eq!(props.factors[1].label, "社区关注");
        assert_eq!(props.factors[2].label, "惩罚");
        assert_eq!(props.factors[2].evidence, "Model rationale");
        assert_eq!(props.factors[2].rule_evidence, "raw evidence");
        assert_eq!(props.factors[2].points, -5);
        assert_eq!(props.version, "v1");
        let english = importance_props(Some(&row), "en");
        assert_eq!(english.factors[0].label, "How bad");
        assert_eq!(english.factors[1].label, "Community");
    }

    #[test]
    fn review_markdown_preserves_sections_and_locations() {
        let rv = serde_json::json!({
            "score": 8.0,
            "size_label": "M",
            "additions": 100,
            "deletions": 5,
            "files_changed": 4,
            "dimensions": "{\"readability\":8}",
            "summary": "Looks good",
            "flagged_files": "[{\"path\":\"src/lib.rs\",\"issue\":\"Needs validation\"}]",
            "comments": [
                {"severity":"high", "path":"src/lib.rs", "start_line":10, "end_line":12, "content":"Guard empty input"},
                {"severity":"low", "path":"src/other.rs", "content":"Fix typo"}
            ]
        });
        assert_eq!(
            build_review_markdown("o/r", 42, &rv, "zh"),
            "## Review: o/r#42\n\n\
**Score: 8.0/10** · M (+100/-5, 4 files)\n\n\
- 可读性: 8\n\n\
### Summary\n\n\
Looks good\n\n\
### Flagged files\n\n\
- `src/lib.rs` — Needs validation\n\n\
### Line comments\n\n\
- **[high]** `src/lib.rs:10-12`: Guard empty input\n\
- **[low]** `src/other.rs`: Fix typo\n"
        );
    }

    #[test]
    fn ocr_comment_rows_sorts_severity_stably_and_formats_locations() {
        let rv = serde_json::json!({
            "comments": [
                {"severity":"low", "category":"style", "path":"src/low.rs", "content":"low"},
                {"severity":"high", "category":"correctness", "path":"src/first.rs", "start_line":7, "end_line":7, "content":"first high"},
                {"severity":"critical", "category":"security", "path":"src/critical.rs", "start_line":2, "end_line":4, "content":"critical"},
                {"severity":"high", "category":"correctness", "path":"src/second.rs", "content":"second high"},
                {"path":"src/other.rs", "content":"defaults"}
            ]
        });
        let rows = ocr_comment_rows(&rv);
        assert_eq!(
            rows.iter()
                .map(|row| row.content.as_str())
                .collect::<Vec<_>>(),
            vec!["critical", "first high", "second high", "low", "defaults"]
        );
        assert_eq!(rows[0].location, "src/critical.rs:2-4");
        assert_eq!(rows[1].location, "src/first.rs:7");
        assert_eq!(rows[3].location, "src/low.rs");
        assert_eq!(rows[4].severity, "other");
        assert_eq!(rows[4].category, "other");
    }
}
