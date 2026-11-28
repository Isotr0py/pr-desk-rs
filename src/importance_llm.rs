//! 使用 LLM 校准规则重要性因子；规则提供上下界、证据与失败回退。

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::importance::{ImportanceCandidate, ImportanceScore};
use crate::reviewer::LlmSettings;

const FACTOR_LIMITS: &[(&str, i32, i32)] = &[
    ("severity", 0, 50),
    ("production_surface", 0, 15),
    ("model_popularity", 0, 6),
    ("gpu_tier", 0, 5),
    ("release_urgency", 0, 10),
    ("reproduction", 0, 6),
    ("diagnostics", 0, 4),
    ("comments", 0, 6),
    ("reactions", 0, 4),
    ("linked_issue", 0, 10),
    ("includes_tests", 0, 5),
    ("focused_change", 0, 5),
    ("production_change", 0, 5),
    ("change_penalties", -15, 0),
];

const SYSTEM_PROMPT: &str = "你是资深开源仓库维护者，负责校准 PR 的重要性。\
你只能使用输入中的 GitHub 证据和规则因子，不得虚构模型热度、用户规模、硬件影响或业务背景。\
你的职责是纠正机械规则的误判，而不是机械照抄基线。区分仓库影响与代码质量，不评价实现好坏。\
trackingIssuesReferences 是 roadmap/tracking 上下文而非关闭关联，应评估其战略影响；不得重复计关闭奖励。\
authorAssociation 仅用于判断证据可信度，作者身份本身不得直接加分。\
所有分值必须为整数且位于给定范围，惩罚只能为负或零；无证据的因子保持 0。\
只输出严格 JSON，不要 Markdown 或额外解释。";

fn build_prompt(candidate: &ImportanceCandidate) -> String {
    let input = serde_json::to_string(&candidate.input).unwrap_or_else(|_| "{}".into());
    let baseline = serde_json::to_string(&candidate.score.factors).unwrap_or_else(|_| "[]".into());
    format!(
        "请校准这个 PR 的重要性因子。规则版本：{}。\n\n\
         GitHub 结构化输入（正文已裁剪）：\n{input}\n\n\
         规则基线因子（含上下限和证据）：\n{baseline}\n\n\
         输出 JSON 格式：\n\
         {{\"factors\":[{{\"key\":\"severity\",\"points\":20,\"rationale\":\"中文理由\"}}]}}\n\
         必须且只能包含以下 14 个 key，每个恰好一次：\n\
         severity(0..50), production_surface(0..15), model_popularity(0..6), gpu_tier(0..5), \
         release_urgency(0..10), reproduction(0..6), diagnostics(0..4), comments(0..6), \
         reactions(0..4), linked_issue(0..10), includes_tests(0..5), focused_change(0..5), \
         production_change(0..5), change_penalties(-15..0)。\n\
         每项 rationale 必须是有证据的中文短句；没有证据时明确写无证据。",
        candidate.score.scoring_version
    )
}

fn extract_json(text: &str) -> Option<Value> {
    let text = text.trim();
    let without_fence = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text);
    let without_fence = without_fence.strip_suffix("```").unwrap_or(without_fence);
    let start = without_fence.find('{')?;
    let end = without_fence.rfind('}')?;
    (end > start).then(|| serde_json::from_str(&without_fence[start..=end]).ok())??
}

fn parse_response(
    candidate: &ImportanceCandidate,
    text: &str,
    model: &str,
) -> Option<ImportanceScore> {
    let data = extract_json(text)?;
    let entries = data["factors"].as_array()?;
    let mut assessed = BTreeMap::new();
    for entry in entries {
        let key = entry["key"].as_str()?.to_string();
        let points = entry["points"].as_i64()?;
        let rationale = entry["rationale"].as_str()?.trim().to_string();
        if rationale.is_empty() || assessed.insert(key, (points, rationale)).is_some() {
            return None;
        }
    }
    if assessed.len() != FACTOR_LIMITS.len() {
        return None;
    }

    let mut score = candidate.score.clone();
    for (key, min, max) in FACTOR_LIMITS {
        let (points, rationale) = assessed.get(*key)?;
        let points = i32::try_from(*points).ok()?;
        if !(*min..=*max).contains(&points) {
            return None;
        }
        let factor = score.factors.iter_mut().find(|factor| factor.key == *key)?;
        factor.points = points;
        factor.evidence = format!("{rationale} | Rule evidence: {}", factor.evidence);
    }
    score.raw_score = score
        .factors
        .iter()
        .map(|factor| factor.points)
        .sum::<i32>()
        .clamp(0, 100) as u32;
    score.score = score.raw_score as f64 / 10.0;
    score.method = format!("llm:{model}");
    Some(score)
}

pub async fn refine_importance(
    llm: &LlmSettings,
    candidate: &ImportanceCandidate,
) -> Result<ImportanceScore> {
    let prompt = build_prompt(candidate);
    let mut last_raw = String::new();
    for attempt in 0..2 {
        let user = if attempt == 0 {
            prompt.clone()
        } else {
            format!(
                "{prompt}\n\n上一轮回复无法通过校验，请只返回满足全部约束的 JSON：\n{}",
                last_raw.chars().take(2_000).collect::<String>()
            )
        };
        let raw = llm
            .complete_text(SYSTEM_PROMPT, &user)
            .await
            .context("LLM 重要性校准请求失败")?;
        last_raw = raw;
        if let Some(score) = parse_response(candidate, &last_raw, &llm.model) {
            return Ok(score);
        }
    }
    anyhow::bail!("LLM 重要性校准结果缺少有效 14 因子")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate() -> ImportanceCandidate {
        let pr = json!({
            "repo": "o/r",
            "number": 42,
            "title": "Fix regression",
            "body": "Fixes #7",
            "labels": {"nodes": [{"name": "bug"}]},
            "baseRefName": "main",
            "changedFiles": 1,
            "additions": 10,
            "deletions": 2,
            "files": {"nodes": [{"path": "src/api/server.rs"}]},
            "closingIssuesReferences": {"nodes": [{
                "number": 7,
                "title": "Wrong output",
                "body": "Steps to reproduce\nExpected: ok",
                "labels": {"nodes": []},
                "comments": {"totalCount": 3},
                "reactions": {"totalCount": 1}
            }]}
        });
        ImportanceCandidate {
            score: crate::importance::score_pr(&pr),
            input: crate::importance::importance_input(&pr),
        }
    }

    fn valid_response() -> String {
        let factors: Vec<Value> = FACTOR_LIMITS
            .iter()
            .map(|(key, min, max)| {
                json!({
                    "key": key,
                    "points": if *key == "change_penalties" { -5 } else { (min + max) / 2 },
                    "rationale": format!("{key} 的证据理由")
                })
            })
            .collect();
        json!({"factors": factors}).to_string()
    }

    #[test]
    fn parses_calibrated_factors_and_recomputes_total() {
        let candidate = candidate();
        let score = parse_response(&candidate, &valid_response(), "k3-test").unwrap();
        assert_eq!(score.method, "llm:k3-test");
        assert_eq!(score.factors.len(), 14);
        assert_eq!(score.raw_score, 58);
        assert_eq!(score.score, 5.8);
        assert!(score.factors[0].evidence.starts_with("severity 的证据理由"));
        assert!(score.factors[0].evidence.contains("Rule evidence:"));
        assert_eq!(score.input_hash, candidate.score.input_hash);
    }

    #[test]
    fn rejects_missing_duplicate_and_out_of_range_factors() {
        let candidate = candidate();
        let mut data: Value = serde_json::from_str(&valid_response()).unwrap();
        data["factors"].as_array_mut().unwrap().remove(0);
        assert!(parse_response(&candidate, &data.to_string(), "k3").is_none());

        let mut data: Value = serde_json::from_str(&valid_response()).unwrap();
        let duplicate = data["factors"][0].clone();
        data["factors"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_response(&candidate, &data.to_string(), "k3").is_none());

        let mut data: Value = serde_json::from_str(&valid_response()).unwrap();
        data["factors"][0]["points"] = json!(51);
        assert!(parse_response(&candidate, &data.to_string(), "k3").is_none());

        let mut data: Value = serde_json::from_str(&valid_response()).unwrap();
        data["factors"][13]["points"] = json!(1);
        assert!(parse_response(&candidate, &data.to_string(), "k3").is_none());
    }

    #[test]
    fn prompt_contains_capped_input_and_all_factor_limits() {
        let prompt = build_prompt(&candidate());
        assert!(prompt.contains("o/r"));
        assert!(prompt.contains("severity(0..50)"));
        assert!(prompt.contains("model_popularity(0..6)"));
        assert!(prompt.contains("gpu_tier(0..5)"));
        assert!(prompt.contains("change_penalties(-15..0)"));
        assert!(prompt.contains("规则基线因子"));
        assert!(!prompt.contains("overall_score"));
    }

    #[test]
    fn input_hash_tracks_relevant_content_changes() {
        let candidate = candidate();
        let mut changed = candidate.input.clone();
        changed["title"] = json!("Different title");
        assert_ne!(
            crate::importance::importance_input_hash(&candidate.input),
            crate::importance::importance_input_hash(&changed)
        );
        assert_eq!(
            crate::importance::importance_input_hash(&candidate.input),
            candidate.score.input_hash
        );
    }
}
