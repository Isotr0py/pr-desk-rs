//! 本地预评审引擎：open-code-review（ocr CLI）。
//!
//! 流程：按 PR base/head 准备可复用的完整 Git 工作树
//! → `ocr review --from <base> --to <head> --format json` → 解析 JSON 拿行级评论。
//! 评分仍由 reviewer.rs 的 LLM 调用负责，本模块只产出行级评论。
//! LLM 配置不落 ocr 全局配置：在临时目录写 config.json，子进程用
//! 环境变量 OCR_CONFIG_PATH 指向它（未配置 LLM 时不设，ocr 用自家全局配置）。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
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

struct StagingDir {
    path: PathBuf,
    publish: bool,
}

impl StagingDir {
    fn new(root: &Path) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let uniq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(".staging-{}-{nanos:x}-{uniq}", std::process::id()));
        std::fs::create_dir_all(&path).context("创建评审工作区 staging 失败")?;
        Ok(Self {
            path,
            publish: false,
        })
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.publish {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Clone)]
struct ContextIdentity {
    repo: String,
    number: i64,
    base_sha: String,
    head_sha: String,
    base_repo: String,
    head_repo: String,
}

impl ContextIdentity {
    fn manifest(&self) -> Value {
        serde_json::json!({
            "version": 2,
            "ready": true,
            "repo": self.repo,
            "number": self.number,
            "base_sha": self.base_sha,
            "head_sha": self.head_sha,
            "base_repo": self.base_repo,
            "head_repo": self.head_repo,
        })
    }
}

pub fn context_dir_for(
    repo: &str,
    number: i64,
    base_sha: &str,
    head_sha: &str,
    root: &Path,
) -> PathBuf {
    use sha2::Digest as _;
    let identity = format!("{repo}#{number}@{base_sha}..{head_sha}");
    let hash = format!("{:x}", sha2::Sha256::digest(identity.as_bytes()));
    root.join(format!("pr-desk-acp-context-v2-{}", &hash[..16]))
}

pub fn context_manifest_matches(
    repo: &str,
    number: i64,
    base_sha: &str,
    head_sha: &str,
    root: &Path,
) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("context.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value["version"].as_i64() == Some(2)
        && value["ready"].as_bool() == Some(true)
        && value["repo"].as_str() == Some(repo)
        && value["number"].as_i64() == Some(number)
        && value["base_sha"].as_str() == Some(base_sha)
        && value["head_sha"].as_str() == Some(head_sha)
}

/// 普通 OCR 与内嵌 Kimi CLI 共用的持久 Git 工作树。
#[derive(Debug)]
pub struct OcrGitSnapshot {
    path: PathBuf,
    base_sha: String,
    head_sha: String,
}

impl OcrGitSnapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn base_sha(&self) -> &str {
        &self.base_sha
    }

    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }

    pub fn context_root(&self) -> &Path {
        self.path.parent().unwrap_or(&self.path)
    }
}

async fn valid_context(path: &Path, identity: &ContextIdentity) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(manifest) = std::fs::read_to_string(path.join("context.json")) else {
        return false;
    };
    let Ok(actual) = serde_json::from_str::<Value>(&manifest) else {
        return false;
    };
    if actual != identity.manifest() || !path.join("worktree/.git").exists() {
        return false;
    }
    let worktree = path.join("worktree");
    let Ok(head) = git_output(&worktree, &["rev-parse", "HEAD"]).await else {
        return false;
    };
    if String::from_utf8_lossy(&head.stdout).trim() != identity.head_sha {
        return false;
    }
    for (reference, expected) in [
        ("refs/heads/base", &identity.base_sha),
        ("refs/heads/head", &identity.head_sha),
    ] {
        let Ok(output) = git_output(&worktree, &["rev-parse", reference]).await else {
            return false;
        };
        if String::from_utf8_lossy(&output.stdout).trim() != expected {
            return false;
        }
    }
    if git_output(&worktree, &["diff", "--quiet"]).await.is_err() {
        return false;
    }
    git_output(
        &worktree,
        &["merge-base", "refs/heads/base", "refs/heads/head"],
    )
    .await
    .is_ok_and(|output| !output.stdout.is_empty())
}

/// 在临时仓库里跑 git（失败带 stderr 尾部报错）。
async fn git_output(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .await
        .context("启动 git 失败")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(300)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        anyhow::bail!("git {:?} 失败：{}", args, tail.trim());
    }
    Ok(out)
}

async fn git(repo: &Path, args: &[&str]) -> Result<()> {
    git_output(repo, args).await.map(|_| ())
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
        anyhow::bail!(
            "{}",
            crate::i18n::trf(lang, "err_ocr_failed_status", &[msg])
        );
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

fn check_cancel(cancel: Option<&Arc<AtomicBool>>, lang: &str) -> Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        anyhow::bail!("{}", crate::i18n::tr(lang, "err_ocr_cancelled"));
    }
    Ok(())
}

fn ocr_progress_text(line: &str) -> Option<String> {
    let text = line.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(240).collect())
}

async fn run_streaming_process(
    cmd: &mut Command,
    lang: &str,
    progress: &mut (dyn FnMut(String) + Send),
    cancel: Option<Arc<AtomicBool>>,
) -> Result<()> {
    use crate::i18n::{tr, trf};
    check_cancel(cancel.as_ref(), lang)?;
    let program = cmd.as_std().get_program().to_string_lossy().to_string();
    let mut child = cmd
        .spawn()
        .context(trf(lang, "err_ocr_spawn", &[&program]))?;
    let stderr = child.stderr.take().context(tr(lang, "err_ocr_no_stderr"))?;
    let mut lines = BufReader::new(stderr).lines();
    let mut tail = VecDeque::<String>::new();
    let deadline = tokio::time::Instant::now() + OCR_TIMEOUT;
    let mut exit_status = None;
    let status = loop {
        tokio::select! {
            status = child.wait(), if exit_status.is_none() => {
                exit_status = Some(status.context(tr(lang, "err_ocr_wait"))?);
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(text) = ocr_progress_text(&line) {
                            if tail.len() == 12 {
                                tail.pop_front();
                            }
                            tail.push_back(text.clone());
                            progress(format!("OCR · {text}"));
                        }
                    }
                    Ok(None) => {
                        break match exit_status {
                            Some(status) => status,
                            None => child.wait().await.context(tr(lang, "err_ocr_wait"))?,
                        };
                    }
                    Err(e) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return Err(e).context(tr(lang, "err_ocr_read_progress"));
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                anyhow::bail!("{}", tr(lang, "err_ocr_timeout"));
            }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if cancel.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    anyhow::bail!("{}", tr(lang, "err_ocr_cancelled"));
                }
            }
        }
    };
    if !status.success() {
        let detail = tail.into_iter().collect::<Vec<_>>().join("\n");
        let detail = detail
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        match status.code() {
            Some(code) => anyhow::bail!(
                "{}",
                trf(lang, "err_ocr_exit", &[&code.to_string(), detail.trim()])
            ),
            None => anyhow::bail!("{}", trf(lang, "err_ocr_terminated", &[detail.trim()])),
        }
    }
    Ok(())
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
    progress(crate::i18n::tr(lang, "prog_fetch_pr_diff").into());
    let diff = gh.fetch_diff(repo, number).await?;
    run_ocr_review_with_diff(cfg, gh, repo, meta, &diff, progress, lang, None, None).await
}

fn git_repository_url(repository: &str) -> String {
    if repository.starts_with("file://") {
        repository.to_string()
    } else {
        format!("https://github.com/{repository}.git")
    }
}

#[derive(Clone)]
struct GitMirror {
    path: PathBuf,
    url: String,
}

fn git_mirror(root: &Path, url: &str) -> GitMirror {
    use sha2::Digest as _;
    let hash = format!("{:x}", sha2::Sha256::digest(url.as_bytes()));
    GitMirror {
        path: root
            .join(".git-mirrors")
            .join(format!("{}.git", &hash[..16])),
        url: url.to_string(),
    }
}

fn git_path(path: &Path) -> String {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = absolute.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('\\', "/")
}

fn git_mirror_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

async fn lock_git_mirror(
    mirror: &GitMirror,
    cancel: Option<&Arc<AtomicBool>>,
    lang: &str,
) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    let lock = git_mirror_lock(&mirror.path);
    check_cancel(cancel, lang)?;
    let guard = lock.lock_owned().await;
    check_cancel(cancel, lang)?;
    Ok(guard)
}

async fn git_mirror_is_shallow(mirror: &GitMirror) -> Result<bool> {
    git_output(
        mirror.path.as_path(),
        &["rev-parse", "--is-shallow-repository"],
    )
    .await
    .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "true")
}

async fn prepare_git_mirror(
    mirror: &GitMirror,
    shas: &[&str],
    cancel: Option<&Arc<AtomicBool>>,
    lang: &str,
) -> Result<()> {
    let _guard = lock_git_mirror(mirror, cancel, lang).await?;
    let parent = mirror.path.parent().context("Git mirror 缺少父目录")?;
    std::fs::create_dir_all(parent)?;
    let is_new = !mirror.path.exists();
    let mirror_path = git_path(&mirror.path);
    git(parent, &["init", "--quiet", "--bare", &mirror_path]).await?;

    let mut missing = Vec::new();
    for sha in shas {
        check_cancel(cancel, lang)?;
        let object = format!("{sha}^{{commit}}");
        if git_output(&mirror.path, &["cat-file", "-e", &object])
            .await
            .is_err()
        {
            missing.push(*sha);
        }
    }
    if missing.is_empty() {
        tracing::info!(mirror = %mirror.path.display(), "OCR Git mirror 复用已有对象");
        return Ok(());
    }

    check_cancel(cancel, lang)?;
    let shallow = is_new || git_mirror_is_shallow(mirror).await?;
    let mut args = vec!["fetch".to_string(), "--no-tags".to_string()];
    if shallow {
        args.push("--depth=64".to_string());
    }
    args.push(mirror.url.clone());
    for sha in &missing {
        args.push(format!("{sha}:refs/pr-desk/{sha}"));
    }
    tracing::info!(
        mirror = %mirror.path.display(),
        commits = missing.len(),
        mode = if shallow { "depth64" } else { "full" },
        "OCR Git mirror 下载缺失对象"
    );
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    git(&mirror.path, &args).await
}

async fn deepen_git_mirror(
    mirror: &GitMirror,
    shas: &[&str],
    cancel: Option<&Arc<AtomicBool>>,
    lang: &str,
) -> Result<()> {
    let _guard = lock_git_mirror(mirror, cancel, lang).await?;
    if !git_mirror_is_shallow(mirror).await? {
        return Ok(());
    }
    check_cancel(cancel, lang)?;
    let mut args = vec![
        "fetch".to_string(),
        "--no-tags".to_string(),
        "--deepen=256".to_string(),
        mirror.url.clone(),
    ];
    args.extend(shas.iter().map(|sha| (*sha).to_string()));
    tracing::info!(mirror = %mirror.path.display(), "OCR Git mirror deepen256");
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    git(&mirror.path, &args).await
}

async fn unshallow_git_mirror(
    mirror: &GitMirror,
    shas: &[&str],
    cancel: Option<&Arc<AtomicBool>>,
    lang: &str,
) -> Result<()> {
    let _guard = lock_git_mirror(mirror, cancel, lang).await?;
    if !git_mirror_is_shallow(mirror).await? {
        return Ok(());
    }
    check_cancel(cancel, lang)?;
    let mut args = vec![
        "fetch".to_string(),
        "--no-tags".to_string(),
        "--unshallow".to_string(),
        mirror.url.clone(),
    ];
    args.extend(shas.iter().map(|sha| (*sha).to_string()));
    tracing::info!(mirror = %mirror.path.display(), "OCR Git mirror unshallow");
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    git(&mirror.path, &args).await
}

async fn update_worktree_refs(
    worktree: &Path,
    mirror: &GitMirror,
    refs: &[(&str, &str)],
) -> Result<()> {
    for (sha, reference) in refs {
        let mirror_ref = format!("refs/pr-desk/{sha}");
        let resolved = git_output(&mirror.path, &["rev-parse", &mirror_ref]).await?;
        anyhow::ensure!(
            String::from_utf8_lossy(&resolved.stdout).trim() == *sha,
            "Git mirror 提交引用不一致"
        );
        let worktree_ref = format!("refs/heads/{reference}");
        git(worktree, &["update-ref", &worktree_ref, sha]).await?;
    }
    Ok(())
}

async fn worktree_has_merge_base(worktree: &Path) -> bool {
    git_output(
        worktree,
        &["merge-base", "refs/heads/base", "refs/heads/head"],
    )
    .await
    .is_ok()
}

pub async fn build_ocr_git_snapshot(
    _gh: &GitHub,
    repo: &str,
    meta: &Value,
    diff: &str,
    root: &Path,
    progress: &mut (dyn FnMut(String) + Send),
    lang: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<OcrGitSnapshot> {
    use crate::i18n::tr;
    let base_sha = meta["base"]["sha"].as_str().unwrap_or("").to_string();
    let head_sha = meta["head"]["sha"].as_str().unwrap_or("").to_string();
    if base_sha.is_empty() || head_sha.is_empty() {
        anyhow::bail!("{}", tr(lang, "err_missing_sha"));
    }
    if reviewer::split_diff_files(diff).is_empty() {
        anyhow::bail!("{}", tr(lang, "err_empty_diff"));
    }
    let base_repo = meta["base"]["repo"]["full_name"]
        .as_str()
        .unwrap_or(repo)
        .to_string();
    let head_repo = meta["head"]["repo"]["full_name"]
        .as_str()
        .unwrap_or(&base_repo)
        .to_string();
    let identity = ContextIdentity {
        repo: repo.to_string(),
        number: meta["number"].as_i64().unwrap_or(0),
        base_sha: base_sha.clone(),
        head_sha: head_sha.clone(),
        base_repo,
        head_repo,
    };
    let final_path = context_dir_for(repo, identity.number, &base_sha, &head_sha, root);
    let snapshot = |path: PathBuf| OcrGitSnapshot {
        path: path.join("worktree"),
        base_sha: base_sha.clone(),
        head_sha: head_sha.clone(),
    };
    if valid_context(&final_path, &identity).await {
        return Ok(snapshot(final_path));
    }
    if std::fs::symlink_metadata(&final_path).is_ok() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let backup =
            final_path.with_extension(format!("incomplete-{}-{nanos}", std::process::id()));
        std::fs::rename(&final_path, &backup).context("备份未完成的评审工作区失败")?;
    }

    check_cancel(cancel.as_ref(), lang)?;
    std::fs::create_dir_all(root)?;
    progress(if lang == "en" {
        "Preparing the full Git worktree…".into()
    } else {
        "正在准备完整 Git 工作区…".into()
    });
    let base_url = git_repository_url(&identity.base_repo);
    let head_url = git_repository_url(&identity.head_repo);
    let base_mirror = git_mirror(root, &base_url);
    let head_mirror = git_mirror(root, &head_url);
    let same_mirror = base_mirror.path == head_mirror.path;
    if same_mirror {
        prepare_git_mirror(
            &base_mirror,
            &[&identity.base_sha, &identity.head_sha],
            cancel.as_ref(),
            lang,
        )
        .await?;
    } else {
        for (mirror, sha) in [
            (&base_mirror, identity.base_sha.as_str()),
            (&head_mirror, identity.head_sha.as_str()),
        ] {
            check_cancel(cancel.as_ref(), lang)?;
            prepare_git_mirror(mirror, &[sha], cancel.as_ref(), lang).await?;
        }
    }

    let mut staging = StagingDir::new(root)?;
    let worktree = staging.path.join("worktree");
    std::fs::create_dir_all(&worktree)?;
    git(&worktree, &["init", "--quiet"]).await?;
    git(&worktree, &["config", "core.longpaths", "true"]).await?;
    git(&worktree, &["remote", "add", "origin", &base_url]).await?;
    let alternates_dir = worktree.join(".git/objects/info");
    std::fs::create_dir_all(&alternates_dir)?;
    let mirrors = if same_mirror {
        vec![&base_mirror]
    } else {
        vec![&base_mirror, &head_mirror]
    };
    let mut alternates = mirrors
        .iter()
        .map(|mirror| git_path(&mirror.path.join("objects")))
        .collect::<Vec<_>>()
        .join("\n");
    alternates.push('\n');
    std::fs::write(alternates_dir.join("alternates"), alternates)?;
    if same_mirror {
        update_worktree_refs(
            &worktree,
            &base_mirror,
            &[(&identity.base_sha, "base"), (&identity.head_sha, "head")],
        )
        .await?;
    } else {
        for (mirror, sha, reference) in [
            (&base_mirror, identity.base_sha.as_str(), "base"),
            (&head_mirror, identity.head_sha.as_str(), "head"),
        ] {
            check_cancel(cancel.as_ref(), lang)?;
            update_worktree_refs(&worktree, mirror, &[(sha, reference)]).await?;
        }
    }

    if !worktree_has_merge_base(&worktree).await {
        if same_mirror {
            deepen_git_mirror(
                &base_mirror,
                &[&identity.base_sha, &identity.head_sha],
                cancel.as_ref(),
                lang,
            )
            .await?;
        } else {
            for (mirror, sha) in [
                (&base_mirror, identity.base_sha.as_str()),
                (&head_mirror, identity.head_sha.as_str()),
            ] {
                check_cancel(cancel.as_ref(), lang)?;
                deepen_git_mirror(mirror, &[sha], cancel.as_ref(), lang).await?;
            }
        }
    }
    if !worktree_has_merge_base(&worktree).await {
        if same_mirror {
            unshallow_git_mirror(
                &base_mirror,
                &[&identity.base_sha, &identity.head_sha],
                cancel.as_ref(),
                lang,
            )
            .await?;
        } else {
            for (mirror, sha) in [
                (&base_mirror, identity.base_sha.as_str()),
                (&head_mirror, identity.head_sha.as_str()),
            ] {
                check_cancel(cancel.as_ref(), lang)?;
                unshallow_git_mirror(mirror, &[sha], cancel.as_ref(), lang).await?;
            }
        }
    }
    git_output(
        &worktree,
        &["merge-base", "refs/heads/base", "refs/heads/head"],
    )
    .await?;
    git(
        &worktree,
        &["checkout", "--quiet", "--detach", &identity.head_sha],
    )
    .await?;
    std::fs::write(
        staging.path.join("context.json"),
        serde_json::to_vec_pretty(&identity.manifest())?,
    )?;
    anyhow::ensure!(
        valid_context(&staging.path, &identity).await,
        "完整 Git 工作区校验失败"
    );

    if let Err(error) = std::fs::rename(&staging.path, &final_path) {
        if valid_context(&final_path, &identity).await {
            return Ok(snapshot(final_path));
        }
        return Err(error).context("发布完整 Git 工作区失败");
    }
    staging.publish = true;
    Ok(snapshot(final_path))
}

/// 使用调用方固定的同一份 Diff 快照运行 OCR。
pub async fn run_ocr_review_with_diff(
    cfg: &Config,
    gh: &GitHub,
    repo: &str,
    meta: &Value,
    diff: &str,
    progress: &mut (dyn FnMut(String) + Send),
    lang: &str,
    cancel: Option<Arc<AtomicBool>>,
    refresh_background: Option<&str>,
) -> Result<OcrRunOutcome> {
    use crate::i18n::{tr, trf};
    let title = meta["title"].as_str().unwrap_or("");
    let body = meta["body"].as_str().unwrap_or("");
    let mut background: String = format!("{title}\n{body}")
        .chars()
        .take(BACKGROUND_MAX_CHARS)
        .collect();
    if let Some(context) = refresh_background.filter(|s| !s.trim().is_empty()) {
        background.push_str("\n\n<review-discussion>\n");
        background.push_str(context);
        background.push_str("\n</review-discussion>");
    }

    let snapshot = build_ocr_git_snapshot(
        gh,
        repo,
        meta,
        diff,
        &cfg.kimi_workspace_root_path(),
        progress,
        lang,
        cancel.clone(),
    )
    .await?;

    // LLM 配置：经环境变量传给 ocr 子进程（OCR_CONFIG_PATH 只对 ocr 的 config 子命令生效，
    // review 的 LLM resolver 只认 OCR_LLM_* 环境变量和 ~/.opencodereview/config.json——后者不动）。
    // 未配置 LLM 时不设这些变量，ocr 用自家全局配置。
    let ocr_bin = resolve_ocr_path(cfg);
    let mut cmd = Command::new(&ocr_bin);
    cmd.arg("review")
        .arg("--repo")
        .arg(snapshot.path())
        .arg("--from")
        .arg(snapshot.base_sha())
        .arg("--to")
        .arg(snapshot.head_sha())
        .arg("--format")
        .arg("json")
        .arg("--audience")
        .arg("human")
        .arg("--output")
        .arg(snapshot.context_root().join("result.json"))
        .arg("--background")
        .arg(&background)
        .current_dir(snapshot.path())
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
    run_streaming_process(&mut cmd, lang, progress, cancel).await?;

    progress(tr(lang, "prog_parse_ocr").into());
    let text = std::fs::read_to_string(snapshot.context_root().join("result.json"))
        .context(tr(lang, "err_ocr_no_result"))?;
    let outcome = parse_ocr_result(&text, lang)?;
    progress(trf(
        lang,
        "ocr_done",
        &[
            &outcome.comments.len().to_string(),
            &outcome.files_reviewed.to_string(),
        ],
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
        assert_eq!(
            out.comments[0]["suggestion_code"].as_str(),
            Some("x.checked_add(1)")
        );
        assert_eq!(out.comments[1]["severity"].as_str(), Some("low"));
    }

    #[test]
    fn parse_result_partial_and_success_ok() {
        let out = parse_ocr_result(
            r#"{"status":"partial","comments":[],"summary":{"files_reviewed":2}}"#,
            "zh",
        )
        .unwrap();
        assert_eq!(out.status, "partial");
        assert_eq!(out.files_reviewed, 2);
        assert!(out.comments.is_empty());

        let out = parse_ocr_result(r#"{"status":"success","comments":[]}"#, "zh").unwrap();
        assert_eq!(out.status, "success");
        assert_eq!(out.files_reviewed, 0); // summary 缺失 → 0
    }

    #[test]
    fn parse_result_failed_errors() {
        let e = parse_ocr_result(
            r#"{"status":"failed","error":"LLM 连接失败","comments":[]}"#,
            "zh",
        )
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

    /// 造一组临时目录：exe_dir、cwd、以及可选的 bin/ocr 候选文件。
    struct ResolveFixture {
        root: PathBuf,
        exe_dir: PathBuf,
        cwd: PathBuf,
    }

    impl ResolveFixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("prdesk-ocr-resolve-{tag}-{}", std::process::id()));
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
        assert_eq!(resolve_ocr_path_in("ocr", Some(&fx.exe_dir), &fx.cwd), bare);
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

    #[test]
    fn progress_text_trims_and_bounds_lines() {
        assert_eq!(
            ocr_progress_text("  file-a.rs  \r"),
            Some("file-a.rs".into())
        );
        assert_eq!(ocr_progress_text(" \r\n"), None);
        assert_eq!(
            ocr_progress_text(&"x".repeat(300)).unwrap().chars().count(),
            240
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn streaming_process_forwards_stderr_lines() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "echo first-line 1>&2& echo second-line 1>&2"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut seen = Vec::new();
        let mut progress = |text: String| seen.push(text);
        run_streaming_process(&mut cmd, "zh", &mut progress, None)
            .await
            .unwrap();
        assert!(seen.iter().any(|line| line.contains("first-line")));
        assert!(seen.iter().any(|line| line.contains("second-line")));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn streaming_process_cancel_kills_child_promptly() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let flag = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let mut progress = |_| {};
        let err = run_streaming_process(&mut cmd, "zh", &mut progress, Some(cancel))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.to_string().contains("取消"));
    }

    fn test_git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn test_file_url(path: &Path) -> String {
        let raw = path.canonicalize().unwrap();
        let raw = raw.to_string_lossy();
        let raw = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
        let path = raw.replace('\\', "/");
        if cfg!(windows) {
            format!("file:///{path}")
        } else {
            format!("file://{path}")
        }
    }

    fn assert_shared_mirrors(snapshot: &OcrGitSnapshot, root: &Path, urls: &[&str]) {
        let expected = urls
            .iter()
            .map(|url| git_path(&git_mirror(root, url).path.join("objects")))
            .collect::<Vec<_>>();
        let alternates =
            std::fs::read_to_string(snapshot.path().join(".git/objects/info/alternates")).unwrap();
        assert_eq!(
            alternates.lines().collect::<Vec<_>>(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(
            std::fs::read_dir(snapshot.path().join(".git/objects/pack"))
                .unwrap()
                .count(),
            0,
            "工作树不应复制 mirror 的 pack 对象"
        );
        assert_eq!(
            test_git(snapshot.path(), &["rev-parse", "--is-shallow-repository"]),
            "false"
        );
    }

    #[tokio::test]
    async fn snapshot_is_a_complete_persistent_range_worktree() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("pr-desk-git-test-{}-{unique}", std::process::id()));
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        test_git(&seed, &["init", "--quiet"]);
        std::fs::create_dir_all(seed.join("src")).unwrap();
        std::fs::write(seed.join("src/lib.rs"), "fn changed() { 1 }\n").unwrap();
        std::fs::write(seed.join("src/unchanged.rs"), "fn untouched() { 42 }\n").unwrap();
        test_git(&seed, &["add", "."]);
        test_git(
            &seed,
            &[
                "-c",
                "user.email=test@local",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        let base_sha = test_git(&seed, &["rev-parse", "HEAD"]);
        for index in 0..321 {
            test_git(
                &seed,
                &[
                    "-c",
                    "user.email=test@local",
                    "-c",
                    "user.name=test",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    &format!("history {index}"),
                ],
            );
        }
        std::fs::write(seed.join("src/lib.rs"), "fn changed() { 2 }\n").unwrap();
        test_git(&seed, &["add", "."]);
        test_git(
            &seed,
            &[
                "-c",
                "user.email=test@local",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "head",
            ],
        );
        let head_sha = test_git(&seed, &["rev-parse", "HEAD"]);

        test_git(&root, &["init", "--quiet", "--bare", "base.git"]);
        test_git(&root, &["init", "--quiet", "--bare", "fork.git"]);
        let base_url = test_file_url(&root.join("base.git"));
        let head_url = test_file_url(&root.join("fork.git"));
        test_git(&seed, &["remote", "add", "base", &base_url]);
        test_git(&seed, &["remote", "add", "head", &head_url]);
        test_git(&seed, &["push", "--quiet", "base", "HEAD:main"]);
        test_git(&seed, &["push", "--quiet", "head", "HEAD:main"]);

        let meta = serde_json::json!({
            "number": 7,
            "title": "test",
            "body": "body",
            "base": {"sha": base_sha, "repo": {"full_name": base_url}},
            "head": {"sha": head_sha, "repo": {"full_name": head_url}},
        });
        let workspace_root = root.join("workspaces");
        let gh = GitHub::new(None, "").unwrap();
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new";
        let snapshot = build_ocr_git_snapshot(
            &gh,
            "owner/repo",
            &meta,
            diff,
            &workspace_root,
            &mut |_| {},
            "zh",
            None,
        )
        .await
        .unwrap();
        assert!(snapshot.path().join("src/unchanged.rs").is_file());
        assert!(snapshot.path().join("src/lib.rs").is_file());
        assert_eq!(test_git(snapshot.path(), &["rev-parse", "HEAD"]), head_sha);
        assert_eq!(
            test_git(
                snapshot.path(),
                &["diff", "--name-only", "refs/heads/base", "refs/heads/head"]
            ),
            "src/lib.rs"
        );
        assert_shared_mirrors(&snapshot, &workspace_root, &[&base_url, &head_url]);
        assert_eq!(
            test_git(snapshot.path(), &["remote", "get-url", "origin"]),
            base_url
        );
        for (url, sha) in [(&base_url, &base_sha), (&head_url, &head_sha)] {
            let mirror = git_mirror(&workspace_root, url);
            let reference = format!("refs/pr-desk/{sha}");
            assert_eq!(test_git(&mirror.path, &["rev-parse", &reference]), *sha);
        }
        test_git(
            snapshot.path(),
            &["symbolic-ref", "HEAD", "refs/heads/pr-desk"],
        );
        let snapshot_path = snapshot.path().to_path_buf();
        drop(snapshot);

        let persistent = build_ocr_git_snapshot(
            &gh,
            "owner/repo",
            &meta,
            diff,
            &workspace_root,
            &mut |_| {},
            "zh",
            None,
        )
        .await
        .unwrap();
        assert_eq!(persistent.path(), snapshot_path);
        drop(persistent);

        let same_repo_root = root.join("same-repo-workspaces");
        let mut same_repo_meta = meta.clone();
        same_repo_meta["head"]["repo"]["full_name"] = serde_json::Value::String(base_url.clone());
        let same_repo_snapshot = build_ocr_git_snapshot(
            &gh,
            "owner/repo",
            &same_repo_meta,
            diff,
            &same_repo_root,
            &mut |_| {},
            "zh",
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            test_git(same_repo_snapshot.path(), &["rev-parse", "HEAD"]),
            head_sha
        );
        assert_shared_mirrors(&same_repo_snapshot, &same_repo_root, &[&base_url]);
        assert_eq!(
            test_git(same_repo_snapshot.path(), &["merge-base", "base", "head"]),
            base_sha
        );
        drop(same_repo_snapshot);

        let concurrent_root = root.join("concurrent-workspaces");
        let mut first_meta = meta.clone();
        first_meta["number"] = serde_json::json!(9);
        let mut second_meta = meta.clone();
        second_meta["number"] = serde_json::json!(10);
        let mut first_progress = |_| {};
        let mut second_progress = |_| {};
        let (first, second) = tokio::join!(
            build_ocr_git_snapshot(
                &gh,
                "owner/repo",
                &first_meta,
                diff,
                &concurrent_root,
                &mut first_progress,
                "zh",
                None,
            ),
            build_ocr_git_snapshot(
                &gh,
                "owner/repo",
                &second_meta,
                diff,
                &concurrent_root,
                &mut second_progress,
                "zh",
                None,
            )
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first.path(), second.path());
        assert_shared_mirrors(&first, &concurrent_root, &[&base_url, &head_url]);
        assert_shared_mirrors(&second, &concurrent_root, &[&base_url, &head_url]);
        drop(first);
        drop(second);

        std::fs::remove_dir_all(root.join("base.git")).unwrap();
        std::fs::remove_dir_all(root.join("fork.git")).unwrap();
        let mut offline_meta = meta.clone();
        offline_meta["number"] = serde_json::json!(11);
        let offline = build_ocr_git_snapshot(
            &gh,
            "owner/repo",
            &offline_meta,
            diff,
            &workspace_root,
            &mut |_| {},
            "zh",
            None,
        )
        .await
        .unwrap();
        assert_ne!(offline.path(), snapshot_path);
        assert_shared_mirrors(&offline, &workspace_root, &[&base_url, &head_url]);
        assert_eq!(
            test_git(offline.path(), &["merge-base", "base", "head"]),
            base_sha
        );
        std::fs::write(snapshot_path.join("src/lib.rs"), "first worktree\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(offline.path().join("src/lib.rs"))
                .unwrap()
                .replace("\r\n", "\n"),
            "fn changed() { 2 }\n"
        );
        std::fs::write(offline.path().join("src/unchanged.rs"), "second worktree\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(snapshot_path.join("src/unchanged.rs"))
                .unwrap()
                .replace("\r\n", "\n"),
            "fn untouched() { 42 }\n"
        );
        drop(offline);

        let invalid_root = root.join("invalid");
        let invalid_path = context_dir_for("owner/repo", 7, &base_sha, &head_sha, &invalid_root);
        std::fs::create_dir_all(&invalid_root).unwrap();
        std::fs::write(&invalid_path, b"not a directory").unwrap();
        let error = build_ocr_git_snapshot(
            &gh,
            "owner/repo",
            &meta,
            diff,
            &invalid_root,
            &mut |_| {},
            "zh",
            None,
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        let backups = std::fs::read_dir(&invalid_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("incomplete-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert!(backups[0].is_file());
        assert!(!invalid_path.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
