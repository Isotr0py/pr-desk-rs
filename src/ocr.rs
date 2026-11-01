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
/// summary.files_reviewed 进 files_reviewed。lang：错误文案语言（"zh"/"en"）。
pub fn parse_ocr_result(text: &str, lang: &str) -> Result<OcrRunOutcome> {
    let data: Value = serde_json::from_str(text.trim()).context("ocr result.json 不是合法 JSON")?;
    if !data.is_object() {
        anyhow::bail!("ocr result.json 顶层不是 JSON 对象");
    }
    let status = data["status"].as_str().unwrap_or("").to_string();
    if status == "failed" {
        let msg = data["error"].as_str().unwrap_or("");
        anyhow::bail!("{}", crate::i18n::trf(lang, "err_ocr_failed_status", &[msg]));
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

/// ocr 可执行文件名候选（先 .exe 后裸名，Windows / 类 Unix 都覆盖）。
const OCR_BIN_NAMES: [&str; 2] = ["ocr.exe", "ocr"];

/// 参数化候选解析（纯函数，便于单测）：按序返回第一个存在的文件。
/// a) ocr_path 显式配置（非空且非默认 "ocr"）且文件存在 → 用户配置优先
/// b) <exe 所在目录>/bin/ocr[.exe]（随包 sidecar，跟随安装布局）
/// c) <cwd>/bin/ocr[.exe]（开发布局：从仓库根跑 target/debug/…）
/// d) 原样返回 ocr_path（含默认 "ocr"，走 PATH 解析）
fn resolve_ocr_path_in(ocr_path: &str, exe_dir: Option<&Path>, cwd: &Path) -> PathBuf {
    if !ocr_path.is_empty() && ocr_path != "ocr" && Path::new(ocr_path).is_file() {
        return PathBuf::from(ocr_path);
    }
    for base in [exe_dir.map(Path::to_path_buf), Some(cwd.to_path_buf())]
        .into_iter()
        .flatten()
    {
        for name in OCR_BIN_NAMES {
            let cand = base.join("bin").join(name);
            if cand.is_file() {
                return cand;
            }
        }
    }
    PathBuf::from(ocr_path)
}

/// 解析 ocr CLI 路径：显式配置 > 随包 bin/（exe 旁）> 开发布局 bin/（cwd 下）> PATH。
pub fn resolve_ocr_path(cfg: &Config) -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_ocr_path_in(&cfg.ocr_path, exe_dir.as_deref(), &cwd)
}

/// 本地预评审主流程：重建临时 git 仓库 → 跑 ocr → 解析行级评论。
/// meta 为 GitHub::fetch_pr_meta 的原始返回（取 base.sha / head.sha / title / body）。
/// lang："zh"/"en"，进度回调与错误文案语言。
pub async fn run_ocr_review(
    cfg: &Config,
    gh: &GitHub,
    repo: &str,
    number: i64,
    meta: &Value,
    progress: &mut (dyn FnMut(String) + Send),
    lang: &str,
) -> Result<OcrRunOutcome> {
    use crate::i18n::{tr, trf};
    let base_sha = meta["base"]["sha"].as_str().unwrap_or("").to_string();
    let head_sha = meta["head"]["sha"].as_str().unwrap_or("").to_string();
    if base_sha.is_empty() || head_sha.is_empty() {
        anyhow::bail!("{}", tr(lang, "err_missing_sha"));
    }
    let title = meta["title"].as_str().unwrap_or("");
    let body = meta["body"].as_str().unwrap_or("");
    let background: String = format!("{title}\n{body}")
        .chars()
        .take(BACKGROUND_MAX_CHARS)
        .collect();

    progress(tr(lang, "prog_create_repo").into());
    let tmp = TempDir::new()?;
    git(&tmp.0, &["init", "--quiet"]).await?;

    progress(tr(lang, "prog_fetch_pr_diff").into());
    let diff = gh.fetch_diff(repo, number).await?;
    let files = reviewer::split_diff_files(&diff);
    if files.is_empty() {
        anyhow::bail!("{}", tr(lang, "err_empty_diff"));
    }
    let total = files.len();

    // 第一个 commit：base 版本（新增文件在 base 侧 404 → None → 跳过；
    // 二进制/超大文件两边都跳过，ocr 反正会排除）。
    for (i, (path, _chunk)) in files.iter().enumerate() {
        progress(trf(lang, "prog_rebuild_base", &[&(i + 1).to_string(), &total.to_string()]));
        if let Some(bytes) = gh.fetch_file_content(repo, path, &base_sha).await? {
            write_file(&tmp.0, path, &bytes)?;
        }
    }
    git_commit_all(&tmp.0, "base").await?;

    // 第二个 commit：head 版本（新增/修改写 head 内容；被删文件从磁盘移除）。
    for (i, (path, chunk)) in files.iter().enumerate() {
        progress(trf(lang, "prog_rebuild_head", &[&(i + 1).to_string(), &total.to_string()]));
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

    // LLM 配置：经环境变量传给 ocr 子进程（OCR_CONFIG_PATH 只对 ocr 的 config 子命令生效，
    // review 的 LLM resolver 只认 OCR_LLM_* 环境变量和 ~/.opencodereview/config.json——后者不动）。
    // 未配置 LLM 时不设这些变量，ocr 用自家全局配置。
    let ocr_bin = resolve_ocr_path(cfg);
    let mut cmd = Command::new(&ocr_bin);
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
        cmd.env("OCR_LLM_URL", &llm.base_url)
            .env("OCR_LLM_TOKEN", &llm.api_key)
            .env("OCR_LLM_MODEL", &llm.model)
            .env("OCR_LLM_PROTOCOL", "openai");
    }

    progress(tr(lang, "prog_ocr_running").into());
    // 超时：output() future 被 drop → kill_on_drop 杀掉子进程
    let out = match tokio::time::timeout(OCR_TIMEOUT, cmd.output()).await {
        Ok(r) => r.context(trf(lang, "err_ocr_spawn", &[&ocr_bin.display().to_string()]))?,
        Err(_) => anyhow::bail!("{}", tr(lang, "err_ocr_timeout")),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.chars().rev().take(500).collect::<String>().chars().rev().collect();
        anyhow::bail!(
            "{}",
            trf(lang, "err_ocr_exit", &[&format!("{:?}", out.status.code()), tail.trim()])
        );
    }

    progress(tr(lang, "prog_parse_ocr").into());
    let text = std::fs::read_to_string(tmp.0.join("result.json"))
        .context(tr(lang, "err_ocr_no_result"))?;
    let outcome = parse_ocr_result(&text, lang)?;
    progress(trf(
        lang,
        "ocr_done",
        &[&outcome.comments.len().to_string(), &outcome.files_reviewed.to_string()],
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
        let out = parse_ocr_result(text, "zh").unwrap();
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
        let out = parse_ocr_result(r#"{"status":"partial","comments":[],"summary":{"files_reviewed":2}}"#, "zh").unwrap();
        assert_eq!(out.status, "partial");
        assert_eq!(out.files_reviewed, 2);
        assert!(out.comments.is_empty());

        let out = parse_ocr_result(r#"{"status":"success","comments":[]}"#, "zh").unwrap();
        assert_eq!(out.status, "success");
        assert_eq!(out.files_reviewed, 0); // summary 缺失 → 0
    }

    #[test]
    fn parse_result_failed_errors() {
        let e = parse_ocr_result(r#"{"status":"failed","error":"LLM 连接失败","comments":[]}"#, "zh")
            .unwrap_err();
        assert!(e.to_string().contains("failed"));
        assert!(e.to_string().contains("LLM 连接失败"));
    }

    #[test]
    fn parse_result_bad_json_errors() {
        assert!(parse_ocr_result("", "zh").is_err());
        assert!(parse_ocr_result("not json", "zh").is_err());
        assert!(parse_ocr_result("[1,2]", "zh").is_err()); // 非对象
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

    /// 造一组临时目录：exe_dir、cwd、以及可选的 bin/ocr 候选文件。
    struct ResolveFixture {
        root: PathBuf,
        exe_dir: PathBuf,
        cwd: PathBuf,
    }

    impl ResolveFixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "prdesk-ocr-resolve-{tag}-{}",
                std::process::id()
            ));
            let exe_dir = root.join("exe");
            let cwd = root.join("cwd");
            std::fs::create_dir_all(&exe_dir).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            Self { root, exe_dir, cwd }
        }

        fn touch(&self, base: &Path, name: &str) -> PathBuf {
            let p = base.join("bin").join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"bin").unwrap();
            p
        }
    }

    impl Drop for ResolveFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn resolve_explicit_config_wins() {
        let fx = ResolveFixture::new("explicit");
        // 显式配置存在 → 优先，即使 sidecar 也在
        let custom = fx.root.join("custom-ocr.exe");
        std::fs::write(&custom, b"bin").unwrap();
        fx.touch(&fx.exe_dir, "ocr.exe");
        assert_eq!(
            resolve_ocr_path_in(custom.to_str().unwrap(), Some(&fx.exe_dir), &fx.cwd),
            custom
        );
        // 显式配置不存在 → 落回 sidecar
        let missing = fx.root.join("nope.exe");
        assert_eq!(
            resolve_ocr_path_in(missing.to_str().unwrap(), Some(&fx.exe_dir), &fx.cwd),
            fx.exe_dir.join("bin").join("ocr.exe")
        );
    }

    #[test]
    fn resolve_sidecar_before_cwd_and_path() {
        let fx = ResolveFixture::new("sidecar");
        // exe 旁 bin/ 优先于 cwd bin/
        let sidecar = fx.touch(&fx.exe_dir, "ocr.exe");
        fx.touch(&fx.cwd, "ocr.exe");
        assert_eq!(
            resolve_ocr_path_in("ocr", Some(&fx.exe_dir), &fx.cwd),
            sidecar
        );
        // 只有 cwd 布局 → 用 cwd 下的
        std::fs::remove_file(&sidecar).unwrap();
        assert_eq!(
            resolve_ocr_path_in("ocr", Some(&fx.exe_dir), &fx.cwd),
            fx.cwd.join("bin").join("ocr.exe")
        );
        // 裸名 ocr（非 Windows）也能命中
        std::fs::remove_file(fx.cwd.join("bin").join("ocr.exe")).unwrap();
        let bare = fx.touch(&fx.cwd, "ocr");
        assert_eq!(
            resolve_ocr_path_in("ocr", Some(&fx.exe_dir), &fx.cwd),
            bare
        );
    }

    #[test]
    fn resolve_fallback_to_path() {
        let fx = ResolveFixture::new("fallback");
        // 无任何候选文件 → 原样返回（默认 "ocr" 走 PATH）
        assert_eq!(
            resolve_ocr_path_in("ocr", Some(&fx.exe_dir), &fx.cwd),
            PathBuf::from("ocr")
        );
        assert_eq!(
            resolve_ocr_path_in("ocr", None, &fx.cwd),
            PathBuf::from("ocr")
        );
    }
}
