//! 本地 LLM 预评审：桌面端直连 OpenAI 兼容 API 跑代码评审。
//!
//! 行为移植自 astrbot_plugin_github_pr_review 的 reviewer.py / plugin_metadata.py：
//! diff 按文件分块、跳过 lock/生成文件、超限截断并注明；要求模型输出严格 JSON，
//! 解析失败把 RETRY_SUFFIX 追加到 user prompt 重试一次，仍失败则报错。

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::Config;
use crate::github::GitHub;

/// 单次评审纳入的 diff 上限（对齐插件 max_diff_files / max_diff_chars 默认值）。
pub const MAX_DIFF_FILES: usize = 30;
pub const MAX_DIFF_CHARS: usize = 60000;

/// 跳过的 lock / 生成文件模式（fnmatch 风格，匹配路径任意段）
const SKIP_PATTERNS: &[&str] = &[
    "*.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "*.min.js",
    "*.min.css",
    "dist/*",
    "*/dist/*",
    "*.pb.go",
    "*.generated.*",
    "*.snap",
];

/// 默认评分标准（plugin_metadata.py DEFAULT_SCORE_RUBRIC 原样移植）
pub const DEFAULT_SCORE_RUBRIC: &str = "\
评分标准（每项 0-10 分）：
- readability（可读性）：命名、结构、注释是否清晰易读
- correctness_risk（正确性风险）：逻辑是否可能引入 bug，边界条件是否处理（分数越高风险越低）
- test_coverage（测试覆盖）：改动是否有相应测试，测试是否覆盖关键路径
- security（安全）：是否引入注入、越权、敏感信息泄露等安全隐患

体量与简洁度也是评分维度之一：在变更目的相同的前提下，更小更聚焦的 PR 应得更高分；
巨型 PR（XL 体量）因可评审性差应酌情扣分，显著影响 overall_score。
overall_score 为综合加权总分（0-10），请客观严格打分。
";

/// 评审主 Prompt 模板（plugin_metadata.py DEFAULT_REVIEW_PROMPT 移植 + 强化：
/// dimensions 四维必填、summary 强制结构化分点——避免弱模型漏维度/写成长段难读文本）
/// 变量：{{repo}} {{number}} {{title}} {{author}} {{url}} {{size_info}} {{rubric}} {{diff}} {{note}}
pub const DEFAULT_REVIEW_PROMPT: &str = "\
请对以下 GitHub Pull Request 进行代码预评审。

仓库：{{repo}}
PR：#{{number}} {{title}}
作者：{{author}}
链接：{{url}}
{{size_info}}

{{rubric}}

以下是该 PR 的 diff（已过滤 lock/生成文件）：
```diff
{{diff}}
```
{{note}}

请只输出一个严格的 JSON 对象（不要输出任何其他文字、不要使用 markdown 代码块），格式：
{\"overall_score\": 8.5,
 \"dimensions\": {\"readability\": 8, \"correctness_risk\": 9, \"test_coverage\": 6, \"security\": 9},
 \"summary\": \"……\",
 \"flagged_files\": [{\"path\": \"src/foo.py\", \"issue\": \"具体问题描述\"}]}
硬性要求：
- dimensions 必须完整包含 readability、correctness_risk、test_coverage、security 四个键，缺一不可；
- summary 用中文、Markdown 分点格式，两节：「**优点**」逐条列点；「**风险与建议**」逐条列点并指出具体文件/位置（无风险写「无明显风险」）；总共不超过 300 字，禁止写成一整段；
- flagged_files 只列出确有问题的文件，没有问题则为空数组。
";

/// JSON 解析失败后的重试提示词追加（plugin_metadata.py RETRY_SUFFIX 原样移植）
pub const RETRY_SUFFIX: &str =
    "\n\n你上一次的回复无法被解析为 JSON。请严格只输出一个 JSON 对象，不要包含任何解释文字或 markdown 代码块标记。";

/// LLM 评审使用的系统提示词（plugin_metadata.py DEFAULT_REVIEWER_PERSONA 原样移植）
pub const DEFAULT_REVIEWER_PERSONA: &str = "\
你是一位资深代码评审专家，擅长快速发现代码中的逻辑错误、安全风险与可维护性问题。\
评审时兼顾体量与简洁度：同等目的下更小更聚焦的 PR 更优，巨型 PR 因可评审性差应酌情扣分。\
评审请用中文，客观严格，结论必须是指定格式的 JSON。";

/// LLM 连接参数；api_key 或 model 缺失时 from_config 返回 None（预评审不可用）。
/// client 复用（连接池/TLS 复用），超时 600s——评审生成本来就慢，不用 GitHub 的 30s。
pub struct LlmSettings {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    client: reqwest::Client,
}

impl LlmSettings {
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let api_key = cfg.llm_key()?;
        let model = cfg.llm_model.clone();
        if model.is_empty() {
            return None;
        }
        let client = reqwest::Client::builder()
            .user_agent("pr-desk-rs")
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .unwrap_or_default();
        Some(Self {
            base_url: cfg.llm_base_url.clone(),
            api_key,
            model,
            client,
        })
    }
}

/// 评审 prompt 的 PR 元信息。
pub struct PrMeta {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub author: String,
    pub url: String,
}

/// 变更规模统计（注入 prompt 的体量说明 + 结果 JSON 的体量字段）。
pub struct SizeStats {
    pub files_changed: i64,
    pub additions: i64,
    pub deletions: i64,
}

/// PR 体量标签：XS <10 行，S <50，M <250，L <1000，XL ≥1000（行数 = 增+删）。
pub fn size_label(additions: i64, deletions: i64) -> &'static str {
    let lines = additions + deletions;
    if lines < 10 {
        "XS"
    } else if lines < 50 {
        "S"
    } else if lines < 250 {
        "M"
    } else if lines < 1000 {
        "L"
    } else {
        "XL"
    }
}

/// 把 SizeStats 渲染为注入 prompt 的规模说明行（空统计返回空串）。
pub fn build_size_info(stats: Option<&SizeStats>) -> String {
    let Some(stats) = stats else {
        return String::new();
    };
    let adds = stats.additions;
    let dels = stats.deletions;
    let files = stats.files_changed;
    let label = size_label(adds, dels);
    let avg = if files > 0 {
        ((adds + dels) as f64 / files as f64 * 10.0).round() / 10.0
    } else {
        0.0
    };
    format!(
        "变更规模：+{adds}/-{dels}，共 {files} 个文件，体量 {label}（平均约 {avg:.1} 行/文件）。"
    )
}

/// Python fnmatch 语义：`*` 跨 `/` 匹配任意字符序列，`?` 匹配单个字符。
fn fnmatch(name: &str, pattern: &str) -> bool {
    fn rec(n: &[char], p: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| rec(&n[i..], &p[1..])),
            Some('?') => !n.is_empty() && rec(&n[1..], &p[1..]),
            Some(c) => !n.is_empty() && n[0] == *c && rec(&n[1..], &p[1..]),
        }
    }
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    rec(&n, &p)
}

/// 路径是否命中 lock / 生成文件跳过模式（全路径或 basename 匹配其一即跳过）。
fn path_skipped(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    SKIP_PATTERNS
        .iter()
        .any(|pat| fnmatch(path, pat) || fnmatch(base, pat))
}

/// 把 unified patch 按 `diff --git` 头拆成 [(path, chunk), ...]。
pub fn split_diff_files(patch: &str) -> Vec<(String, String)> {
    let mut starts: Vec<(usize, String)> = Vec::new();
    let mut offset = 0usize;
    for line in patch.split_inclusive('\n') {
        let l = line.strip_suffix('\n').unwrap_or(line);
        if let Some(rest) = l.strip_prefix("diff --git a/") {
            if let Some(pos) = rest.find(" b/") {
                let path = rest[pos + 3..].trim();
                if !path.is_empty() {
                    starts.push((offset, path.to_string()));
                }
            }
        }
        offset += line.len();
    }
    let mut chunks = Vec::with_capacity(starts.len());
    for (i, (start, path)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map(|(s, _)| *s).unwrap_or(patch.len());
        chunks.push((path.clone(), patch[*start..end].to_string()));
    }
    chunks
}

/// 过滤 lock/生成文件并按上限截断。
/// 返回 (diff_text, included_files, omitted_files)；omitted 含被跳过的 lock/生成文件。
pub fn filter_diff(
    patch: &str,
    max_files: usize,
    max_chars: usize,
) -> (String, Vec<String>, Vec<String>) {
    let chunks = split_diff_files(patch);
    let mut included: Vec<(String, String)> = Vec::new();
    let mut omitted: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (path, chunk) in chunks {
        if path_skipped(&path) {
            skipped.push(path);
            continue;
        }
        if included.len() >= max_files || (total > 0 && total + chunk.len() > max_chars) {
            omitted.push(path);
            continue;
        }
        total += chunk.len();
        included.push((path, chunk));
    }
    let diff_text: String = included.iter().map(|(_, c)| c.as_str()).collect();
    omitted.extend(skipped);
    (diff_text, included.into_iter().map(|(p, _)| p).collect(), omitted)
}

/// 输出格式约定（自定义 prompt 模板时自动附加，保证 JSON 输出契约不被破坏）。
pub const OUTPUT_CONTRACT: &str = "\
请只输出一个严格的 JSON 对象（不要输出任何其他文字、不要使用 markdown 代码块），格式：
{\"overall_score\": 8.5,
 \"dimensions\": {\"readability\": 8, \"correctness_risk\": 9, \"test_coverage\": 6, \"security\": 9},
 \"summary\": \"……\",
 \"flagged_files\": [{\"path\": \"src/foo.py\", \"issue\": \"具体问题描述\"}]}
硬性要求：
- dimensions 必须完整包含 readability、correctness_risk、test_coverage、security 四个键，缺一不可；
- summary 用中文、Markdown 分点格式，两节：「**优点**」逐条列点；「**风险与建议**」逐条列点并指出具体文件/位置（无风险写「无明显风险」）；
- flagged_files 只列出确有问题的文件，没有问题则为空数组。
";

/// 组装评审 prompt；被截断/跳过的文件以注释形式附在末尾。
/// rubric 为空（或全空白）时使用 DEFAULT_SCORE_RUBRIC。
/// custom_template 非空时替代 DEFAULT_REVIEW_PROMPT 作为主模板（变量同），
/// 并自动追加 OUTPUT_CONTRACT（自定义模板只管评审视角，输出契约不可丢）。
pub fn build_prompt(
    meta: &PrMeta,
    diff_text: &str,
    omitted: &[String],
    rubric: &str,
    size_stats: Option<&SizeStats>,
    custom_template: Option<&str>,
) -> String {
    let mut notes: Vec<String> = Vec::new();
    if !omitted.is_empty() {
        let mut shown = omitted.iter().take(20).cloned().collect::<Vec<_>>().join("、");
        if omitted.len() > 20 {
            shown.push_str(" 等");
        }
        notes.push(format!(
            "注意：diff 过长已截断，以下 {} 个文件未包含在内（含 lock/生成文件），评审时请说明这一点：{shown}",
            omitted.len()
        ));
    }
    let rubric = if rubric.trim().is_empty() {
        DEFAULT_SCORE_RUBRIC
    } else {
        rubric.trim()
    };
    let (template, is_custom) = match custom_template {
        Some(t) if !t.trim().is_empty() => (t, true),
        _ => (DEFAULT_REVIEW_PROMPT, false),
    };
    let rendered = template
        .replace("{{repo}}", &meta.repo)
        .replace("{{number}}", &meta.number.to_string())
        .replace("{{title}}", &meta.title)
        .replace("{{author}}", &meta.author)
        .replace("{{url}}", &meta.url)
        .replace("{{size_info}}", &build_size_info(size_stats))
        .replace("{{rubric}}", rubric)
        .replace("{{diff}}", diff_text)
        .replace("{{note}}", &notes.join("\n"));
    if is_custom {
        format!("{}\n\n{}", rendered.trim_end(), OUTPUT_CONTRACT)
    } else {
        rendered
    }
}

/// JSON 值转字符串：字符串原样，标量 to_string，其余/缺失为 ""。
fn jstr(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// 从 LLM 回复中提取严格 JSON 评审结果。
/// 剥离 markdown 代码块围栏后解析；校验 overall_score 必填。失败返回 None。
pub fn parse_review_json(text: &str) -> Option<Value> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    let extracted = if let Some(open) = s.find("```") {
        // 围栏内：取第一个 { 到围栏结束前的最后一个 }
        let after = &s[open + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let body = match after.find("```") {
            Some(close) => &after[..close],
            None => after,
        };
        let start = body.find('{')?;
        let end = body.rfind('}')?;
        if end <= start {
            return None;
        }
        &body[start..=end]
    } else {
        // 无围栏时截取第一个 { 到最后一个 }
        let start = s.find('{')?;
        let end = s.rfind('}')?;
        if end <= start {
            return None;
        }
        &s[start..=end]
    };
    let data: Value = serde_json::from_str(extracted).ok()?;
    if !data.is_object() {
        return None;
    }
    let score = data.get("overall_score")?.as_f64()?;
    let dimensions = match data.get("dimensions") {
        Some(d) if d.is_object() => d.clone(),
        _ => json!({}),
    };
    let flagged_files: Vec<Value> = match data.get("flagged_files") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|f| f.is_object())
            .map(|f| {
                json!({
                    "path": jstr(f.get("path")),
                    "issue": jstr(f.get("issue")),
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    Some(json!({
        "score": score,
        "dimensions": dimensions,
        "summary": jstr(data.get("summary")),
        "flagged_files": flagged_files,
    }))
}

/// 统计 diff 文本的增/删行数（排除 +++/--- 文件头）。
fn diff_line_stats(diff_text: &str) -> (i64, i64) {
    let mut adds = 0i64;
    let mut dels = 0i64;
    for line in diff_text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            adds += 1;
        } else if line.starts_with('-') {
            dels += 1;
        }
    }
    (adds, dels)
}

/// 调用 OpenAI 兼容 chat/completions，返回 assistant 文本。
async fn chat_completion(llm: &LlmSettings, user_prompt: &str) -> Result<String> {
    let url = format!("{}/chat/completions", llm.base_url.trim_end_matches('/'));
    let body = json!({
        "model": llm.model,
        "messages": [
            {"role": "system", "content": DEFAULT_REVIEWER_PERSONA},
            {"role": "user", "content": user_prompt},
        ],
        "temperature": 0.2,
    });
    let resp = llm
        .client
        .post(&url)
        .header("Authorization", format!("Bearer {}", llm.api_key))
        .json(&body)
        .send()
        .await
        .context("LLM 请求发送失败")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("LLM API {status}: {}", text.chars().take(500).collect::<String>());
    }
    let data: Value = resp.json().await.context("LLM 响应不是 JSON")?;
    data["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .context("LLM 响应缺少 choices[0].message.content")
}

/// 本地预评审主流程：拉 PR 元信息 + diff → 过滤 → LLM 评审 → 组装结果 JSON。
/// 返回的 JSON 可直接传给 Db::upsert_review。
/// lang："zh"/"en"，进度回调与失败摘要的文案语言（LLM prompt 本身保持中文）。
/// custom_prompt：用户在设置里自定义的评审模板（None/空 = 用默认模板）；
/// 自定义模板自动追加输出契约（OUTPUT_CONTRACT），JSON 格式不会被破坏。
pub async fn run_review(
    llm: &LlmSettings,
    gh: &GitHub,
    repo: &str,
    number: i64,
    progress: &mut (dyn FnMut(String) + Send),
    lang: &str,
    custom_prompt: Option<&str>,
) -> Result<Value> {
    use crate::i18n::{tr, trf};
    progress(tr(lang, "prog_fetch_pr").into());
    let meta_raw = gh.fetch_pr_meta(repo, number).await?;
    let head_sha = meta_raw["head"]["sha"].as_str().unwrap_or("").to_string();
    if head_sha.is_empty() {
        // 评审主键含 head_sha，空 sha 入库会互相覆盖——缺 sha 直接不评审
        anyhow::bail!("{}", tr(lang, "err_missing_sha"));
    }
    let meta = PrMeta {
        repo: repo.to_string(),
        number,
        title: meta_raw["title"].as_str().unwrap_or("").to_string(),
        author: meta_raw["user"]["login"].as_str().unwrap_or("").to_string(),
        url: meta_raw["html_url"].as_str().unwrap_or("").to_string(),
    };

    progress(tr(lang, "prog_fetch_diff").into());
    let diff_full = gh.fetch_diff(repo, number).await?;
    let (diff_text, included, omitted) = filter_diff(&diff_full, MAX_DIFF_FILES, MAX_DIFF_CHARS);

    let (additions, deletions) = diff_line_stats(&diff_text);
    let files_changed = included.len() as i64;
    // diff 被截断或文件被 omit（含 lock/生成文件跳过）时置 1
    let oversized = if omitted.is_empty() { 0 } else { 1 };
    let stats = SizeStats {
        files_changed,
        additions,
        deletions,
    };
    let reviewed_at = crate::db::now();
    let size_label = size_label(additions, deletions);

    let base = json!({
        "repo": repo,
        "number": number,
        "head_sha": head_sha,
        "reviewed_at": reviewed_at,
        "oversized": oversized,
        "files_changed": files_changed,
        "additions": additions,
        "deletions": deletions,
        "size_label": size_label,
    });
    let finish = |score: Value, dimensions: Value, summary: String, flagged_files: Value| {
        let high_value = if score.as_f64().unwrap_or(0.0) >= 8.0 { 1 } else { 0 };
        let mut out = base.clone();
        out["score"] = score;
        out["high_value"] = json!(high_value);
        out["dimensions"] = dimensions;
        out["summary"] = json!(summary);
        out["flagged_files"] = flagged_files;
        out
    };

    if diff_text.trim().is_empty() {
        // 对齐插件：diff 为空（可能全部为 lock/生成文件）时不调 LLM，score 记 NULL
        return Ok(finish(
            Value::Null,
            json!({}),
            tr(lang, "empty_diff_summary").into(),
            json!([]),
        ));
    }

    let prompt = build_prompt(&meta, &diff_text, &omitted, "", Some(&stats), custom_prompt);

    progress(tr(lang, "prog_llm_review").into());
    let mut last_raw = String::new();
    let mut parsed = None;
    for attempt in 0..2 {
        let p = if attempt == 0 {
            prompt.clone()
        } else {
            format!("{prompt}{RETRY_SUFFIX}")
        };
        let raw = chat_completion(llm, &p).await?;
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            continue;
        }
        last_raw = raw.clone();
        if let Some(p) = parse_review_json(&raw) {
            parsed = Some(p);
            break;
        }
    }

    progress(tr(lang, "prog_parse_review").into());
    let Some(mut parsed) = parsed else {
        anyhow::bail!(
            "{}",
            trf(
                lang,
                "err_parse_failed",
                &[&last_raw.chars().take(200).collect::<String>()]
            )
        );
    };
    if !omitted.is_empty() {
        let summary = parsed["summary"].as_str().unwrap_or("").to_string();
        parsed["summary"] = json!(format!(
            "{summary}\n{}",
            trf(lang, "oversized_note", &[&omitted.len().to_string()])
        ));
    }
    Ok(finish(
        parsed["score"].clone(),
        parsed["dimensions"].clone(),
        parsed["summary"].as_str().unwrap_or("").to_string(),
        parsed["flagged_files"].clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,3 @@
 fn a() {}
+fn b() {}
 fn c() {}
diff --git a/yarn.lock b/yarn.lock
index 333..444 100644
--- a/yarn.lock
+++ b/yarn.lock
@@ -1 +1 @@
-old
+new
diff --git a/dist/bundle.js b/dist/bundle.js
index 555..666 100644
--- a/dist/bundle.js
+++ b/dist/bundle.js
@@ -1 +1 @@
-x
+y
";

    #[test]
    fn size_label_thresholds() {
        assert_eq!(size_label(0, 0), "XS");
        assert_eq!(size_label(5, 4), "XS");
        assert_eq!(size_label(5, 5), "S");
        assert_eq!(size_label(30, 19), "S");
        assert_eq!(size_label(30, 20), "M");
        assert_eq!(size_label(200, 49), "M");
        assert_eq!(size_label(200, 50), "L");
        assert_eq!(size_label(500, 499), "L");
        assert_eq!(size_label(500, 500), "XL");
    }

    #[test]
    fn size_info_rendering() {
        assert_eq!(build_size_info(None), "");
        let s = SizeStats { files_changed: 3, additions: 90, deletions: 10 };
        assert_eq!(
            build_size_info(Some(&s)),
            "变更规模：+90/-10，共 3 个文件，体量 M（平均约 33.3 行/文件）。"
        );
        let zero = SizeStats { files_changed: 0, additions: 0, deletions: 0 };
        assert_eq!(
            build_size_info(Some(&zero)),
            "变更规模：+0/-0，共 0 个文件，体量 XS（平均约 0.0 行/文件）。"
        );
    }

    #[test]
    fn path_skipped_patterns() {
        assert!(path_skipped("yarn.lock"));
        assert!(path_skipped("sub/dir/yarn.lock"));
        assert!(path_skipped("foo/package-lock.json"));
        assert!(path_skipped("web/app.min.js"));
        assert!(path_skipped("dist/bundle.js"));
        assert!(path_skipped("web/dist/bundle.js"));
        assert!(path_skipped("proto/x.pb.go"));
        assert!(path_skipped("gen/api.generated.ts"));
        assert!(path_skipped("__snapshots__/a.snap"));
        assert!(path_skipped("deps/cargo.lock"));
        assert!(!path_skipped("src/main.rs"));
        assert!(!path_skipped("src/lock.rs"));
        assert!(!path_skipped("src/distributed.rs"));
    }

    #[test]
    fn split_diff_files_basic() {
        let chunks = split_diff_files(SAMPLE_DIFF);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, "src/a.rs");
        assert!(chunks[0].1.starts_with("diff --git a/src/a.rs"));
        assert!(chunks[0].1.ends_with("fn c() {}\n"));
        assert_eq!(chunks[1].0, "yarn.lock");
        assert_eq!(chunks[2].0, "dist/bundle.js");
        assert!(split_diff_files("").is_empty());
        assert!(split_diff_files("no diff here").is_empty());
    }

    #[test]
    fn filter_diff_skips_lock_and_generated() {
        let (text, included, omitted) = filter_diff(SAMPLE_DIFF, 30, 60000);
        assert_eq!(included, vec!["src/a.rs".to_string()]);
        assert!(text.contains("fn b()"));
        assert!(!text.contains("yarn.lock"));
        // omitted = 截断省略 + 跳过（lock/生成文件排在后面）
        assert_eq!(omitted, vec!["yarn.lock".to_string(), "dist/bundle.js".to_string()]);
    }

    #[test]
    fn filter_diff_max_files_and_chars() {
        let (_t, included, omitted) = filter_diff(SAMPLE_DIFF, 0, 60000);
        assert!(included.is_empty());
        assert_eq!(omitted, vec!["src/a.rs".to_string(), "yarn.lock".to_string(), "dist/bundle.js".to_string()]);

        // 字符上限：第一个文件纳入后 total 超限，后续省略；但首文件即使超限也纳入（total==0 不截断）
        let (text, included, omitted) = filter_diff(SAMPLE_DIFF, 30, 10);
        assert_eq!(included, vec!["src/a.rs".to_string()]);
        assert_eq!(text.len(), chunks_len(SAMPLE_DIFF, "src/a.rs"));
        assert_eq!(omitted, vec!["yarn.lock".to_string(), "dist/bundle.js".to_string()]);
    }

    fn chunks_len(patch: &str, path: &str) -> usize {
        split_diff_files(patch)
            .into_iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.len())
            .unwrap()
    }

    #[test]
    fn build_prompt_substitutes_vars() {
        let meta = PrMeta {
            repo: "o/r".into(),
            number: 42,
            title: "标题".into(),
            author: "alice".into(),
            url: "https://github.com/o/r/pull/42".into(),
        };
        let stats = SizeStats { files_changed: 1, additions: 1, deletions: 0 };
        let p = build_prompt(&meta, "DIFF_BODY", &[], "", Some(&stats), None);
        assert!(p.contains("仓库：o/r"));
        assert!(p.contains("PR：#42 标题"));
        assert!(p.contains("作者：alice"));
        assert!(p.contains("链接：https://github.com/o/r/pull/42"));
        assert!(p.contains("变更规模：+1/-0，共 1 个文件，体量 XS"));
        assert!(p.contains("```diff\nDIFF_BODY\n```"));
        assert!(p.contains("评分标准（每项 0-10 分）：")); // 默认 rubric
        assert!(!p.contains("{{"));

        let custom = build_prompt(&meta, "D", &[], "自定义标准", None, None);
        assert!(custom.contains("自定义标准"));
        assert!(!custom.contains("评分标准（每项 0-10 分）："));
    }

    #[test]
    fn build_prompt_custom_template_keeps_output_contract() {
        let meta = PrMeta {
            repo: "o/r".into(),
            number: 42,
            title: "标题".into(),
            author: "alice".into(),
            url: "u".into(),
        };
        let p = build_prompt(&meta, "DIFF", &[], "", None, Some("自定义视角评审 {{repo}}#{{number}}\n{{diff}}"));
        assert!(p.contains("自定义视角评审 o/r#42"));
        assert!(p.contains("DIFF"));
        // 输出契约自动附加：dimensions 四维硬性要求必须在
        assert!(p.contains("overall_score"));
        assert!(p.contains("dimensions 必须完整包含"));
        assert!(!p.contains("{{")); // 变量已替换
    }

    #[test]
    fn build_prompt_omitted_note() {
        let meta = PrMeta {
            repo: "o/r".into(),
            number: 1,
            title: "t".into(),
            author: "a".into(),
            url: "u".into(),
        };
        let omitted: Vec<String> = (0..25).map(|i| format!("f{i}.rs")).collect();
        let p = build_prompt(&meta, "D", &omitted, "", None, None);
        assert!(p.contains("以下 25 个文件未包含在内（含 lock/生成文件），评审时请说明这一点"));
        assert!(p.contains("f0.rs、f1.rs"));
        assert!(p.contains("f19.rs 等")); // 只展示前 20 个
        assert!(!p.contains("f20.rs"));
    }

    #[test]
    fn parse_review_json_plain() {
        let v = parse_review_json(
            r#"{"overall_score": 8.5, "dimensions": {"readability": 8}, "summary": "好", "flagged_files": [{"path": "a.rs", "issue": "x"}]}"#,
        )
        .unwrap();
        assert_eq!(v["score"].as_f64(), Some(8.5));
        assert_eq!(v["dimensions"]["readability"].as_i64(), Some(8));
        assert_eq!(v["summary"].as_str(), Some("好"));
        assert_eq!(v["flagged_files"][0]["path"].as_str(), Some("a.rs"));
    }

    #[test]
    fn parse_review_json_fenced_and_noisy() {
        let v = parse_review_json("前言\n```json\n{\"overall_score\": 7, \"summary\": \"s\"}\n```\n后语").unwrap();
        assert_eq!(v["score"].as_f64(), Some(7.0));
        assert_eq!(v["dimensions"], json!({}));
        assert_eq!(v["flagged_files"], json!([]));

        let v = parse_review_json("结果是 {\"overall_score\": 6} 完").unwrap();
        assert_eq!(v["score"].as_f64(), Some(6.0));
    }

    #[test]
    fn parse_review_json_failures() {
        assert!(parse_review_json("").is_none());
        assert!(parse_review_json("没有 JSON").is_none());
        assert!(parse_review_json("{}").is_none()); // 缺 overall_score
        assert!(parse_review_json("{\"overall_score\": \"高\"}").is_none()); // 非数字
        assert!(parse_review_json("[1,2]").is_none()); // 非对象
        assert!(parse_review_json("{\"overall_score\": 8,").is_none()); // 截断 JSON
    }

    #[test]
    fn parse_review_json_flagged_files_normalized() {
        let v = parse_review_json(
            r#"{"overall_score": 5, "flagged_files": [{"path": "a.rs", "issue": "i"}, "junk", {"path": "b.rs"}]}"#,
        )
        .unwrap();
        let flagged = v["flagged_files"].as_array().unwrap();
        assert_eq!(flagged.len(), 2); // 非对象项被丢弃
        assert_eq!(flagged[1]["path"].as_str(), Some("b.rs"));
        assert_eq!(flagged[1]["issue"].as_str(), Some(""));
    }

    #[test]
    fn diff_line_stats_counts() {
        // 完整 diff：+fn b() / +new / +y = 3 增，-old / -x = 2 删
        let (adds, dels) = diff_line_stats(SAMPLE_DIFF);
        assert_eq!(adds, 3);
        assert_eq!(dels, 2);
        // 过滤后只剩 src/a.rs
        let (filtered, _, _) = filter_diff(SAMPLE_DIFF, 30, 60000);
        let (adds, dels) = diff_line_stats(&filtered);
        assert_eq!(adds, 1);
        assert_eq!(dels, 0);
    }
}
