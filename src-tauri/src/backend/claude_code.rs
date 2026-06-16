use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

use crate::backend::events::{AppServerEvent, EventSink};
use crate::types::WorkspaceEntry;

/// Claude Code session that manages communication with the `claude` CLI
pub(crate) struct ClaudeCodeSession {
    pub(crate) entry: WorkspaceEntry,
    pub(crate) child: Mutex<Child>,
    pub(crate) stdin: Mutex<ChildStdin>,
    pub(crate) session_id: Mutex<Option<String>>,
    /// Channel for receiving responses (for request/response patterns)
    pub(crate) pending_result: Mutex<Option<oneshot::Sender<Value>>>,
    /// Callbacks for background operations - events for these are sent through the channel
    pub(crate) background_callbacks: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
}

impl ClaudeCodeSession {
    /// Send a user message to the Claude Code process
    pub(crate) async fn send_user_message(&self, content: &str) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let message = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": content
            }
        });
        let mut line = serde_json::to_string(&message).map_err(|e| e.to_string())?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    /// Send a control message (like abort/interrupt)
    pub(crate) async fn send_control(&self, subtype: &str) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let message = json!({
            "type": "control",
            "subtype": subtype
        });
        let mut line = serde_json::to_string(&message).map_err(|e| e.to_string())?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    /// Get the current session ID
    pub(crate) async fn get_session_id(&self) -> Option<String> {
        self.session_id.lock().await.clone()
    }
}

/// Build the PATH environment variable for finding the claude CLI
pub(crate) fn build_claude_path_env(claude_bin: Option<&str>) -> Option<String> {
    let mut paths: Vec<String> = env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect();
    let mut extras = vec![
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ]
    .into_iter()
    .map(|value| value.to_string())
    .collect::<Vec<String>>();
    if let Ok(home) = env::var("HOME") {
        extras.push(format!("{home}/.local/bin"));
        extras.push(format!("{home}/.local/share/mise/shims"));
        extras.push(format!("{home}/.cargo/bin"));
        extras.push(format!("{home}/.bun/bin"));
        // NPM global bin location
        extras.push(format!("{home}/.npm-global/bin"));
        let nvm_root = Path::new(&home).join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(nvm_root) {
            for entry in entries.flatten() {
                let bin_path = entry.path().join("bin");
                if bin_path.is_dir() {
                    extras.push(bin_path.to_string_lossy().to_string());
                }
            }
        }
    }
    if let Some(bin_path) = claude_bin.filter(|value| !value.trim().is_empty()) {
        let parent = Path::new(bin_path).parent();
        if let Some(parent) = parent {
            extras.push(parent.to_string_lossy().to_string());
        }
    }
    for extra in extras {
        if !paths.contains(&extra) {
            paths.push(extra);
        }
    }
    if paths.is_empty() {
        None
    } else {
        Some(paths.join(":"))
    }
}

/// Build a Command for running the claude CLI
pub(crate) fn build_claude_command_with_bin(claude_bin: Option<String>) -> Command {
    let bin = claude_bin
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "claude".into());
    let mut command = Command::new(bin);
    if let Some(path_env) = build_claude_path_env(claude_bin.as_deref()) {
        command.env("PATH", path_env);
    }
    command
}

/// Check if Claude Code CLI is installed and working
pub(crate) async fn check_claude_installation(
    claude_bin: Option<String>,
) -> Result<Option<String>, String> {
    let mut command = build_claude_command_with_bin(claude_bin);
    command.arg("--version");
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let output = match timeout(Duration::from_secs(5), command.output()).await {
        Ok(result) => result.map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                "Claude Code CLI not found. Install Claude Code and ensure `claude` is on your PATH."
                    .to_string()
            } else {
                e.to_string()
            }
        })?,
        Err(_) => {
            return Err(
                "Timed out while checking Claude Code CLI. Make sure `claude --version` runs in Terminal."
                    .to_string(),
            );
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        if detail.is_empty() {
            return Err(
                "Claude Code CLI failed to start. Try running `claude --version` in Terminal."
                    .to_string(),
            );
        }
        return Err(format!(
            "Claude Code CLI failed to start: {detail}. Try running `claude --version` in Terminal."
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if version.is_empty() { None } else { Some(version) })
}

/// Spawn a new Claude Code session
pub(crate) async fn spawn_claude_code_session<E: EventSink>(
    entry: WorkspaceEntry,
    claude_bin: Option<String>,
    _client_version: String,
    event_sink: E,
    resume_session_id: Option<String>,
) -> Result<Arc<ClaudeCodeSession>, String> {
    eprintln!("[spawn_claude_code_session] Starting with resume_session_id={:?}", resume_session_id);
    let _ = check_claude_installation(claude_bin.clone()).await?;

    let mut command = build_claude_command_with_bin(claude_bin);
    command.current_dir(&entry.path);

    // Use print mode with stream-json input/output for bidirectional communication
    command.arg("-p");
    command.arg("--input-format");
    command.arg("stream-json");
    command.arg("--output-format");
    command.arg("stream-json");
    command.arg("--verbose");
    command.arg("--include-partial-messages");

    // Resume existing session if provided
    if let Some(ref session_id) = resume_session_id {
        eprintln!("[spawn_claude_code_session] Adding --resume {}", session_id);
        command.arg("--resume");
        command.arg(session_id);
    }

    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    eprintln!("[spawn_claude_code_session] Spawning command in dir: {}", entry.path);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdin = child.stdin.take().ok_or("missing stdin")?;
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let stderr = child.stderr.take().ok_or("missing stderr")?;

    let session = Arc::new(ClaudeCodeSession {
        entry: entry.clone(),
        child: Mutex::new(child),
        stdin: Mutex::new(stdin),
        session_id: Mutex::new(resume_session_id),
        pending_result: Mutex::new(None),
        background_callbacks: Mutex::new(HashMap::new()),
    });

    let session_clone = Arc::clone(&session);
    let workspace_id = entry.id.clone();
    let event_sink_clone = event_sink.clone();

    // Handle stdout - parse stream-json messages
    tokio::spawn(async move {
        eprintln!("[claude_stdout] Starting stdout handler");
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            eprintln!("[claude_stdout] Received: {}", &line[..std::cmp::min(200, line.len())]);
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("[claude_stdout] Parse error: {}", err);
                    let payload = AppServerEvent {
                        workspace_id: workspace_id.clone(),
                        message: json!({
                            "method": "claude/parseError",
                            "params": { "error": err.to_string(), "raw": line },
                        }),
                    };
                    event_sink_clone.emit_app_server_event(payload);
                    continue;
                }
            };

            let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
            eprintln!("[claude_stdout] Message type: {}", msg_type);

            match msg_type {
                "system" => {
                    let subtype = value.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
                    if subtype == "init" {
                        // Extract and store session_id
                        if let Some(sid) = value.get("session_id").and_then(|s| s.as_str()) {
                            let mut session_id_lock = session_clone.session_id.lock().await;
                            *session_id_lock = Some(sid.to_string());
                        }
                        // Convert to Codex-compatible event
                        let payload = AppServerEvent {
                            workspace_id: workspace_id.clone(),
                            message: json!({
                                "method": "claude/init",
                                "params": value,
                            }),
                        };
                        event_sink_clone.emit_app_server_event(payload);
                    } else {
                        let payload = AppServerEvent {
                            workspace_id: workspace_id.clone(),
                            message: json!({
                                "method": format!("claude/system/{}", subtype),
                                "params": value,
                            }),
                        };
                        event_sink_clone.emit_app_server_event(payload);
                    }
                }
                "assistant" => {
                    // Convert assistant messages to a Codex-compatible format
                    let payload = AppServerEvent {
                        workspace_id: workspace_id.clone(),
                        message: json!({
                            "method": "claude/assistant",
                            "params": value,
                        }),
                    };
                    event_sink_clone.emit_app_server_event(payload);
                }
                "result" => {
                    // Session completed
                    let payload = AppServerEvent {
                        workspace_id: workspace_id.clone(),
                        message: json!({
                            "method": "claude/result",
                            "params": value,
                        }),
                    };
                    event_sink_clone.emit_app_server_event(payload);

                    // If there's a pending result receiver, send it
                    if let Some(tx) = session_clone.pending_result.lock().await.take() {
                        let _ = tx.send(value);
                    }
                }
                _ => {
                    // Forward any other message types
                    let payload = AppServerEvent {
                        workspace_id: workspace_id.clone(),
                        message: json!({
                            "method": format!("claude/{}", msg_type),
                            "params": value,
                        }),
                    };
                    event_sink_clone.emit_app_server_event(payload);
                }
            }
        }
    });

    // Handle stderr
    let workspace_id = entry.id.clone();
    let event_sink_clone = event_sink.clone();
    tokio::spawn(async move {
        eprintln!("[claude_stderr] Starting stderr handler");
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            eprintln!("[claude_stderr] {}", line);
            let payload = AppServerEvent {
                workspace_id: workspace_id.clone(),
                message: json!({
                    "method": "claude/stderr",
                    "params": { "message": line },
                }),
            };
            event_sink_clone.emit_app_server_event(payload);
        }
    });

    // Wait briefly for the init message
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check if session_id was captured (meaning init was received)
    let has_session = session.session_id.lock().await.is_some();
    if !has_session {
        // Wait a bit more
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Emit connected event
    let payload = AppServerEvent {
        workspace_id: entry.id.clone(),
        message: json!({
            "method": "claude/connected",
            "params": {
                "workspaceId": entry.id.clone(),
                "backend": "claude_code"
            }
        }),
    };
    event_sink.emit_app_server_event(payload);

    Ok(session)
}

/// Load session history from transcript file and emit events
pub(crate) async fn load_session_history<E: EventSink>(
    workspace_id: &str,
    workspace_path: &str,
    session_id: &str,
    event_sink: &E,
) -> Result<(), String> {
    eprintln!("[load_session_history] Loading history for session_id={}", session_id);

    // Build the path to the transcript file
    // Claude stores transcripts in ~/.claude/projects/<encoded-path>/<session-id>.jsonl
    let home = env::var("HOME").map_err(|_| "HOME not set")?;
    let encoded_path = workspace_path.replace('/', "-");
    let transcript_path = Path::new(&home)
        .join(".claude")
        .join("projects")
        .join(&encoded_path)
        .join(format!("{}.jsonl", session_id));

    eprintln!("[load_session_history] Looking for transcript at: {:?}", transcript_path);

    if !transcript_path.exists() {
        eprintln!("[load_session_history] Transcript file not found");
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&transcript_path)
        .await
        .map_err(|e| e.to_string())?;

    let mut messages: Vec<Value> = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            let entry_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let message = entry.get("message").cloned();

            match entry_type {
                "user" => {
                    if let Some(msg) = message {
                        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        let uuid = entry.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
                        messages.push(json!({
                            "type": "user",
                            "id": uuid,
                            "content": content,
                            "timestamp": entry.get("timestamp").cloned()
                        }));
                    }
                }
                _ => {
                    // Assistant messages don't have a "type" field at the top level
                    // They have message.role = "assistant"
                    if let Some(msg) = &message {
                        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                            let uuid = entry.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
                            // Extract text content from the content array
                            let mut text_content = String::new();
                            if let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) {
                                for item in content_arr {
                                    if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                            text_content.push_str(text);
                                        }
                                    }
                                }
                            }
                            if !text_content.is_empty() {
                                messages.push(json!({
                                    "type": "assistant",
                                    "id": uuid,
                                    "content": text_content,
                                    "timestamp": entry.get("timestamp").cloned()
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    eprintln!("[load_session_history] Loaded {} messages", messages.len());

    // Emit an event with the session history
    let payload = AppServerEvent {
        workspace_id: workspace_id.to_string(),
        message: json!({
            "method": "claude/history",
            "params": {
                "sessionId": session_id,
                "messages": messages
            }
        }),
    };
    eprintln!("[load_session_history] Emitting claude/history event with sessionId={}, messages_count={}", session_id, messages.len());
    event_sink.emit_app_server_event(payload);
    eprintln!("[load_session_history] Event emitted");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_claude_path_env;

    #[test]
    fn build_claude_path_env_includes_standard_paths() {
        let path = build_claude_path_env(None).unwrap();
        assert!(path.contains("/usr/local/bin"));
        assert!(path.contains("/usr/bin"));
    }

    #[test]
    fn build_claude_path_env_includes_custom_bin_parent() {
        let path = build_claude_path_env(Some("/custom/path/claude")).unwrap();
        assert!(path.contains("/custom/path"));
    }
}
