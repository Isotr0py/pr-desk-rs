use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use egui_xterm::{TerminalConfig, TerminalResponse, TerminalRuntime};
use egui_xterm_pty::{PtyBackend, PtyConfig, TerminalInput, TerminalSize};
use pr_desk_rs::config::Config;

#[derive(Clone, Debug, Default)]
pub struct PrContext {
    pub repo: String,
    pub number: i64,
    pub base_sha: String,
    pub head_sha: String,
    pub title: String,
    pub body: String,
    pub diff: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KimiKey {
    pub profile: String,
    pub repo: String,
    pub number: i64,
    pub request_id: u64,
}

pub fn prepare_context_dir(context: &PrContext, root: &Path) -> Result<PathBuf> {
    let path = pr_desk_rs::ocr::context_dir_for(
        &context.repo,
        context.number,
        &context.base_sha,
        &context.head_sha,
        root,
    );
    let repository = path.join("repository");
    let mirror = pr_desk_rs::ocr::repository_mirror_path(root, &context.repo);
    pr_desk_rs::ocr::write_repository_gitdir(&repository, &mirror)
        .context("创建 PR 问答只读上下文失败")?;
    std::fs::write(repository.join("pr-body.md"), &context.body)?;
    std::fs::write(repository.join("pr.diff"), &context.diff)?;
    let remote = format!("pr-desk-pr-{}", context.number);
    std::fs::write(
        repository.join("pr.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "repo": context.repo,
            "number": context.number,
            "base_sha": context.base_sha,
            "head_sha": context.head_sha,
            "title": context.title,
            "pull_ref": format!("refs/pull/{}/head", context.number),
            "remote": remote,
        }))?,
    )?;
    std::fs::write(
        repository.join("AGENTS.md"),
        format!(
            "# Read-only PR context\n\n\
             This directory has no source checkout. Do not edit files, checkout branches, commit, pull, or merge.\n\
             Read `base_sha` and `head_sha` from `pr.json`; the PR body and diff are in `pr-body.md` and `pr.diff`.\n\n\
             - Read a file: `git show <sha>:path`\n\
             - Search a revision: `git grep <pattern> <sha>`\n\
             - Compare the PR: `git diff <base_sha> <head_sha>`\n\
             - Find history: `git merge-base <base_sha> <head_sha>`\n\n\
             To update only this PR, run `git fetch {remote} refs/pull/{}/head`.\n\
             Do not use `git pull` or merge another branch. Other PRs use different remotes and fetch refspecs.\n",
            context.number
        ),
    )?;
    let _ = std::fs::remove_file(path.join("findings.json"));
    Ok(repository)
}

pub struct KimiTerminal {
    key: KimiKey,
    runtime: TerminalRuntime<PtyBackend>,
    last_response: Option<TerminalResponse>,
}

impl KimiTerminal {
    pub fn spawn(
        key: KimiKey,
        cfg: &Config,
        context: &PrContext,
        ctx: &egui::Context,
        fresh: bool,
    ) -> Result<Self> {
        let cwd = prepare_context_dir(context, &cfg.kimi_workspace_root_path())?;
        let args = if fresh {
            Vec::new()
        } else {
            vec!["--continue".into()]
        };
        let repaint = ctx.clone();
        let backend = PtyBackend::spawn_with_repaint(
            PtyConfig {
                shell: Some(cfg.kimi_cli_path.clone()),
                args,
                cwd: Some(cwd),
                env: Vec::new(),
                initial_size: TerminalSize::new(100, 32),
            },
            Box::new(move || repaint.request_repaint()),
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let mut config = TerminalConfig::default();
        config.theme = crate::theme::terminal_theme(&cfg.theme);
        config.background_opacity = crate::theme::terminal_background_opacity(&cfg.theme);
        Ok(Self {
            key,
            runtime: TerminalRuntime::with_config(backend, config),
            last_response: None,
        })
    }

    pub fn key(&self) -> &KimiKey {
        &self.key
    }

    pub fn is_closed(&self) -> bool {
        self.runtime.input().is_closed()
    }

    pub fn show(&mut self, ui: &mut egui::Ui, theme: &str) {
        let config = self.runtime.config_mut();
        config.theme = crate::theme::terminal_theme(theme);
        config.background_opacity = crate::theme::terminal_background_opacity(theme);
        self.last_response = Some(self.runtime.show(ui));
    }

    pub fn send_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.runtime
            .input_mut()
            .write_bytes(bytes)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }

    pub fn dimensions(&self) -> Option<(u16, u16)> {
        self.last_response
            .as_ref()
            .map(|response| (response.cols, response.rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_dir_is_stable_lightweight_and_session_scoped() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pr-desk-workspace-test-{}-{unique}",
            std::process::id()
        ));
        let mut context = PrContext {
            repo: "owner/repo".into(),
            number: 42,
            base_sha: "base-1".into(),
            head_sha: "head-1".into(),
            body: "PR body".into(),
            diff: "diff --git a/src/lib.rs b/src/lib.rs".into(),
            ..Default::default()
        };
        let first = prepare_context_dir(&context, &root).unwrap();
        std::fs::write(first.parent().unwrap().join("findings.json"), b"[]").unwrap();
        let second = prepare_context_dir(&context, &root).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with(&root));
        assert_eq!(first.file_name().unwrap(), "repository");
        assert!(first.join(".git").is_file());
        let pointer = std::fs::read_to_string(first.join(".git")).unwrap();
        let pointer = pointer.strip_prefix("gitdir: ").unwrap().trim();
        assert!(Path::new(pointer).is_absolute());
        let mirror = pr_desk_rs::ocr::repository_mirror_path(&root, &context.repo);
        assert_eq!(Path::new(pointer), std::path::absolute(mirror).unwrap());
        assert!(!first.join("src").exists());
        assert!(!first.join("objects").exists());
        assert!(!first.join("worktree").exists());
        assert!(!first.parent().unwrap().join("worktree").exists());
        let context_root = first.parent().unwrap();
        assert_eq!(
            std::fs::read_to_string(first.join("pr-body.md")).unwrap(),
            "PR body"
        );
        assert!(first.join("pr.diff").is_file());
        assert!(first.join("pr.json").is_file());
        assert!(!context_root.join("pr-body.md").exists());
        assert!(!context_root.join("findings.json").exists());
        let pr_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(first.join("pr.json")).unwrap()).unwrap();
        assert_eq!(pr_json["pull_ref"].as_str(), Some("refs/pull/42/head"));
        assert_eq!(pr_json["remote"].as_str(), Some("pr-desk-pr-42"));
        let agents = std::fs::read_to_string(first.join("AGENTS.md")).unwrap();
        assert!(agents.contains("git show <sha>:path"));
        assert!(agents.contains("git grep <pattern> <sha>"));
        assert!(agents.contains("git diff <base_sha> <head_sha>"));
        assert!(agents.contains("git fetch pr-desk-pr-42 refs/pull/42/head"));
        assert!(agents.contains("Do not use `git pull`"));

        context.number = 43;
        context.body = "other PR".into();
        let other = prepare_context_dir(&context, &root).unwrap();
        assert_ne!(other, first);
        assert_eq!(
            std::fs::read_to_string(other.join("pr-body.md")).unwrap(),
            "other PR"
        );
        assert_eq!(
            std::fs::read_to_string(first.join("pr-body.md")).unwrap(),
            "PR body"
        );
        context.head_sha = "head-2".into();
        assert_ne!(prepare_context_dir(&context, &root).unwrap(), other);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn conpty_reads_real_process_output() {
        #[cfg(windows)]
        let (shell, args) = (
            "cmd",
            vec!["/C".into(), "echo PR_DESK_PTY_OK && exit 0".into()],
        );
        #[cfg(not(windows))]
        let (shell, args) = (
            "sh",
            vec!["-c".into(), "echo PR_DESK_PTY_OK; exit 0".into()],
        );
        let mut backend = PtyBackend::spawn(PtyConfig {
            shell: Some(shell.into()),
            args,
            cwd: None,
            env: Vec::new(),
            initial_size: TerminalSize::new(80, 24),
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            match backend.poll_read(&mut buffer) {
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(_) => break,
            }
            if String::from_utf8_lossy(&output).contains("PR_DESK_PTY_OK") && backend.is_closed() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(String::from_utf8_lossy(&output).contains("PR_DESK_PTY_OK"));
    }

    #[test]
    fn conpty_writes_real_process_input() {
        #[cfg(windows)]
        let (shell, args) = ("cmd", vec!["/Q".into()]);
        #[cfg(not(windows))]
        let (shell, args) = ("sh", vec!["-i".into()]);
        let mut backend = PtyBackend::spawn(PtyConfig {
            shell: Some(shell.into()),
            args,
            cwd: None,
            env: Vec::new(),
            initial_size: TerminalSize::new(80, 24),
        })
        .unwrap();
        backend.write_bytes(b"echo PR_DESK_INPUT_OK\r").unwrap();
        backend.write_bytes(b"exit\r").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            match backend.poll_read(&mut buffer) {
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(_) => break,
            }
            if String::from_utf8_lossy(&output)
                .matches("PR_DESK_INPUT_OK")
                .count()
                >= 2
                && backend.is_closed()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output = String::from_utf8_lossy(&output);
        assert!(output.matches("PR_DESK_INPUT_OK").count() >= 2, "{output}");
    }
}
