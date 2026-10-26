//! 本地预评审引擎：open-code-review（ocr CLI）。
//!
//! 流程：用 GitHub API 按需重建临时 git 仓库（base / head 两个 commit）
//! → `ocr review --commit HEAD --format json` → 解析 JSON 拿行级评论。
//! 评分仍由 reviewer.rs 的 LLM 调用负责，本模块只产出行级评论。
//! LLM 配置不落 ocr 全局配置：在临时目录写 config.json，子进程用
//! 环境变量 OCR_CONFIG_PATH 指向它（未配置 LLM 时不设，ocr 用自家全局配置）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::config::Config;
use crate::github::GitHub;
use crate::reviewer::{self, LlmSettings};

/// ocr 子进程总超时（超时 kill 后报错）。
const OCR_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// --background 注入的 PR 标题+正文截断长度。
const BACKGROUND_MAX_CHARS: usize = 2000;

/// 单次 ocr 评审结果：行级评论（原样保留 ocr schema）+ 已评审文件数 + 状态。
#[derive(Debug)]
pub struct OcrRunOutcome {
    pub comments: Vec<Value>,
    pub files_reviewed: usize,
    pub status: String,
}

/// 临时目录守卫：Drop 时整树删除（成功/失败/超时都清理）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let uniq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "prdesk-ocr-{}-{nanos:x}-{uniq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).context("创建临时仓库目录失败")?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 拼接临时仓库内的文件路径；含 `..` / 盘符 / 反斜杠等可疑成分的拒绝（防逃逸）。
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let mut p = root.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." || comp.contains('\\') || comp.contains(':') {
            return None;
        }
        p.push(comp);
    }
    Some(p)
}

/// 写文件到临时仓库，保持目录结构。
fn write_file(root: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let Some(path) = safe_join(root, rel) else {
        tracing::warn!("跳过可疑路径 {rel}");
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)?;
    Ok(())
}

/// 从 diff chunk 头部判断文件状态：added / deleted / modified。
fn file_status(chunk: &str) -> &'static str {
    for line in chunk.lines().take(10) {
        if line.starts_with("new file mode") {
            return "added";
        }
        if line.starts_with("deleted file mode") {
            return "deleted";
        }
    }
    "modified"
}

/// 在临时仓库里跑 git（失败带 stderr 尾部报错）。
async fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .await
        .context("启动 git 失败")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.chars().rev().take(300).collect::<String>().chars().rev().collect();
        anyhow::bail!("git {:?} 失败：{}", args, tail.trim());
    }
    Ok(())
}

async fn git_commit_all(repo: &Path, message: &str) -> Result<()> {
    git(
        repo,
        &["-c", "user.email=prdesk@local", "-c", "user.name=prdesk", "add", "-A"],
    )
    .await?;
    // --allow-empty：base 侧可能全是新增文件（无任何 base 内容可写）
    git(
        repo,
        &[
            "-c", "user.email=prdesk@local", "-c", "user.name=prdesk", "commit", "--quiet",
            "--allow-empty", "-m", message,
        ],
    )
    .await
}

/// 解析 ocr 的 result.json：status=failed 报错；comments 原样收集；
/// summary.files_reviewed 进 files_reviewed。
pub fn parse_ocr_result(text: &str) -> Result<OcrRunOutcome> {
    let data: Value = serde_json::from_str(text.trim()).context("ocr result.json 不是合法 JSON")?;
    if !data.is_object() {
        anyhow::bail!("ocr result.json 顶层不是 JSON 对象");
    }
    let status = data["status"].as_str().unwrap_or("").to_string();
    if status == "failed" {
        let msg = data["error"].as_str().unwrap_or("");
        anyhow::bail!("ocr 评审失败（status=failed）：{msg}");
    }
    // complete / partial / success（及未知非 failed 状态）都算可用
    let comments = data["comments"].as_array().cloned().unwrap_or_default();
    let files_reviewed = data["summary"]["files_reviewed"].as_u64().unwrap_or(0) as usize;
    Ok(OcrRunOutcome {
        comments,
        files_reviewed,
        status,
    })
}

/// 本地预评审主流程：重建临时 git 仓库 → 跑 ocr → 解析行级评论。
/// meta 为 GitHub::fetch_pr_meta 的原始返回（取 base.sha / head.sha / title / body）。
pub async fn run_ocr_review(
    cfg: &Config,
    gh: &GitHub,
    repo: &str,
    number: i64,
    meta: &Value,
    progress: &mut dyn FnMut(String),
) -> Result<OcrRunOutcome> {
    let base_sha = meta["base"]["sha"].as_str().unwrap_or("").to_string();
    let head_sha = meta["head"]["sha"].as_str().unwrap_or("").to_string();
    if base_sha.is_empty() || head_sha.is_empty() {
        anyhow::bail!("PR 元信息缺少 base/head sha");
    }
    let title = meta["title"].as_str().unwrap_or("");
    let body = meta["body"].as_str().unwrap_or("");
    let background: String = format!("{title}\n{body}")
        .chars()
        .take(BACKGROUND_MAX_CHARS)
        .collect();

    progress("创建临时仓库…".into());
    let tmp = TempDir::new()?;
    git(&tmp.0, &["init", "--quiet"]).await?;

    progress("拉取 PR diff…".into());
    let diff = gh.fetch_diff(repo, number).await?;
    let files = reviewer::split_diff_files(&diff);
    if files.is_empty() {
        anyhow::bail!("PR diff 为空，无文件可评审");
    }
    let total = files.len();

    // 第一个 commit：base 版本（新增文件在 base 侧 404 → None → 跳过；
    // 二进制/超大文件两边都跳过，ocr 反正会排除）。
    for (i, (path, _chunk)) in files.iter().enumerate() {
        progress(format!("重建 base 版本（{}/{total}）…", i + 1));
        if let Some(bytes) = gh.fetch_file_content(repo, path, &base_sha).await? {
            write_file(&tmp.0, path, &bytes)?;
        }
    }
    git_commit_all(&tmp.0, "base").await?;

    // 第二个 commit：head 版本（新增/修改写 head 内容；被删文件从磁盘移除）。
    for (i, (path, chunk)) in files.iter().enumerate() {
        progress(format!("重建 head 版本（{}/{total}）…", i + 1));
        if file_status(chunk) == "deleted" {
            if let Some(p) = safe_join(&tmp.0, path) {
                let _ = std::fs::remove_file(p);
            }
            continue;
        }
        if let Some(bytes) = gh.fetch_file_content(repo, path, &head_sha).await? {
            write_file(&tmp.0, path, &bytes)?;
        }
    }
    git_commit_all(&tmp.0, "head").await?;

    // LLM 配置：写进临时目录的 config.json，OCR_CONFIG_PATH 指向它（不落 ocr 全局配置）。
    // 未配置 LLM 时不设该变量，ocr 用自家全局配置。
    let mut cmd = Command::new(&cfg.ocr_path);
    cmd.arg("review")
        .arg("--repo")
        .arg(&tmp.0)
        .arg("--commit")
        .arg("HEAD")
        .arg("--format")
        .arg("json")
        .arg("--audience")
        .arg("agent")
        .arg("--output")
        .arg(tmp.0.join("result.json"))
        .arg("--background")
        .arg(&background)
        .current_dir(&tmp.0)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(llm) = LlmSettings::from_config(cfg) {
        let ocr_cfg = json!({
            "provider": "prdesk",
            "model": llm.model,
            "language": "Chinese",
            "custom_providers": {
                "prdesk": {
                    "protocol": "openai",
                    "url": llm.base_url,
                    "api_key": llm.api_key,
                    "model": llm.model,
                }
            }
        });
        let cfg_path = tmp.0.join("config.json");
        std::fs::write(&cfg_path, serde_json::to_string_pretty(&ocr_cfg)?)
            .context("写入 ocr config.json 失败")?;
        cmd.env("OCR_CONFIG_PATH", &cfg_path);
    }

    progress("运行 ocr 评审中（最长 30 分钟）…".into());
    // 超时：output() future 被 drop → kill_on_drop 杀掉子进程
    let out = match tokio::time::timeout(OCR_TIMEOUT, cmd.output()).await {
        Ok(r) => r.context(format!("启动 ocr 失败（检查 ocr_path 配置：{}）", cfg.ocr_path))?,
        Err(_) => anyhow::bail!("ocr 评审超时（30 分钟），已终止"),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.chars().rev().take(500).collect::<String>().chars().rev().collect();
        anyhow::bail!("ocr 退出码 {:?}：{}", out.status.code(), tail.trim());
    }

    progress("解析 ocr 结果…".into());
    let text = std::fs::read_to_string(tmp.0.join("result.json")).context("ocr 未生成 result.json")?;
    let outcome = parse_ocr_result(&text)?;
    progress(format!(
        "ocr 完成：{} 条行级评论，{} 个文件",
        outcome.comments.len(),
        outcome.files_reviewed
    ));
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_complete() {
        let text = r#"{
            "status": "complete",
            "comments": [
                {"path": "src/a.rs", "content": "这里可能越界", "start_line": 10, "end_line": 12,
                 "category": "bug", "severity": "high",
                 "suggestion_code": "x.checked_add(1)", "existing_code": "x + 1", "thinking": "..."},
                {"path": "src/b.rs", "content": "命名不清", "start_line": 3, "end_line": 3,
                 "category": "style", "severity": "low"}
            ],
            "summary": {"files_reviewed": 5}
        }"#;
        let out = parse_ocr_result(text).unwrap();
        assert_eq!(out.status, "complete");
        assert_eq!(out.files_reviewed, 5);
        assert_eq!(out.comments.len(), 2);
        // 原样保留 schema
        assert_eq!(out.comments[0]["path"].as_str(), Some("src/a.rs"));
        assert_eq!(out.comments[0]["start_line"].as_i64(), Some(10));
        assert_eq!(out.comments[0]["suggestion_code"].as_str(), Some("x.checked_add(1)"));
        assert_eq!(out.comments[1]["severity"].as_str(), Some("low"));
    }

    #[test]
    fn parse_result_partial_and_success_ok() {
        let out = parse_ocr_result(r#"{"status":"partial","comments":[],"summary":{"files_reviewed":2}}"#).unwrap();
        assert_eq!(out.status, "partial");
        assert_eq!(out.files_reviewed, 2);
        assert!(out.comments.is_empty());

        let out = parse_ocr_result(r#"{"status":"success","comments":[]}"#).unwrap();
        assert_eq!(out.status, "success");
        assert_eq!(out.files_reviewed, 0); // summary 缺失 → 0
    }

    #[test]
    fn parse_result_failed_errors() {
        let e = parse_ocr_result(r#"{"status":"failed","error":"LLM 连接失败","comments":[]}"#)
            .unwrap_err();
        assert!(e.to_string().contains("failed"));
        assert!(e.to_string().contains("LLM 连接失败"));
    }

    #[test]
    fn parse_result_bad_json_errors() {
        assert!(parse_ocr_result("").is_err());
        assert!(parse_ocr_result("not json").is_err());
        assert!(parse_ocr_result("[1,2]").is_err()); // 非对象
    }

    #[test]
    fn file_status_detection() {
        assert_eq!(file_status("diff --git a/x b/x\nnew file mode 100644\nindex 000..111\n"), "added");
        assert_eq!(file_status("diff --git a/x b/x\ndeleted file mode 100644\nindex 111..000\n"), "deleted");
        assert_eq!(file_status("diff --git a/x b/x\nindex 111..222 100644\n"), "modified");
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let root = Path::new("/tmp/x");
        assert!(safe_join(root, "src/a.rs").is_some());
        assert!(safe_join(root, "../etc/passwd").is_none());
        assert!(safe_join(root, "a/../../b").is_none());
        assert!(safe_join(root, "C:/win").is_none());
        assert!(safe_join(root, "a\\b").is_none());
        assert!(safe_join(root, "/abs").is_none());
        assert!(safe_join(root, "").is_none());
    }
}
