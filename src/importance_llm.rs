//! 使用 LLM 校准规则重要性因子；规则提供上下界、证据与失败回退。

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::importance::{FACTOR_SPECS, ImportanceCandidate, ImportanceScore, finalize_score};
use crate::reviewer::LlmSettings;

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
    let factor_specs = FACTOR_SPECS
        .iter()
        .map(|spec| format!("{}({}..{})", spec.key, spec.min_points, spec.max_points))
        .collect::<Vec<_>>()
        .join(", ");
    let example = FACTOR_SPECS
        .first()
        .expect("factor specs must not be empty");
    format!(
        "请校准这个 PR 的重要性因子。规则版本：{}。\n\n\
         GitHub 结构化输入（正文已裁剪）：\n{input}\n\n\
         规则基线因子（含上下限和证据）：\n{baseline}\n\n\
         输出 JSON 格式：\n\
         {{\"factors\":[{{\"key\":\"{}\",\"points\":{},\"rationale\":\"中文理由\"}}]}}\n\
         必须且只能包含以下 {} 个 key，每个恰好一次：\n\
         {factor_specs}。\n\
         每项 rationale 必须是有证据的中文短句；没有证据时明确写无证据。",
        candidate.score.scoring_version,
        example.key,
        (example.min_points + example.max_points) / 2,
        FACTOR_SPECS.len(),
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
    if assessed.len() != FACTOR_SPECS.len() {
        return None;
    }

    let mut factors = candidate.score.factors.clone();
    for spec in FACTOR_SPECS {
        let (points, rationale) = assessed.get(spec.key)?;
        let points = i32::try_from(*points).ok()?;
        if !(spec.min_points..=spec.max_points).contains(&points) {
            return None;
        }
        let factor = factors.iter_mut().find(|factor| factor.key == spec.key)?;
        factor.points = points;
        factor.evidence = format!("{rationale} | Rule evidence: {}", factor.evidence);
    }
    Some(finalize_score(
        factors,
        format!("llm:{model}"),
        candidate.score.input_hash.clone(),
    ))
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
    anyhow::bail!("LLM 重要性校准结果缺少有效 {} 因子", FACTOR_SPECS.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::importance::FactorSpec;
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
        ImportanceCandidate::from_pr(&pr)
    }

    fn response_with_points(points: impl Fn(&FactorSpec) -> i32) -> String {
        let factors: Vec<Value> = FACTOR_SPECS
            .iter()
            .map(|spec| {
                json!({
                    "key": spec.key,
                    "points": points(spec),
                    "rationale": format!("{} 的证据理由", spec.key)
                })
            })
            .collect();
        json!({"factors": factors}).to_string()
    }

    fn valid_response() -> String {
        response_with_points(|spec| (spec.min_points + spec.max_points) / 2)
    }

    #[test]
    fn parses_calibrated_factors_and_recomputes_total() {
        let candidate = candidate();
        let score = parse_response(&candidate, &valid_response(), "k3-test").unwrap();
        let expected = FACTOR_SPECS
            .iter()
            .map(|spec| (spec.min_points + spec.max_points) / 2)
            .sum::<i32>()
            .clamp(0, 100) as u32;
        assert_eq!(score.method, "llm:k3-test");
        assert_eq!(score.factors.len(), FACTOR_SPECS.len());
        assert_eq!(score.raw_score, expected);
        assert_eq!(score.score, expected as f64 / 10.0);
        for (factor, baseline) in score.factors.iter().zip(&candidate.score.factors) {
            assert_eq!(factor.group, baseline.group);
            assert_eq!(factor.key, baseline.key);
            assert_eq!(factor.max_points, baseline.max_points);
            assert!(
                factor
                    .evidence
                    .starts_with(&format!("{} 的证据理由", factor.key))
            );
            assert!(factor.evidence.contains("Rule evidence:"));
        }
        assert_eq!(score.input_hash, candidate.score.input_hash);
    }

    #[test]
    fn accepts_and_enforces_every_spec_boundary() {
        let candidate = candidate();
        for spec in FACTOR_SPECS {
            for points in [spec.min_points, spec.max_points] {
                let response = response_with_points(|current| {
                    if current.key == spec.key {
                        points
                    } else {
                        current.min_points
                    }
                });
                assert!(
                    parse_response(&candidate, &response, "k3").is_some(),
                    "{} should accept {points}",
                    spec.key
                );
            }
            for points in [spec.min_points - 1, spec.max_points + 1] {
                let response = response_with_points(|current| {
                    if current.key == spec.key {
                        points
                    } else {
                        current.min_points
                    }
                });
                assert!(
                    parse_response(&candidate, &response, "k3").is_none(),
                    "{} should reject {points}",
                    spec.key
                );
            }
        }
    }

    #[test]
    fn rejects_missing_duplicate_and_unknown_factors() {
        let candidate = candidate();
        let mut data: Value = serde_json::from_str(&valid_response()).unwrap();
        data["factors"].as_array_mut().unwrap().remove(0);
        assert!(parse_response(&candidate, &data.to_string(), "k3").is_none());

        let mut data: Value = serde_json::from_str(&valid_response()).unwrap();
        let duplicate = data["factors"][0].clone();
        data["factors"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_response(&candidate, &data.to_string(), "k3").is_none());

        let mut data: Value = serde_json::from_str(&valid_response()).unwrap();
        data["factors"][0]["key"] = json!("unknown_factor");
        assert!(parse_response(&candidate, &data.to_string(), "k3").is_none());
    }

    #[test]
    fn prompt_contains_capped_input_and_all_factor_limits() {
        let prompt = build_prompt(&candidate());
        assert!(prompt.contains("o/r"));
        assert!(prompt.contains(&format!("必须且只能包含以下 {} 个 key", FACTOR_SPECS.len())));
        for spec in FACTOR_SPECS {
            assert!(prompt.contains(&format!(
                "{}({}..{})",
                spec.key, spec.min_points, spec.max_points
            )));
        }
        assert!(prompt.contains("规则基线因子"));
        assert!(!prompt.contains("overall_score"));
    }

    #[test]
    fn llm_total_matches_rules_for_unchanged_factor_points() {
        let candidate = candidate();
        let response = response_with_points(|spec| {
            candidate
                .score
                .factors
                .iter()
                .find(|factor| factor.key == spec.key)
                .unwrap()
                .points
        });
        let score = parse_response(&candidate, &response, "k3-test").unwrap();
        assert_eq!(score.raw_score, candidate.score.raw_score);
        assert_eq!(score.score, candidate.score.score);
        assert_eq!(score.scoring_version, candidate.score.scoring_version);
        assert_eq!(score.input_hash, candidate.score.input_hash);
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
