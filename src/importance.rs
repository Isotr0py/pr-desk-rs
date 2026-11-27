//! PR 对仓库的重要性评分：确定性规则 + 可展示证据，与 LLM 代码质量分相互独立。

use serde::Serialize;
use serde_json::Value;

pub const SCORING_VERSION: &str = "2026-11-27.1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportanceFactor {
    pub key: &'static str,
    pub points: u32,
    pub max_points: u32,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ImportanceScore {
    pub score: f64,
    pub raw_score: u32,
    pub scoring_version: &'static str,
    pub factors: Vec<ImportanceFactor>,
}

fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && repo
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
}

fn parse_issue_reference(text: &str, default_repo: &str) -> Option<(String, i64)> {
    let text = text.trim_start_matches([' ', ':', '\t', '(']);
    if let Some(rest) = text.strip_prefix("https://github.com/") {
        let mut parts = rest.split('/');
        let owner = parts.next()?;
        let name = parts.next()?;
        if parts.next()? != "issues" {
            return None;
        }
        let digits: String = parts
            .next()?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let number = digits.parse().ok()?;
        let repo = format!("{owner}/{name}");
        return (number > 0 && valid_repo(&repo)).then_some((repo, number));
    }
    if let Some(rest) = text.strip_prefix('#') {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let number = digits.parse().ok()?;
        return (number > 0 && valid_repo(default_repo))
            .then_some((default_repo.to_string(), number));
    }
    let (repo, suffix) = text.split_once('#')?;
    if !valid_repo(repo) {
        return None;
    }
    let digits: String = suffix.chars().take_while(char::is_ascii_digit).collect();
    let number = digits.parse().ok()?;
    (number > 0).then_some((repo.to_string(), number))
}

/// 解析正文中的 GitHub 自动关闭 Issue 引用。只接受关键字后的直接引用，去重且最多 5 条。
pub fn closing_issue_references(body: &str, default_repo: &str) -> Vec<(String, i64)> {
    let body = body.to_ascii_lowercase();
    let keywords = [
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let mut refs = Vec::new();
    for keyword in keywords {
        let mut offset = 0;
        while let Some(relative) = body[offset..].find(keyword) {
            let start = offset + relative;
            let end = start + keyword.len();
            let word_before = start > 0 && body.as_bytes()[start - 1].is_ascii_alphanumeric();
            let word_after = end < body.len() && body.as_bytes()[end].is_ascii_alphanumeric();
            offset = end;
            if word_before || word_after {
                continue;
            }
            let Some(reference) = parse_issue_reference(&body[end..], default_repo) else {
                continue;
            };
            if !refs.contains(&reference) {
                refs.push(reference);
            }
            if refs.len() == 5 {
                return refs;
            }
        }
    }
    refs
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    points: u32,
    evidence: String,
}

fn label_names(value: &Value) -> Vec<String> {
    value["labels"]["nodes"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label["name"].as_str())
                .map(|name| name.to_ascii_lowercase().replace('_', "-"))
                .collect()
        })
        .unwrap_or_default()
}

fn title_tokens(title: &str) -> Vec<&str> {
    title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect()
}

fn has_any_token(title: &str, needles: &[&str]) -> bool {
    let tokens = title_tokens(title);
    needles.iter().any(|needle| tokens.contains(needle))
}

fn label_matches(labels: &[String], needles: &[&str]) -> Option<String> {
    labels.iter().find_map(|label| {
        let normalized = label.replace('-', " ");
        needles
            .iter()
            .any(|needle| label == needle || normalized == *needle)
            .then(|| label.clone())
    })
}

fn severity_candidate(title: &str, labels: &[String], source: &str) -> Option<Candidate> {
    let title = title.to_ascii_lowercase();
    let rules: &[(u32, &[&str], &[&str], &[&str])] = &[
        (
            50,
            &["security", "vulnerability", "data-loss", "data loss"],
            &["security", "vulnerability"],
            &["data loss", "remote code execution", "数据丢失", "安全漏洞"],
        ),
        (
            45,
            &["regression", "crash", "wrong-output", "wrong output"],
            &["regression", "crash", "crashes"],
            &[
                "wrong output",
                "incorrect output",
                "回归",
                "崩溃",
                "输出错误",
            ],
        ),
        (
            35,
            &["bug", "bugfix", "defect"],
            &["bug", "bugfix", "fix", "fixes", "fixed"],
            &["修复"],
        ),
        (
            25,
            &["performance", "perf", "latency", "memory"],
            &["performance", "perf", "latency"],
            &["性能", "延迟"],
        ),
        (
            15,
            &["enhancement", "feature", "feature-request"],
            &["feature", "enhancement", "support", "add"],
            &["功能", "支持"],
        ),
        (
            5,
            &["documentation", "docs", "chore", "tests", "ci"],
            &[
                "docs",
                "doc",
                "documentation",
                "chore",
                "test",
                "tests",
                "ci",
            ],
            &["文档", "测试"],
        ),
    ];
    for (points, label_needles, token_needles, phrases) in rules {
        if let Some(label) = label_matches(labels, label_needles) {
            return Some(Candidate {
                points: *points,
                evidence: format!("{source} label: {label}"),
            });
        }
        if has_any_token(&title, token_needles)
            || phrases.iter().any(|phrase| title.contains(phrase))
        {
            return Some(Candidate {
                points: *points,
                evidence: format!("{source} title signal: {}", title.trim()),
            });
        }
    }
    None
}

fn severity_factor(pr: &Value, issues: &[Value]) -> ImportanceFactor {
    let pr_title = pr["title"].as_str().unwrap_or("");
    let mut best = severity_candidate(pr_title, &label_names(pr), "PR");
    for issue in issues {
        let number = issue["number"].as_i64().unwrap_or(0);
        let source = format!("issue #{number}");
        let candidate = severity_candidate(
            issue["title"].as_str().unwrap_or(""),
            &label_names(issue),
            &source,
        );
        if candidate.as_ref().is_some_and(|next| {
            best.as_ref()
                .is_none_or(|current| next.points > current.points)
        }) {
            best = candidate;
        }
    }
    let candidate = best.unwrap_or(Candidate {
        points: 0,
        evidence: "No severity signal in PR or linked issues".into(),
    });
    ImportanceFactor {
        key: "severity",
        points: candidate.points,
        max_points: 50,
        evidence: candidate.evidence,
    }
}

fn engagement_points(engagement: u64) -> u32 {
    match engagement {
        0 => 0,
        1..=2 => 6,
        3..=5 => 12,
        6..=15 => 18,
        16..=30 => 24,
        _ => 30,
    }
}

fn reach_factor(issues: &[Value]) -> ImportanceFactor {
    let best = issues
        .iter()
        .map(|issue| {
            let comments = issue["comments"]["totalCount"].as_u64().unwrap_or(0);
            let reactions = issue["reactions"]["totalCount"].as_u64().unwrap_or(0);
            let engagement = comments.saturating_add(reactions);
            (
                engagement,
                format!(
                    "issue #{}: {comments} comments + {reactions} reactions",
                    issue["number"].as_i64().unwrap_or(0)
                ),
            )
        })
        .max_by_key(|(engagement, _)| *engagement);
    let (points, evidence) = match best {
        Some((engagement, evidence)) => (engagement_points(engagement), evidence),
        None => (0, "No linked issue; audience reach is unknown".into()),
    };
    ImportanceFactor {
        key: "reach",
        points,
        max_points: 30,
        evidence,
    }
}

fn release_factor(pr: &Value, issues: &[Value]) -> ImportanceFactor {
    let mut best: Option<Candidate> = None;
    let mut consider = |candidate: Candidate| {
        if best
            .as_ref()
            .is_none_or(|current| candidate.points > current.points)
        {
            best = Some(candidate);
        }
    };
    let pr_labels = label_names(pr);
    if let Some(label) = label_matches(
        &pr_labels,
        &[
            "release-blocker",
            "release blocker",
            "blocker",
            "release-critical",
        ],
    ) {
        consider(Candidate {
            points: 20,
            evidence: format!("PR label: {label}"),
        });
    }
    for issue in issues {
        let number = issue["number"].as_i64().unwrap_or(0);
        if let Some(label) = label_matches(
            &label_names(issue),
            &[
                "release-blocker",
                "release blocker",
                "blocker",
                "release-critical",
            ],
        ) {
            consider(Candidate {
                points: 20,
                evidence: format!("issue #{number} label: {label}"),
            });
        }
    }
    if let Some(milestone) = pr["milestone"]["title"]
        .as_str()
        .filter(|title| !title.is_empty())
    {
        consider(Candidate {
            points: 12,
            evidence: format!("PR milestone: {milestone}"),
        });
    }
    for issue in issues {
        if let Some(milestone) = issue["milestone"]["title"]
            .as_str()
            .filter(|title| !title.is_empty())
        {
            consider(Candidate {
                points: 12,
                evidence: format!(
                    "issue #{} milestone: {milestone}",
                    issue["number"].as_i64().unwrap_or(0)
                ),
            });
        }
    }
    let base = pr["baseRefName"].as_str().unwrap_or("");
    if ["release/", "release-", "hotfix/"]
        .iter()
        .any(|prefix| base.starts_with(prefix))
    {
        consider(Candidate {
            points: 10,
            evidence: format!("PR targets release branch: {base}"),
        });
    }
    let candidate = best.unwrap_or(Candidate {
        points: 0,
        evidence: "No release blocker, milestone, or release branch signal".into(),
    });
    ImportanceFactor {
        key: "release",
        points: candidate.points,
        max_points: 20,
        evidence: candidate.evidence,
    }
}

pub fn score_pr(pr: &Value) -> ImportanceScore {
    let issues = pr["closingIssuesReferences"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let factors = vec![
        severity_factor(pr, &issues),
        reach_factor(&issues),
        release_factor(pr, &issues),
    ];
    let raw_score = factors.iter().map(|factor| factor.points).sum();
    ImportanceScore {
        score: raw_score as f64 / 10.0,
        raw_score,
        scoring_version: SCORING_VERSION,
        factors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn issue(number: i64, title: &str, labels: &[&str], comments: u64, reactions: u64) -> Value {
        let labels: Vec<Value> = labels.iter().map(|name| json!({"name": name})).collect();
        json!({
            "number": number,
            "title": title,
            "labels": {"nodes": labels},
            "comments": {"totalCount": comments},
            "reactions": {"totalCount": reactions},
            "milestone": null
        })
    }

    fn pr(title: &str, labels: &[&str], issues: Vec<Value>) -> Value {
        let labels: Vec<Value> = labels.iter().map(|name| json!({"name": name})).collect();
        json!({
            "title": title,
            "labels": {"nodes": labels},
            "baseRefName": "main",
            "milestone": null,
            "closingIssuesReferences": {"nodes": issues}
        })
    }

    #[test]
    fn severity_uses_strongest_evidence_without_double_counting() {
        let score = score_pr(&pr(
            "Fix incorrect results",
            &["bug"],
            vec![issue(7, "Incorrect results", &["wrong-output"], 0, 0)],
        ));
        assert_eq!(score.factors[0].points, 45);
        assert_eq!(score.raw_score, 45);
        assert_eq!(score.score, 4.5);
        assert!(score.factors[0].evidence.contains("wrong-output"));
    }

    #[test]
    fn title_matching_does_not_treat_prefix_as_fix() {
        let score = score_pr(&pr("Add prefix support", &[], vec![]));
        assert_eq!(score.factors[0].points, 15);
    }

    #[test]
    fn reach_uses_highest_issue_engagement_and_caps_at_thirty() {
        let score = score_pr(&pr(
            "Feature",
            &[],
            vec![
                issue(1, "Feature request", &[], 2, 1),
                issue(2, "Popular request", &[], 40, 20),
            ],
        ));
        assert_eq!(score.factors[1].points, 30);
        assert!(score.factors[1].evidence.contains("issue #2"));
    }

    #[test]
    fn reach_tiers_and_missing_issue_are_explicit() {
        for (engagement, expected) in [(0, 0), (1, 6), (3, 12), (6, 18), (16, 24), (31, 30)] {
            let score = score_pr(&pr(
                "Feature",
                &[],
                vec![issue(1, "Feature", &[], engagement, 0)],
            ));
            assert_eq!(score.factors[1].points, expected);
        }
        let score = score_pr(&pr("Unclassified", &[], vec![]));
        assert_eq!(score.factors[1].points, 0);
        assert!(score.factors[1].evidence.contains("unknown"));
    }

    #[test]
    fn release_uses_strongest_signal_across_pr_and_issues() {
        let mut input = pr("Feature", &[], vec![issue(1, "Feature", &[], 0, 0)]);
        input["milestone"] = json!({"title": "v1.0"});
        assert_eq!(score_pr(&input).factors[2].points, 12);
        input["baseRefName"] = json!("release/v1");
        assert_eq!(score_pr(&input).factors[2].points, 12);
        input["closingIssuesReferences"]["nodes"][0]["labels"]["nodes"] =
            json!([{"name": "release-blocker"}]);
        assert_eq!(score_pr(&input).factors[2].points, 20);
    }

    #[test]
    fn closing_issue_parser_accepts_common_forms_and_deduplicates() {
        let body = "Fixes #68575. Also resolves other/repo#12 and closes https://github.com/another/project/issues/8. Fixed #68575";
        let refs = closing_issue_references(body, "vllm-project/vllm");
        assert_eq!(refs.len(), 3);
        assert!(refs.contains(&("vllm-project/vllm".into(), 68575)));
        assert!(refs.contains(&("other/repo".into(), 12)));
        assert!(refs.contains(&("another/project".into(), 8)));
    }

    #[test]
    fn closing_issue_parser_rejects_non_references_and_keyword_substrings() {
        for body in [
            "prefix #12",
            "fixes pull request #12",
            "fixes https://github.com/o/r/pull/12",
            "see #12",
            "fixed #0",
        ] {
            assert!(closing_issue_references(body, "o/r").is_empty(), "{body}");
        }
        let refs = closing_issue_references(
            "fixes #1, fixes #2, fixes #3, fixes #4, fixes #5, fixes #6",
            "o/r",
        );
        assert_eq!(refs.len(), 5);
    }

    #[test]
    fn empty_input_is_a_scored_zero_with_versioned_explanations() {
        let score = score_pr(&json!({}));
        assert_eq!(score.score, 0.0);
        assert_eq!(score.raw_score, 0);
        assert_eq!(score.scoring_version, SCORING_VERSION);
        assert_eq!(score.factors.len(), 3);
        assert!(score.factors.iter().all(|factor| factor.points == 0));
    }
}
