use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpEvent {
    AssistantText(String),
    Progress(String),
    PermissionDenied(String),
}

#[derive(Debug)]
pub struct AcpOutcome {
    pub session_id: String,
    pub text: String,
    pub stop_reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct KimiAcpOptions {
    pub env: Vec<(String, String)>,
}

impl KimiAcpOptions {
    #[cfg(test)]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

pub struct KimiAcpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    session_id: Option<String>,
    current_text: String,
    event_tx: Option<mpsc::Sender<AcpEvent>>,
}

fn message_chunk_text(update: &Value) -> &str {
    update["content"]["text"].as_str().unwrap_or("")
}

fn progress_text(update: &Value) -> String {
    let title = update["title"]
        .as_str()
        .or_else(|| update["name"].as_str())
        .or_else(|| update["toolCallId"].as_str())
        .unwrap_or(update["type"].as_str().unwrap_or("tool"));
    let status = update["status"].as_str().unwrap_or("");
    if status.is_empty() {
        title.to_string()
    } else {
        format!("{title} · {status}")
    }
}

impl KimiAcpClient {
    pub async fn spawn(cli_path: &str, event_tx: Option<mpsc::Sender<AcpEvent>>) -> Result<Self> {
        Self::spawn_with_options(cli_path, event_tx, KimiAcpOptions::default()).await
    }

    pub async fn spawn_with_options(
        cli_path: &str,
        event_tx: Option<mpsc::Sender<AcpEvent>>,
        options: KimiAcpOptions,
    ) -> Result<Self> {
        let mut command = Command::new(cli_path);
        command
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in options.env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("无法启动 Kimi CLI：{cli_path}"))?;
        let stdin = child.stdin.take().context("无法打开 Kimi ACP stdin")?;
        let stdout = child.stdout.take().context("无法打开 Kimi ACP stdout")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("kimi acp: {line}");
                }
            });
        }
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            session_id: None,
            current_text: String::new(),
            event_tx,
        };
        let result = client
            .call(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false,
                    }
                }),
            )
            .await?;
        anyhow::ensure!(result["protocolVersion"].as_i64() == Some(1), "Kimi ACP 协议版本不兼容");
        Ok(client)
    }

    fn emit(&self, event: AcpEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    fn handle_update(&mut self, params: &Value) {
        let session_id = params["sessionId"].as_str().unwrap_or("");
        if self.session_id.as_deref().is_some_and(|current| current != session_id) {
            return;
        }
        let update = &params["update"];
        match update["sessionUpdate"]
            .as_str()
            .or_else(|| update["type"].as_str())
            .unwrap_or("") {
            "agent_message_chunk" => {
                let text = message_chunk_text(update);
                if !text.is_empty() {
                    self.current_text.push_str(text);
                    self.emit(AcpEvent::AssistantText(text.to_string()));
                }
            }
            "tool_call" | "tool_call_update" | "plan" => {
                self.emit(AcpEvent::Progress(progress_text(update)));
            }
            _ => {}
        }
    }

    async fn write(&mut self, value: Value) -> Result<()> {
        let bytes = serde_json::to_vec(&value)?;
        self.stdin.write_all(&bytes).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    fn is_read_only_permission(params: &Value) -> bool {
        let title = params["toolCall"]["title"].as_str().unwrap_or("").to_lowercase();
        if title.contains("read") || title.contains("grep") || title.contains("glob") || title.contains("search") {
            return true;
        }
        let command = params["toolCall"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part["content"]["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let command = command
            .split("Running: ")
            .last()
            .unwrap_or(&command)
            .trim();
        if command.is_empty() || command.contains([';', '&', '|', '>', '<', '\n', '\r']) {
            return false;
        }
        if matches!(command, "pwd" | "ls" | "dir") {
            return true;
        }
        [
            "cat ", "type ", "rg ", "grep ", "findstr ", "ls ", "dir ",
            "git show", "git diff", "git log", "git status", "git rev-parse", "git ls-files",
        ]
        .iter()
        .any(|prefix| command.starts_with(prefix))
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).await?;
            if read == 0 {
                anyhow::bail!("Kimi ACP 在 {method} 期间退出");
            }
            let value: Value = serde_json::from_str(line.trim())
                .with_context(|| format!("Kimi ACP 返回非 JSON 行：{}", line.trim()))?;
            if value["method"].as_str() == Some("session/update") {
                self.handle_update(&value["params"]);
                continue;
            }
            if value["method"].as_str() == Some("session/request_permission") {
                let request_id = value["id"].clone();
                let title = value["params"]["toolCall"]["title"].as_str().unwrap_or("工具调用");
                let outcome = if Self::is_read_only_permission(&value["params"]) {
                    "allow_once"
                } else {
                    "reject_once"
                };
                let option_id = if outcome == "allow_once" { "approve_once" } else { "reject" };
                self.write(json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {"outcome": {"outcome": outcome, "optionId": option_id}}
                }))
                .await?;
                if outcome == "reject_once" {
                    self.emit(AcpEvent::PermissionDenied(title.to_string()));
                }
                continue;
            }
            if value["id"].as_u64() != Some(id) {
                continue;
            }
            if value.get("error").is_some() {
                anyhow::bail!("Kimi ACP {method} 失败：{}", value["error"]["message"].as_str().unwrap_or("未知错误"));
            }
            return Ok(value["result"].clone());
        }
    }

    pub async fn new_session(&mut self, cwd: &Path) -> Result<String> {
        let result = self
            .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let session_id = result["sessionId"]
            .as_str()
            .context("Kimi ACP 未返回 sessionId")?
            .to_string();
        self.session_id = Some(session_id.clone());
        Ok(session_id)
    }

    pub async fn resume_session(&mut self, session_id: &str, cwd: &Path) -> Result<String> {
        self.call("session/resume", json!({"sessionId": session_id, "cwd": cwd})).await?;
        self.session_id = Some(session_id.to_string());
        Ok(session_id.to_string())
    }

    pub async fn prompt(&mut self, text: &str) -> Result<AcpOutcome> {
        let session_id = self.session_id.clone().context("尚未建立 Kimi ACP 会话")?;
        self.current_text.clear();
        let result = self
            .call(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}],
                }),
            )
            .await?;
        Ok(AcpOutcome {
            session_id,
            text: self.current_text.trim().to_string(),
            stop_reason: result["stopReason"].as_str().unwrap_or("").to_string(),
        })
    }

    pub async fn close(&mut self) -> Result<()> {
        let Some(session_id) = self.session_id.take() else {
            return Ok(());
        };
        self.call("session/close", json!({"sessionId": session_id})).await?;
        Ok(())
    }
}

impl Drop for KimiAcpClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
pub mod test_support {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    const FAKE_ACP_SOURCE: &str = r#"
use std::io::{self, BufRead, Read, Write};

fn value_start(document: &str, field: &str) -> Option<usize> {
    let needle = format!("\"{field}\"");
    let bytes = document.as_bytes();
    let mut search_from = 0;
    while let Some(found) = document[search_from..].find(&needle) {
        let key_start = search_from + found;
        let after_key = key_start + needle.len();
        let escaped = key_start > 0 && bytes[key_start - 1] == b'\\';
        if !escaped {
            let mut index = after_key;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            if bytes.get(index) == Some(&b':') {
                index += 1;
                while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                    index += 1;
                }
                return Some(index);
            }
        }
        search_from = after_key;
    }
    None
}

fn hex4(chars: &mut std::str::Chars<'_>) -> Option<u32> {
    let mut value = 0;
    for _ in 0..4 {
        value = value * 16 + chars.next()?.to_digit(16)?;
    }
    Some(value)
}

fn string_at(document: &str, start: usize) -> Option<String> {
    if document.as_bytes().get(start) != Some(&b'"') {
        return None;
    }
    let mut chars = document[start + 1..].chars();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(output),
            '\\' => match chars.next()? {
                '"' => output.push('"'),
                '\\' => output.push('\\'),
                '/' => output.push('/'),
                'b' => output.push('\u{0008}'),
                'f' => output.push('\u{000c}'),
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                'u' => {
                    let high = hex4(&mut chars)?;
                    let scalar = match high {
                        0xd800..=0xdbff => {
                            if chars.next() != Some('\\') || chars.next() != Some('u') {
                                return None;
                            }
                            let low = hex4(&mut chars)?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return None;
                            }
                            0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                        }
                        0xdc00..=0xdfff => return None,
                        _ => high,
                    };
                    output.push(char::from_u32(scalar)?);
                }
                _ => return None,
            },
            _ => output.push(ch),
        }
    }
    None
}

fn string_field(document: &str, field: &str) -> Option<String> {
    string_at(document, value_start(document, field)?)
}

fn id_field(document: &str) -> Option<u64> {
    let start = value_start(document, "id")?;
    let digits: String = document[start..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .map(char::from)
        .collect();
    digits.parse().ok()
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{0008}' => output.push_str("\\b"),
            '\u{000c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch <= '\u{001f}' => output.push_str(&format!("\\u{:04x}", ch as u32)),
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn forward_prompt(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let base_url = std::env::var("PR_DESK_TEST_ACP_OPENAI_BASE_URL")?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let rest = url.strip_prefix("http://").ok_or("fake ACP only supports local HTTP")?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let body = format!(
        "{{\"model\":\"test-model\",\"messages\":[{{\"role\":\"system\",\"content\":\"You are a helpful assistant.\"}},{{\"role\":\"user\",\"content\":{}}}]}}",
        json_string(prompt)
    );
    let mut stream = std::net::TcpStream::connect(authority)?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer sk-test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    if !response.starts_with(b"HTTP/1.1 200") {
        return Err("fake OpenAI server returned a non-200 response".into());
    }
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
        .ok_or("fake OpenAI response has no header terminator")?;
    let body = std::str::from_utf8(&response[header_end..])?;
    string_field(body, "content").ok_or_else(|| "fake OpenAI response has no content".into())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut active_session: Option<String> = None;
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let id = id_field(&line).ok_or("ACP request has no numeric id")?;
        let method = string_field(&line, "method").ok_or("ACP request has no method")?;
        let result = match method.as_str() {
            "initialize" => "{\"protocolVersion\":1,\"agentCapabilities\":{}}".to_string(),
            "session/new" => {
                let session_id = format!("fake-acp-session-{}", std::process::id());
                active_session = Some(session_id.clone());
                format!("{{\"sessionId\":{}}}", json_string(&session_id))
            }
            "session/resume" => {
                let session_id = string_field(&line, "sessionId")
                    .ok_or("session/resume has no sessionId")?;
                active_session = Some(session_id.clone());
                format!("{{\"sessionId\":{}}}", json_string(&session_id))
            }
            "session/prompt" => {
                let session_id = string_field(&line, "sessionId")
                    .ok_or("session/prompt has no sessionId")?;
                if active_session.as_deref() != Some(session_id.as_str()) {
                    return Err("session/prompt used an unknown session".into());
                }
                let prompt = string_field(&line, "text").ok_or("session/prompt has no text")?;
                let answer = forward_prompt(&prompt)?;
                writeln!(
                    output,
                    "{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":{},\"update\":{{\"type\":\"agent_message_chunk\",\"content\":{{\"type\":\"text\",\"text\":{}}}}}}}}}",
                    json_string(&session_id),
                    json_string(&answer)
                )?;
                output.flush()?;
                "{\"stopReason\":\"end_turn\"}".to_string()
            }
            "session/cancel" => "{}".to_string(),
            "session/close" => {
                active_session = None;
                "{}".to_string()
            }
            _ => {
                writeln!(
                    output,
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":{}}}}}",
                    json_string(&format!("unsupported method: {method}"))
                )?;
                output.flush()?;
                continue;
            }
        };
        writeln!(
            output,
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}"
        )?;
        output.flush()?;
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fake ACP failed: {error}");
        std::process::exit(1);
    }
}
"#;

    fn compile_fake_acp() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pr-desk-fake-acp-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("fake_acp.rs");
        let executable = dir.join(format!("fake-acp{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&source, FAKE_ACP_SOURCE).unwrap();
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .arg("--edition=2021")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to compile fake ACP: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        executable
    }

    pub fn fake_acp_path() -> &'static Path {
        static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
        EXECUTABLE.get_or_init(compile_fake_acp).as_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_parsing_covers_chunks_and_tools() {
        let chunk = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello"}});
        assert_eq!(message_chunk_text(&chunk), "hello");
        let tool = json!({"sessionUpdate": "tool_call", "title": "Read file", "status": "running"});
        assert_eq!(progress_text(&tool), "Read file · running");
        let unknown = json!({"type": "other_event"});
        assert_eq!(message_chunk_text(&unknown), "");
    }

    #[test]
    fn permission_allows_only_read_only_commands() {
        fn permission(title: &str, command: &str) -> Value {
            json!({
                "toolCall": {
                    "title": title,
                    "content": [
                        {"content": {"type": "text", "text": command}}
                    ]
                }
            })
        }
        let show = permission("Bash", "git show HEAD:src/a.rs");
        let read = permission("Read file", "");
        let diff = permission("Bash", "git diff --stat");
        let chained = permission("Bash", "git show HEAD:src/a.rs && rm -rf /tmp/x");
        let remove = permission("Bash", "rm -rf /tmp/x");
        assert!(KimiAcpClient::is_read_only_permission(&show));
        assert!(KimiAcpClient::is_read_only_permission(&read));
        assert!(KimiAcpClient::is_read_only_permission(&diff));
        assert!(!KimiAcpClient::is_read_only_permission(&chained));
        assert!(!KimiAcpClient::is_read_only_permission(&remove));
    }
}
