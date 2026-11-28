//! PR 对仓库的重要性评分：确定性多因子规则 + 可展示证据，与 LLM 代码质量分独立。

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const SCORING_VERSION: &str = "2026-11-28.4";
pub const MIN_RAW_SCORE: i32 = 0;
pub const MAX_RAW_SCORE: i32 = 100;
pub const SCORE_SCALE: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FactorSpec {
    pub group: &'static str,
    pub key: &'static str,
    pub min_points: i32,
    pub max_points: i32,
}

impl FactorSpec {
    pub const fn display_max_points(&self) -> u32 {
        if self.max_points > 0 {
            self.max_points as u32
        } else {
            self.min_points.unsigned_abs()
        }
    }
}

pub const FACTOR_SPECS: &[FactorSpec] = &[
    FactorSpec {
        group: "how_bad",
        key: "severity",
        min_points: 0,
        max_points: 50,
    },
    FactorSpec {
        group: "who_it_hits",
        key: "production_surface",
        min_points: 0,
        max_points: 15,
    },
    FactorSpec {
        group: "who_it_hits",
        key: "model_popularity",
        min_points: 0,
        max_points: 6,
    },
    FactorSpec {
        group: "who_it_hits",
        key: "gpu_tier",
        min_points: 0,
        max_points: 5,
    },
    FactorSpec {
        group: "release",
        key: "release_urgency",
        min_points: 0,
        max_points: 10,
    },
    FactorSpec {
        group: "evidence",
        key: "reproduction",
        min_points: 0,
        max_points: 6,
    },
    FactorSpec {
        group: "evidence",
        key: "diagnostics",
        min_points: 0,
        max_points: 4,
    },
    FactorSpec {
        group: "community",
        key: "comments",
        min_points: 0,
        max_points: 6,
    },
    FactorSpec {
        group: "community",
        key: "reactions",
        min_points: 0,
        max_points: 4,
    },
    FactorSpec {
        group: "pr_signals",
        key: "linked_issue",
        min_points: 0,
        max_points: 10,
    },
    FactorSpec {
        group: "pr_signals",
        key: "includes_tests",
        min_points: 0,
        max_points: 5,
    },
    FactorSpec {
        group: "pr_signals",
        key: "focused_change",
        min_points: 0,
        max_points: 5,
    },
    FactorSpec {
        group: "pr_signals",
        key: "production_change",
        min_points: 0,
        max_points: 5,
    },
    FactorSpec {
        group: "penalty",
        key: "change_penalties",
        min_points: -15,
        max_points: 0,
    },
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportanceFactor {
    pub group: &'static str,
    pub key: &'static str,
    pub points: i32,
    pub max_points: u32,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ImportanceScore {
    pub score: f64,
    pub raw_score: u32,
    pub scoring_version: &'static str,
    pub factors: Vec<ImportanceFactor>,
    pub method: String,
    pub input_hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ImportanceCandidate {
    pub score: ImportanceScore,
    pub input: Value,
}

impl ImportanceCandidate {
    pub fn from_pr(pr: &Value) -> Self {
        let input = importance_input(pr);
        let score = score_canonical_input(&input);
        Self { score, input }
    }
}

pub fn importance_input(pr: &Value) -> Value {
    let mut input = pr.clone();
    if let Some(body) = input["body"].as_str() {
        input["body"] = json!(body.chars().take(6_000).collect::<String>());
    }
    for field in ["closingIssuesReferences", "trackingIssuesReferences"] {
        if let Some(issues) = input[field]["nodes"].as_array_mut() {
            for issue in issues.iter_mut().take(5) {
                if let Some(body) = issue["body"].as_str() {
                    issue["body"] = json!(body.chars().take(3_000).collect::<String>());
                }
            }
        }
    }
    input
}

fn canonical_input_hash(input: &Value) -> String {
    let input = json!({
        "scoring_version": SCORING_VERSION,
        "input": input,
    });
    hex::encode(Sha256::digest(input.to_string().as_bytes()))
}

pub fn importance_input_hash(pr: &Value) -> String {
    canonical_input_hash(&importance_input(pr))
}

fn factor_spec(key: &str) -> &'static FactorSpec {
    FACTOR_SPECS
        .iter()
        .find(|spec| spec.key == key)
        .unwrap_or_else(|| panic!("unknown importance factor: {key}"))
}

fn factor(key: &'static str, points: i32, evidence: impl Into<String>) -> ImportanceFactor {
    let spec = factor_spec(key);
    assert!(
        (spec.min_points..=spec.max_points).contains(&points),
        "importance factor {key} points {points} outside {}..={}",
        spec.min_points,
        spec.max_points
    );
    ImportanceFactor {
        group: spec.group,
        key: spec.key,
        points,
        max_points: spec.display_max_points(),
        evidence: evidence.into(),
    }
}

pub fn finalize_score(
    factors: Vec<ImportanceFactor>,
    method: impl Into<String>,
    input_hash: impl Into<String>,
) -> ImportanceScore {
    assert_eq!(
        factors.len(),
        FACTOR_SPECS.len(),
        "importance score must contain every factor"
    );
    for (factor, spec) in factors.iter().zip(FACTOR_SPECS) {
        assert_eq!(factor.key, spec.key, "importance factors are out of order");
        assert_eq!(factor.group, spec.group, "importance factor group mismatch");
        assert_eq!(
            factor.max_points,
            spec.display_max_points(),
            "importance factor limit mismatch"
        );
        assert!(
            (spec.min_points..=spec.max_points).contains(&factor.points),
            "importance factor {} points {} outside {}..={}",
            factor.key,
            factor.points,
            spec.min_points,
            spec.max_points
        );
    }
    let raw_score = factors
        .iter()
        .map(|factor| factor.points)
        .sum::<i32>()
        .clamp(MIN_RAW_SCORE, MAX_RAW_SCORE) as u32;
    ImportanceScore {
        score: raw_score as f64 / SCORE_SCALE,
        raw_score,
        scoring_version: SCORING_VERSION,
        factors,
        method: method.into(),
        input_hash: input_hash.into(),
    }
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
            factor_spec("severity").max_points as u32,
            &[
                "security",
                "vulnerability",
                "data-loss",
                "data loss",
                "wrong-output",
                "wrong output",
            ],
            &["security", "vulnerability"],
            &[
                "data loss",
                "remote code execution",
                "wrong output",
                "incorrect output",
                "数据丢失",
                "安全漏洞",
                "输出错误",
            ],
        ),
        (
            40,
            &["regression", "crash"],
            &["regression", "crash", "crashes"],
            &["回归", "崩溃"],
        ),
        (
            20,
            &["bug", "bugfix", "defect"],
            &["bug", "bugfix", "fix", "fixes", "fixed"],
            &["修复"],
        ),
        (
            15,
            &["performance", "perf", "latency", "memory"],
            &["performance", "perf", "latency"],
            &["性能", "延迟"],
        ),
        (
            8,
            &["enhancement", "feature", "feature-request"],
            &["feature", "enhancement", "support", "add"],
            &["功能", "支持"],
        ),
        (
            2,
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
    let mut best = severity_candidate(pr["title"].as_str().unwrap_or(""), &label_names(pr), "PR");
    for issue in issues {
        let source = format!("issue #{}", issue["number"].as_i64().unwrap_or(0));
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
    factor("severity", candidate.points as i32, candidate.evidence)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileKind {
    Production,
    Test,
    Other,
}

fn file_kind(path: &str) -> FileKind {
    let path = path.to_ascii_lowercase();
    let base = path.rsplit('/').next().unwrap_or(&path);
    if base.ends_with(".lock")
        || matches!(base, "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml")
        || base.ends_with(".min.js")
        || base.ends_with(".min.css")
        || base.ends_with(".pb.go")
        || base.ends_with(".snap")
        || base.contains(".generated.")
        || path.starts_with("dist/")
        || path.contains("/dist/")
    {
        return FileKind::Other;
    }
    let in_test_dir = path.starts_with("tests/")
        || path.starts_with("test/")
        || path.contains("/tests/")
        || path.contains("/test/");
    let test_file = base.starts_with("test_")
        || base.ends_with("_test.rs")
        || base.ends_with("_test.go")
        || base.ends_with("_test.py")
        || base.ends_with(".test.js")
        || base.ends_with(".test.ts")
        || base.ends_with(".spec.js")
        || base.ends_with(".spec.ts");
    if in_test_dir || test_file {
        return FileKind::Test;
    }
    if path.starts_with("docs/")
        || path.contains("/docs/")
        || base.ends_with(".md")
        || base.ends_with(".rst")
        || base == "license"
    {
        return FileKind::Other;
    }
    FileKind::Production
}

fn production_area(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > 2
        && matches!(
            parts[0],
            "src" | "vllm" | "crates" | "packages" | "pkg" | "libs"
        )
    {
        format!("{}/{}", parts[0], parts[1])
    } else {
        parts[0].to_string()
    }
}

#[derive(Default)]
struct FileSummary {
    files_present: bool,
    size_present: bool,
    production_files: usize,
    test_files: usize,
    areas: BTreeSet<String>,
    changed_files: u64,
    lines: u64,
}

fn file_summary(pr: &Value) -> FileSummary {
    let mut summary = FileSummary {
        files_present: pr["files"]["nodes"].as_array().is_some(),
        size_present: pr["changedFiles"].as_u64().is_some()
            || pr["additions"].as_u64().is_some()
            || pr["deletions"].as_u64().is_some(),
        changed_files: pr["changedFiles"].as_u64().unwrap_or(0),
        lines: pr["additions"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(pr["deletions"].as_u64().unwrap_or(0)),
        ..Default::default()
    };
    if let Some(files) = pr["files"]["nodes"].as_array() {
        if !summary.size_present {
            summary.changed_files = files.len() as u64;
        }
        for path in files.iter().filter_map(|file| file["path"].as_str()) {
            match file_kind(path) {
                FileKind::Production => {
                    summary.production_files += 1;
                    summary.areas.insert(production_area(path));
                }
                FileKind::Test => summary.test_files += 1,
                FileKind::Other => {}
            }
        }
    }
    summary
}

fn surface_factor(summary: &FileSummary) -> ImportanceFactor {
    if !summary.files_present {
        return factor("production_surface", 0, "File details unavailable");
    }
    if summary.production_files == 0 {
        return factor("production_surface", 2, "No production code changed");
    }
    let points = match summary.areas.len() {
        0 | 1 => 4,
        2 => 8,
        3 => 12,
        _ => 15,
    };
    let areas = summary
        .areas
        .iter()
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    factor(
        "production_surface",
        points,
        format!(
            "{} production files across {} areas: {areas}",
            summary.production_files,
            summary.areas.len()
        ),
    )
}

fn evidence_text(pr: &Value) -> String {
    let mut parts = vec![
        pr["title"].as_str().unwrap_or(""),
        pr["body"].as_str().unwrap_or(""),
    ];
    for group in ["closingIssuesReferences", "trackingIssuesReferences"] {
        if let Some(issues) = pr[group]["nodes"].as_array() {
            for issue in issues {
                parts.push(issue["title"].as_str().unwrap_or(""));
                parts.push(issue["body"].as_str().unwrap_or(""));
            }
        }
    }
    if let Some(files) = pr["files"]["nodes"].as_array() {
        for file in files {
            parts.push(file["path"].as_str().unwrap_or(""));
        }
    }
    parts.join("\n").to_lowercase()
}

fn contains_any<'a>(text: &str, terms: &'a [&'a str]) -> Option<&'a str> {
    terms.iter().find_map(|term| {
        if term.len() > 3 {
            return text.contains(term).then_some(*term);
        }
        text.match_indices(term).find_map(|(start, _)| {
            let previous = start.checked_sub(1).and_then(|i| text.as_bytes().get(i));
            let next = text.as_bytes().get(start + term.len());
            let bounded = previous.is_none_or(|byte| !byte.is_ascii_alphanumeric())
                && next.is_none_or(|byte| !byte.is_ascii_alphanumeric());
            bounded.then_some(*term)
        })
    })
}

fn model_popularity_factor(pr: &Value) -> ImportanceFactor {
    let text = evidence_text(pr);
    let mainstream = contains_any(
        &text,
        &["deepseek", "qwen", "llama", "kimi", "gemma", "gpt"],
    );
    let common = contains_any(
        &text,
        &[
            "phi", "granite", "nemotron", "internvl", "mistral", "gemini", "claude",
        ],
    );
    let customized = contains_any(
        &text,
        &["fine-tune", "finetune", "custom model", "rare model"],
    );
    let (points, evidence) = if customized.is_some() && (mainstream.is_some() || common.is_some()) {
        (1, "Custom or fine-tuned model variant".to_string())
    } else if let Some(model) = mainstream {
        (6, format!("Mainstream model evidence: {model}"))
    } else if let Some(model) = common {
        (3, format!("Established model evidence: {model}"))
    } else if customized.is_some() {
        (1, "Custom or fine-tuned model variant".to_string())
    } else {
        (0, "No named model evidence".to_string())
    };
    factor("model_popularity", points, evidence)
}

fn gpu_tier_factor(pr: &Value) -> ImportanceFactor {
    let text = evidence_text(pr);
    let high = contains_any(
        &text,
        &[
            "h100", "h200", "b200", "b300", "gb200", "gb300", "mi300x", "mi355x",
        ],
    );
    let common = contains_any(
        &text,
        &[
            "a100", "mi325x", "tpu", "l40s", "cpu_x86", "cpu_arm", "gaudi", "xpu",
        ],
    );
    let other = contains_any(
        &text,
        &[
            "rtx", "geforce", "gtx", "v100", "t4", "p100", "4090", "3090", "5090", "a5000", "a6000",
        ],
    );
    let (points, evidence) = if let Some(gpu) = high {
        (5, format!("High-tier GPU evidence: {gpu}"))
    } else if let Some(gpu) = common {
        (3, format!("Common datacenter hardware evidence: {gpu}"))
    } else if let Some(gpu) = other {
        (1, format!("Consumer or older hardware evidence: {gpu}"))
    } else {
        (0, "No named GPU or hardware evidence".to_string())
    };
    factor("gpu_tier", points, evidence)
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
    let release_labels = [
        "release-blocker",
        "release blocker",
        "blocker",
        "release-critical",
    ];
    if let Some(label) = label_matches(&label_names(pr), &release_labels) {
        consider(Candidate {
            points: 10,
            evidence: format!("PR label: {label}"),
        });
    }
    for issue in issues {
        if let Some(label) = label_matches(&label_names(issue), &release_labels) {
            consider(Candidate {
                points: 10,
                evidence: format!(
                    "issue #{} label: {label}",
                    issue["number"].as_i64().unwrap_or(0)
                ),
            });
        }
    }
    if let Some(milestone) = pr["milestone"]["title"]
        .as_str()
        .filter(|title| !title.is_empty())
    {
        consider(Candidate {
            points: 8,
            evidence: format!("PR milestone: {milestone}"),
        });
    }
    for issue in issues {
        if let Some(milestone) = issue["milestone"]["title"]
            .as_str()
            .filter(|title| !title.is_empty())
        {
            consider(Candidate {
                points: 8,
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
            points: 6,
            evidence: format!("PR targets release branch: {base}"),
        });
    } else if matches!(base, "main" | "master") {
        consider(Candidate {
            points: 4,
            evidence: format!("PR targets {base} (nightly development)"),
        });
    }
    let candidate = best.unwrap_or(Candidate {
        points: 0,
        evidence: "No release signal".into(),
    });
    factor(
        "release_urgency",
        candidate.points as i32,
        candidate.evidence,
    )
}

fn issue_evidence_factor(issues: &[Value], reproduction: bool) -> ImportanceFactor {
    let (key, needles): (&str, &[&str]) = if reproduction {
        (
            "reproduction",
            &[
                "steps to reproduce",
                "reproduction steps",
                "minimal repro",
                "reproducer",
                "reproduction",
                "复现",
            ],
        )
    } else {
        (
            "diagnostics",
            &[
                "expected behavior",
                "actual behavior",
                "expected result",
                "actual result",
                "expected:",
                "actual:",
                "traceback",
                "stack trace",
                "error log",
                "logs",
                "预期",
                "实际",
                "日志",
            ],
        )
    };
    for issue in issues {
        let body = issue["body"].as_str().unwrap_or("").to_ascii_lowercase();
        if let Some(needle) = needles.iter().find(|needle| body.contains(**needle)) {
            return factor(
                key,
                factor_spec(key).max_points,
                format!(
                    "issue #{} contains {needle}",
                    issue["number"].as_i64().unwrap_or(0)
                ),
            );
        }
    }
    let evidence = if issues.is_empty() {
        "No linked issue evidence".to_string()
    } else if reproduction {
        "No reproduction steps found in linked issues".to_string()
    } else {
        "No expected/actual behavior or diagnostic logs found".to_string()
    };
    factor(key, 0, evidence)
}

fn comments_factor(issues: &[Value]) -> ImportanceFactor {
    let best = issues
        .iter()
        .max_by_key(|issue| issue["comments"]["totalCount"].as_u64().unwrap_or(0));
    let Some(issue) = best else {
        return factor("comments", 0, "No linked issue");
    };
    let comments = issue["comments"]["totalCount"].as_u64().unwrap_or(0);
    let points = match comments {
        0 => 0,
        1..=2 => 2,
        3..=5 => 4,
        _ => 6,
    };
    factor(
        "comments",
        points,
        format!(
            "issue #{}: {comments} comments",
            issue["number"].as_i64().unwrap_or(0)
        ),
    )
}

fn reactions_factor(issues: &[Value]) -> ImportanceFactor {
    let best = issues
        .iter()
        .max_by_key(|issue| issue["reactions"]["totalCount"].as_u64().unwrap_or(0));
    let Some(issue) = best else {
        return factor("reactions", 0, "No linked issue");
    };
    let reactions = issue["reactions"]["totalCount"].as_u64().unwrap_or(0);
    let points = match reactions {
        0 => 0,
        1..=4 => 1,
        5..=9 => 2,
        10..=19 => 3,
        _ => 4,
    };
    factor(
        "reactions",
        points,
        format!(
            "issue #{}: {reactions} reactions",
            issue["number"].as_i64().unwrap_or(0)
        ),
    )
}

fn linked_issue_factor(issues: &[Value], tracking_issues: &[Value]) -> ImportanceFactor {
    let numbers = issues
        .iter()
        .filter_map(|issue| issue["number"].as_i64())
        .take(3)
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>()
        .join(", ");
    let tracking = tracking_issues
        .iter()
        .map(|issue| {
            format!(
                "#{} {}",
                issue["number"].as_i64().unwrap_or(0),
                issue["title"].as_str().unwrap_or("")
            )
        })
        .take(2)
        .collect::<Vec<_>>()
        .join("; ");
    let has_context = !issues.is_empty() || !tracking_issues.is_empty();
    let evidence = if !issues.is_empty() {
        format!("Closes {numbers}; link counted once without inheriting the issue score")
    } else if !tracking_issues.is_empty() {
        format!("Roadmap context: {tracking}; counted once without a closing-link bonus")
    } else {
        "No closing issue or roadmap reference".to_string()
    };
    factor("linked_issue", if has_context { 10 } else { 0 }, evidence)
}

fn tests_factor(summary: &FileSummary) -> ImportanceFactor {
    factor(
        "includes_tests",
        if summary.test_files > 0 { 5 } else { 0 },
        if summary.test_files > 0 {
            format!(
                "Includes {} test files (validation maturity)",
                summary.test_files
            )
        } else {
            "No test files in the fetched file list".to_string()
        },
    )
}

fn focused_factor(summary: &FileSummary) -> ImportanceFactor {
    let points = if !summary.size_present {
        0
    } else if summary.lines <= 50 && summary.changed_files <= 5 {
        5
    } else if summary.lines <= 200 && summary.changed_files <= 10 {
        3
    } else {
        0
    };
    factor(
        "focused_change",
        points,
        if summary.size_present {
            format!(
                "{} changed files, +/− {} lines total",
                summary.changed_files, summary.lines
            )
        } else {
            "Change size unavailable".to_string()
        },
    )
}

fn production_factor(summary: &FileSummary) -> ImportanceFactor {
    factor(
        "production_change",
        if summary.production_files > 0 { 5 } else { 0 },
        if summary.production_files > 0 {
            format!("Touches {} production files", summary.production_files)
        } else {
            "No production code change in the fetched file list".to_string()
        },
    )
}

fn penalties_factor(pr: &Value, summary: &FileSummary) -> ImportanceFactor {
    let large = if summary.size_present && (summary.lines >= 1000 || summary.changed_files >= 30) {
        10
    } else if summary.size_present && (summary.lines >= 500 || summary.changed_files >= 20) {
        5
    } else {
        0
    };
    let draft = if pr["isDraft"].as_bool() == Some(true) {
        5
    } else {
        0
    };
    let mut reasons = Vec::new();
    if large > 0 {
        reasons.push(format!(
            "large change: {} files, {} lines (-{large})",
            summary.changed_files, summary.lines
        ));
    }
    if draft > 0 {
        reasons.push("draft PR (-5)".to_string());
    }
    factor(
        "change_penalties",
        -((large + draft) as i32),
        if reasons.is_empty() {
            "No large-change or draft penalty".to_string()
        } else {
            reasons.join("; ")
        },
    )
}

fn score_canonical_input(input: &Value) -> ImportanceScore {
    let issues = input["closingIssuesReferences"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let tracking_issues = input["trackingIssuesReferences"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let files = file_summary(input);
    let factors = vec![
        severity_factor(input, &issues),
        surface_factor(&files),
        model_popularity_factor(input),
        gpu_tier_factor(input),
        release_factor(input, &issues),
        issue_evidence_factor(&issues, true),
        issue_evidence_factor(&issues, false),
        comments_factor(&issues),
        reactions_factor(&issues),
        linked_issue_factor(&issues, &tracking_issues),
        tests_factor(&files),
        focused_factor(&files),
        production_factor(&files),
        penalties_factor(input, &files),
    ];
    finalize_score(factors, "rules", canonical_input_hash(input))
}

pub fn score_pr(pr: &Value) -> ImportanceScore {
    ImportanceCandidate::from_pr(pr).score
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
            "body": "",
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
            "body": "",
            "labels": {"nodes": labels},
            "baseRefName": "main",
            "milestone": null,
            "changedFiles": 0,
            "additions": 0,
            "deletions": 0,
            "isDraft": false,
            "files": {"nodes": []},
            "closingIssuesReferences": {"nodes": issues}
        })
    }

    fn set_files(input: &mut Value, paths: &[&str]) {
        input["files"]["nodes"] = paths.iter().map(|path| json!({"path": path})).collect();
        input["changedFiles"] = json!(paths.len());
    }

    fn points(score: &ImportanceScore, key: &str) -> i32 {
        score
            .factors
            .iter()
            .find(|factor| factor.key == key)
            .map(|factor| factor.points)
            .unwrap()
    }

    #[test]
    fn factor_specs_match_rule_metadata_and_order() {
        let expected = vec![
            ("how_bad", "severity", 0, 50),
            ("who_it_hits", "production_surface", 0, 15),
            ("who_it_hits", "model_popularity", 0, 6),
            ("who_it_hits", "gpu_tier", 0, 5),
            ("release", "release_urgency", 0, 10),
            ("evidence", "reproduction", 0, 6),
            ("evidence", "diagnostics", 0, 4),
            ("community", "comments", 0, 6),
            ("community", "reactions", 0, 4),
            ("pr_signals", "linked_issue", 0, 10),
            ("pr_signals", "includes_tests", 0, 5),
            ("pr_signals", "focused_change", 0, 5),
            ("pr_signals", "production_change", 0, 5),
            ("penalty", "change_penalties", -15, 0),
        ];
        let actual = FACTOR_SPECS
            .iter()
            .map(|spec| (spec.group, spec.key, spec.min_points, spec.max_points))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);

        let score = score_pr(&json!({}));
        assert_eq!(score.factors.len(), FACTOR_SPECS.len());
        for (factor, spec) in score.factors.iter().zip(FACTOR_SPECS) {
            assert_eq!(factor.group, spec.group);
            assert_eq!(factor.key, spec.key);
            assert_eq!(factor.max_points, spec.display_max_points());
        }
    }

    #[test]
    fn finalize_score_clamps_only_after_summing_all_factors() {
        fn at_limits(maximum: bool) -> Vec<ImportanceFactor> {
            FACTOR_SPECS
                .iter()
                .map(|spec| {
                    factor(
                        spec.key,
                        if maximum {
                            spec.max_points
                        } else {
                            spec.min_points
                        },
                        "boundary",
                    )
                })
                .collect()
        }

        let maximum = finalize_score(at_limits(true), "test", "maximum");
        assert_eq!(
            maximum
                .factors
                .iter()
                .map(|factor| factor.points)
                .sum::<i32>(),
            131
        );
        assert_eq!(maximum.raw_score, MAX_RAW_SCORE as u32);
        assert_eq!(maximum.score, MAX_RAW_SCORE as f64 / SCORE_SCALE);
        assert_eq!(maximum.method, "test");
        assert_eq!(maximum.input_hash, "maximum");

        let minimum = finalize_score(at_limits(false), "test", "minimum");
        assert_eq!(
            minimum
                .factors
                .iter()
                .map(|factor| factor.points)
                .sum::<i32>(),
            -15
        );
        assert_eq!(minimum.raw_score, MIN_RAW_SCORE as u32);
        assert_eq!(minimum.score, MIN_RAW_SCORE as f64 / SCORE_SCALE);
    }

    #[test]
    fn candidate_uses_one_canonical_input_at_clip_boundaries() {
        let pr_body = "界".repeat(6_001);
        let issue_body = "🦀".repeat(3_001);
        let mut closing = issue(1, "Bug", &[], 0, 0);
        closing["body"] = json!(issue_body);
        let mut tracking = issue(2, "Roadmap", &[], 0, 0);
        tracking["body"] = json!(issue_body);
        let mut input = pr("Fix bug", &[], vec![closing]);
        input["body"] = json!(pr_body);
        input["trackingIssuesReferences"] = json!({"nodes": [tracking]});

        let candidate = ImportanceCandidate::from_pr(&input);
        let expected_pr_body = "界".repeat(6_000);
        let expected_issue_body = "🦀".repeat(3_000);
        assert_eq!(
            candidate.input["body"].as_str(),
            Some(expected_pr_body.as_str())
        );
        for field in ["closingIssuesReferences", "trackingIssuesReferences"] {
            assert_eq!(
                candidate.input[field]["nodes"][0]["body"].as_str(),
                Some(expected_issue_body.as_str())
            );
        }
        assert_eq!(
            candidate.score.input_hash,
            canonical_input_hash(&candidate.input)
        );
        assert_eq!(candidate.score.input_hash, importance_input_hash(&input));
        assert_eq!(candidate.score, score_pr(&candidate.input));

        let exact_body = "a".repeat(6_000);
        let mut exact = pr("Unclassified", &[], vec![]);
        exact["body"] = json!(exact_body.clone());
        assert_eq!(
            ImportanceCandidate::from_pr(&exact).input["body"].as_str(),
            Some(exact_body.as_str())
        );
    }

    #[test]
    fn severity_uses_strongest_evidence_without_double_counting() {
        let score = score_pr(&pr(
            "Fix incorrect results",
            &["bug"],
            vec![issue(7, "Incorrect results", &["wrong-output"], 0, 0)],
        ));
        assert_eq!(points(&score, "severity"), 50);
        assert_eq!(score.factors[0].max_points, 50);
        assert!(score.factors[0].evidence.contains("wrong-output"));
    }

    #[test]
    fn title_matching_does_not_treat_prefix_as_fix() {
        let score = score_pr(&pr("Add prefix support", &[], vec![]));
        assert_eq!(points(&score, "severity"), 8);
    }

    #[test]
    fn surface_counts_production_areas_not_tests_or_docs() {
        let mut input = pr("Feature", &[], vec![]);
        set_files(
            &mut input,
            &[
                "src/api/server.rs",
                "src/core/engine.rs",
                "vllm/renderers/online.py",
                "tests/api/test_server.py",
                "docs/guide.md",
            ],
        );
        let score = score_pr(&input);
        assert_eq!(points(&score, "production_surface"), 12);
        assert_eq!(points(&score, "includes_tests"), 5);
        assert_eq!(points(&score, "production_change"), 5);
    }

    #[test]
    fn release_uses_strongest_signal_and_main_has_a_baseline() {
        let mut input = pr("Feature", &[], vec![issue(1, "Feature", &[], 0, 0)]);
        assert_eq!(points(&score_pr(&input), "release_urgency"), 4);
        input["milestone"] = json!({"title": "v1.0"});
        assert_eq!(points(&score_pr(&input), "release_urgency"), 8);
        input["baseRefName"] = json!("release/v1");
        assert_eq!(points(&score_pr(&input), "release_urgency"), 8);
        input["closingIssuesReferences"]["nodes"][0]["labels"]["nodes"] =
            json!([{"name": "release-blocker"}]);
        assert_eq!(points(&score_pr(&input), "release_urgency"), 10);
    }

    #[test]
    fn issue_evidence_has_independent_reproduction_and_diagnostic_factors() {
        let mut linked = issue(7, "Bug", &[], 0, 0);
        linked["body"] = json!("Steps to reproduce:\n1. run server\n**Expected:** success");
        let score = score_pr(&pr("Fix bug", &[], vec![linked]));
        assert_eq!(points(&score, "reproduction"), 6);
        assert_eq!(points(&score, "diagnostics"), 4);
    }

    #[test]
    fn community_uses_separate_comment_and_reaction_tiers() {
        let score = score_pr(&pr(
            "Feature",
            &[],
            vec![
                issue(1, "Feature request", &[], 2, 20),
                issue(2, "Popular request", &[], 40, 1),
            ],
        ));
        assert_eq!(points(&score, "comments"), 6);
        assert_eq!(points(&score, "reactions"), 4);
    }

    #[test]
    fn roadmap_context_counts_once_without_a_closing_link_bonus() {
        let mut input = pr("Feature", &[], vec![]);
        input["trackingIssuesReferences"] = json!({"nodes": [
            issue(68784, "[Tracking] Rust frontend as default", &[], 7, 2)
        ]});
        let score = score_pr(&input);
        assert_eq!(points(&score, "linked_issue"), 10);
        assert!(score.factors.iter().any(|factor| {
            factor.key == "linked_issue" && factor.evidence.contains("Roadmap context")
        }));
        input["closingIssuesReferences"]["nodes"] = json!([issue(1, "Closing bug", &[], 0, 0)]);
        assert_eq!(points(&score_pr(&input), "linked_issue"), 10);
    }

    #[test]
    fn pr_signals_cover_link_tests_focus_and_production_code() {
        let mut input = pr("Fix bug", &[], vec![issue(1, "Bug", &[], 0, 0)]);
        set_files(
            &mut input,
            &["src/api/server.rs", "tests/api/test_server.py"],
        );
        input["additions"] = json!(30);
        input["deletions"] = json!(10);
        let score = score_pr(&input);
        assert_eq!(points(&score, "linked_issue"), 10);
        assert_eq!(points(&score, "includes_tests"), 5);
        assert_eq!(points(&score, "focused_change"), 5);
        assert_eq!(points(&score, "production_change"), 5);
        assert_eq!(points(&score, "change_penalties"), 0);
    }

    #[test]
    fn large_and_draft_penalties_accumulate_and_total_is_clamped() {
        let mut input = pr("Unclassified", &[], vec![]);
        set_files(&mut input, &["src/api/server.rs"]);
        input["changedFiles"] = json!(30);
        input["additions"] = json!(900);
        input["deletions"] = json!(200);
        input["isDraft"] = json!(true);
        let score = score_pr(&input);
        assert_eq!(points(&score, "focused_change"), 0);
        assert_eq!(points(&score, "change_penalties"), -15);
        assert_eq!(score.factors.last().unwrap().max_points, 15);
        assert!(score.raw_score <= 100);

        let mut negative = pr("Unclassified", &[], vec![]);
        negative["baseRefName"] = json!("feature-branch");
        negative["changedFiles"] = json!(30);
        negative["additions"] = json!(1100);
        negative["isDraft"] = json!(true);
        assert_eq!(score_pr(&negative).score, 0.0);
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
    fn model_and_gpu_evidence_have_bounded_independent_weights() {
        let mainstream = score_pr(&pr("Fix Qwen3 crash on H100", &[], vec![]));
        assert_eq!(points(&mainstream, "model_popularity"), 6);
        assert_eq!(points(&mainstream, "gpu_tier"), 5);

        let custom = score_pr(&pr("Fix Qwen fine-tune on RTX 4090", &[], vec![]));
        assert_eq!(points(&custom, "model_popularity"), 1);
        assert_eq!(points(&custom, "gpu_tier"), 1);

        let unknown = score_pr(&pr("Fix incorrect output", &[], vec![]));
        assert_eq!(points(&unknown, "model_popularity"), 0);
        assert_eq!(points(&unknown, "gpu_tier"), 0);
    }

    #[test]
    fn model_and_gpu_evidence_can_come_from_changed_files() {
        let mut input = pr("Fix vision encoder", &[], vec![]);
        set_files(&mut input, &["vllm/model_executor/models/qwen3.py"]);
        input["body"] = json!("Fails on A100 with default settings");
        let score = score_pr(&input);
        assert_eq!(points(&score, "model_popularity"), 6);
        assert_eq!(points(&score, "gpu_tier"), 3);
    }

    #[test]
    fn empty_input_is_a_scored_zero_with_versioned_explanations() {
        let score = score_pr(&json!({}));
        assert_eq!(score.score, 0.0);
        assert_eq!(score.raw_score, 0);
        assert_eq!(score.scoring_version, SCORING_VERSION);
        assert_eq!(score.factors.len(), 14);
        assert!(score.factors.iter().all(|factor| factor.points == 0));
        assert!(score.factors.iter().any(|factor| factor.group == "penalty"));
    }
}
