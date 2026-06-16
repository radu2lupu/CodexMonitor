use serde_json::{json, Map, Value};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, State};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

pub(crate) use crate::backend::app_server::WorkspaceSession;
use crate::backend::app_server::{
    build_codex_command_with_bin, build_codex_path_env, check_codex_installation,
    spawn_workspace_session as spawn_workspace_session_inner,
};
use crate::backend::claude_code::ClaudeCodeSession;
use crate::codex_home::{resolve_default_codex_home, resolve_workspace_codex_home};
use crate::event_sink::TauriEventSink;
use crate::remote_backend;
use crate::rules;
use crate::state::AppState;
use crate::types::{AgentBackend, WorkspaceEntry};

/// Helper to get a Codex session from the sessions map
async fn get_codex_session(
    state: &AppState,
    workspace_id: &str,
) -> Result<Arc<WorkspaceSession>, String> {
    let sessions = state.sessions.lock().await;
    let workspace_sessions = sessions
        .get(workspace_id)
        .ok_or("workspace not connected")?;
    let session = workspace_sessions
        .get(&AgentBackend::Codex)
        .ok_or("Codex backend not connected")?;
    session
        .as_codex()
        .cloned()
        .ok_or_else(|| "session is not a Codex session".to_string())
}

/// Helper to get a Claude Code session from the sessions map
async fn get_claude_code_session(
    state: &AppState,
    workspace_id: &str,
) -> Result<Arc<ClaudeCodeSession>, String> {
    let sessions = state.sessions.lock().await;
    let workspace_sessions = sessions
        .get(workspace_id)
        .ok_or("workspace not connected")?;
    let session = workspace_sessions
        .get(&AgentBackend::ClaudeCode)
        .ok_or("Claude Code backend not connected")?;
    session
        .as_claude_code()
        .cloned()
        .ok_or_else(|| "session is not a Claude Code session".to_string())
}

/// Helper to check which backends are connected for a workspace
async fn get_connected_backends(state: &AppState, workspace_id: &str) -> Vec<AgentBackend> {
    let sessions = state.sessions.lock().await;
    sessions
        .get(workspace_id)
        .map(|ws| ws.keys().cloned().collect())
        .unwrap_or_default()
}

/// Thread ID prefix for Claude Code sessions
const CLAUDE_THREAD_PREFIX: &str = "claude-";

pub(crate) async fn spawn_workspace_session(
    entry: WorkspaceEntry,
    default_codex_bin: Option<String>,
    app_handle: AppHandle,
    codex_home: Option<PathBuf>,
) -> Result<Arc<WorkspaceSession>, String> {
    let client_version = app_handle.package_info().version.to_string();
    let event_sink = TauriEventSink::new(app_handle);
    spawn_workspace_session_inner(
        entry,
        default_codex_bin,
        client_version,
        event_sink,
        codex_home,
    )
    .await
}

#[tauri::command]
pub(crate) async fn codex_doctor(
    codex_bin: Option<String>,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let default_bin = {
        let settings = state.app_settings.lock().await;
        settings.codex_bin.clone()
    };
    let resolved = codex_bin
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or(default_bin);
    let path_env = build_codex_path_env(resolved.as_deref());
    let version = check_codex_installation(resolved.clone()).await?;
    let mut command = build_codex_command_with_bin(resolved.clone());
    command.arg("app-server");
    command.arg("--help");
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let app_server_ok = match timeout(Duration::from_secs(5), command.output()).await {
        Ok(result) => result.map(|output| output.status.success()).unwrap_or(false),
        Err(_) => false,
    };
    let (node_ok, node_version, node_details) = {
        let mut node_command = Command::new("node");
        if let Some(ref path_env) = path_env {
            node_command.env("PATH", path_env);
        }
        node_command.arg("--version");
        node_command.stdout(std::process::Stdio::piped());
        node_command.stderr(std::process::Stdio::piped());
        match timeout(Duration::from_secs(5), node_command.output()).await {
            Ok(result) => match result {
                Ok(output) => {
                    if output.status.success() {
                        let version = String::from_utf8_lossy(&output.stdout)
                            .trim()
                            .to_string();
                        (
                            !version.is_empty(),
                            if version.is_empty() { None } else { Some(version) },
                            None,
                        )
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let detail = if stderr.trim().is_empty() {
                            stdout.trim()
                        } else {
                            stderr.trim()
                        };
                        (
                            false,
                            None,
                            Some(if detail.is_empty() {
                                "Node failed to start.".to_string()
                            } else {
                                detail.to_string()
                            }),
                        )
                    }
                }
                Err(err) => {
                    if err.kind() == ErrorKind::NotFound {
                        (false, None, Some("Node not found on PATH.".to_string()))
                    } else {
                        (false, None, Some(err.to_string()))
                    }
                }
            },
            Err(_) => (false, None, Some("Timed out while checking Node.".to_string())),
        }
    };
    let details = if app_server_ok {
        None
    } else {
        Some("Failed to run `codex app-server --help`.".to_string())
    };
    Ok(json!({
        "ok": version.is_some() && app_server_ok,
        "codexBin": resolved,
        "version": version,
        "appServerOk": app_server_ok,
        "details": details,
        "path": path_env,
        "nodeOk": node_ok,
        "nodeVersion": node_version,
        "nodeDetails": node_details,
    }))
}

#[tauri::command]
pub(crate) async fn start_thread(
    workspace_id: String,
    backend: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "start_thread",
            json!({ "workspaceId": workspace_id, "backend": backend }),
        )
        .await;
    }

    // Parse backend from string, default to codex if not specified
    let target_backend = match backend.as_deref() {
        Some("claudecode") => AgentBackend::ClaudeCode,
        Some("codex") | None => AgentBackend::Codex,
        Some(other) => return Err(format!("Unknown backend: {}", other)),
    };

    match target_backend {
        AgentBackend::Codex => {
            let session = get_codex_session(&*state, &workspace_id).await?;
            let params = json!({
                "cwd": session.entry.path,
                "approvalPolicy": "on-request"
            });
            session.send_request("thread/start", params).await
        }
        AgentBackend::ClaudeCode => {
            // For Claude Code, return the session as a "thread"
            let session = get_claude_code_session(&*state, &workspace_id).await?;
            let session_id = session.get_session_id().await.unwrap_or_else(|| {
                format!("{}{}", CLAUDE_THREAD_PREFIX, workspace_id)
            });
            Ok(json!({
                "result": {
                    "threadId": session_id,
                    "thread": {
                        "id": session_id,
                        "name": "Claude Code Session",
                        "updatedAt": chrono::Utc::now().timestamp_millis(),
                        "backend": "claudecode"
                    }
                }
            }))
        }
    }
}

#[tauri::command]
pub(crate) async fn resume_thread(
    workspace_id: String,
    thread_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "resume_thread",
            json!({ "workspaceId": workspace_id, "threadId": thread_id }),
        )
        .await;
    }

    // Determine backend from thread_id prefix
    let is_claude = thread_id.starts_with(CLAUDE_THREAD_PREFIX);

    if is_claude {
        // Extract the actual session ID (remove the claude- prefix)
        let claude_session_id = thread_id.strip_prefix(CLAUDE_THREAD_PREFIX).unwrap_or(&thread_id);
        eprintln!("[resume_thread] Resuming Claude session: thread_id={}, claude_session_id={}", thread_id, claude_session_id);

        // Get workspace entry and settings
        let (entry, default_bin) = {
            let workspaces = state.workspaces.lock().await;
            let entry = workspaces.get(&workspace_id).cloned().ok_or("workspace not found")?;
            let settings = state.app_settings.lock().await;
            eprintln!("[resume_thread] Workspace: path={}, default_bin={:?}", entry.path, settings.codex_bin);
            (entry, settings.codex_bin.clone())
        };

        // Kill the existing Claude Code session if any
        {
            let mut sessions = state.sessions.lock().await;
            if let Some(workspace_sessions) = sessions.get_mut(&workspace_id) {
                if let Some(old_session) = workspace_sessions.remove(&AgentBackend::ClaudeCode) {
                    eprintln!("[resume_thread] Killing existing Claude session");
                    old_session.kill_child().await;
                }
            }
        }

        // Spawn a new Claude Code session with --resume
        eprintln!("[resume_thread] Spawning new Claude session with --resume {}", claude_session_id);
        let event_sink = crate::event_sink::TauriEventSink::new(app.clone());
        let new_session = crate::backend::claude_code::spawn_claude_code_session(
            entry.clone(),
            default_bin,
            env!("CARGO_PKG_VERSION").to_string(),
            event_sink,
            Some(claude_session_id.to_string()),
        )
        .await?;

        // Check what session ID was captured
        let captured_session_id = new_session.get_session_id().await;
        eprintln!("[resume_thread] New session captured session_id: {:?}", captured_session_id);

        // Store the new session
        {
            let mut sessions = state.sessions.lock().await;
            let workspace_sessions = sessions.entry(workspace_id.clone()).or_insert_with(std::collections::HashMap::new);
            workspace_sessions.insert(AgentBackend::ClaudeCode, crate::state::AgentSession::ClaudeCode(new_session));
        }

        // Load and emit the session history
        let event_sink = crate::event_sink::TauriEventSink::new(app.clone());
        if let Err(e) = crate::backend::claude_code::load_session_history(
            &workspace_id,
            &entry.path,
            claude_session_id,
            &event_sink,
        ).await {
            eprintln!("[resume_thread] Failed to load session history: {}", e);
        }

        let response = json!({
            "result": {
                "threadId": thread_id,
                "thread": {
                    "id": thread_id,
                    "preview": "Claude Code Session",
                    "updatedAt": chrono::Utc::now().timestamp_millis(),
                    "backend": "claudecode",
                    "cwd": entry.path
                }
            }
        });
        eprintln!("[resume_thread] Returning response: {}", response);
        Ok(response)
    } else {
        let session = get_codex_session(&*state, &workspace_id).await?;
        let params = json!({
            "threadId": thread_id
        });
        session.send_request("thread/resume", params).await
    }
}

#[tauri::command]
pub(crate) async fn list_threads(
    workspace_id: String,
    cursor: Option<String>,
    limit: Option<u32>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "list_threads",
            json!({ "workspaceId": workspace_id, "cursor": cursor, "limit": limit }),
        )
        .await;
    }

    // Aggregate threads from all connected backends
    let connected_backends = get_connected_backends(&*state, &workspace_id).await;
    let mut all_threads: Vec<Value> = Vec::new();

    // Get Codex threads if connected
    if connected_backends.contains(&AgentBackend::Codex) {
        if let Ok(session) = get_codex_session(&*state, &workspace_id).await {
            let params = json!({
                "cursor": cursor,
                "limit": limit,
            });
            if let Ok(result) = session.send_request("thread/list", params).await {
                // Try both "data" and "threads" keys since Codex uses "data"
                let threads = result
                    .get("result")
                    .and_then(|r| r.get("data").or_else(|| r.get("threads")))
                    .and_then(|t| t.as_array());
                if let Some(threads) = threads {
                    // Add backend info to each thread
                    for thread in threads {
                        let mut thread_with_backend = thread.clone();
                        if let Some(obj) = thread_with_backend.as_object_mut() {
                            obj.insert("backend".to_string(), json!("codex"));
                        }
                        all_threads.push(thread_with_backend);
                    }
                }
            }
        }
    }

    // Get Claude Code sessions from history file
    if connected_backends.contains(&AgentBackend::ClaudeCode) {
        // Get workspace path for filtering sessions
        let workspace_path = {
            let workspaces = state.workspaces.lock().await;
            workspaces.get(&workspace_id).map(|e| e.path.clone()).unwrap_or_default()
        };

        // Read Claude Code history file to get sessions for this workspace
        if let Ok(home) = std::env::var("HOME") {
            let history_path = PathBuf::from(&home).join(".claude").join("history.jsonl");
            if let Ok(content) = tokio::fs::read_to_string(&history_path).await {
                // Parse history entries and group by session ID
                // Track: (first_display, first_timestamp, last_timestamp)
                let mut sessions: std::collections::HashMap<String, (String, i64, i64)> = std::collections::HashMap::new();

                for line in content.lines() {
                    if let Ok(entry) = serde_json::from_str::<Value>(line) {
                        let project = entry.get("project").and_then(|p| p.as_str()).unwrap_or("");
                        let session_id = entry.get("sessionId").and_then(|s| s.as_str()).unwrap_or("");
                        let display = entry.get("display").and_then(|d| d.as_str()).unwrap_or("").trim();
                        let timestamp = entry.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);

                        // Check if this session belongs to our workspace
                        if !session_id.is_empty() && project == workspace_path {
                            sessions.entry(session_id.to_string())
                                .and_modify(|(name, first_ts, last_ts)| {
                                    // Update last timestamp if newer
                                    if timestamp > *last_ts {
                                        *last_ts = timestamp;
                                    }
                                    // Use the earliest display as the name
                                    if timestamp < *first_ts {
                                        *name = display.to_string();
                                        *first_ts = timestamp;
                                    }
                                })
                                .or_insert_with(|| {
                                    (display.to_string(), timestamp, timestamp)
                                });
                        }
                    }
                }

                // Add sessions to all_threads
                for (session_id, (display, _, last_timestamp)) in sessions {
                    let thread_id = format!("{}{}", CLAUDE_THREAD_PREFIX, session_id);
                    all_threads.push(json!({
                        "id": thread_id,
                        "preview": display,  // Frontend uses "preview" for thread names
                        "updatedAt": last_timestamp,
                        "backend": "claudecode",
                        "cwd": workspace_path
                    }));
                }
            }
        }
    }

    // Sort by updatedAt descending (most recent first)
    all_threads.sort_by(|a, b| {
        let a_time = a.get("updatedAt").and_then(|t| t.as_i64()).unwrap_or(0);
        let b_time = b.get("updatedAt").and_then(|t| t.as_i64()).unwrap_or(0);
        b_time.cmp(&a_time)
    });

    Ok(json!({
        "result": {
            "data": all_threads,
            "hasMore": false
        }
    }))
}

#[tauri::command]
pub(crate) async fn archive_thread(
    workspace_id: String,
    thread_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "archive_thread",
            json!({ "workspaceId": workspace_id, "threadId": thread_id }),
        )
        .await;
    }

    // Determine backend from thread_id prefix
    let is_claude = thread_id.starts_with(CLAUDE_THREAD_PREFIX);

    if is_claude {
        // Claude Code doesn't have a thread archiving concept
        // Return success as a no-op
        Ok(json!({
            "result": {
                "ok": true,
                "threadId": thread_id,
            }
        }))
    } else {
        let session = get_codex_session(&*state, &workspace_id).await?;
        let params = json!({
            "threadId": thread_id
        });
        session.send_request("thread/archive", params).await
    }
}

#[tauri::command]
pub(crate) async fn send_user_message(
    workspace_id: String,
    thread_id: String,
    text: String,
    model: Option<String>,
    effort: Option<String>,
    access_mode: Option<String>,
    images: Option<Vec<String>>,
    collaboration_mode: Option<Value>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "send_user_message",
            json!({
                "workspaceId": workspace_id,
                "threadId": thread_id,
                "text": text,
                "model": model,
                "effort": effort,
                "accessMode": access_mode,
                "images": images,
                "collaborationMode": collaboration_mode,
            }),
        )
        .await;
    }

    // Determine backend from thread_id prefix
    let is_claude = thread_id.starts_with(CLAUDE_THREAD_PREFIX);

    if is_claude {
        let session = get_claude_code_session(&*state, &workspace_id).await?;
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() {
            return Err("empty user message".to_string());
        }
        // Send user message to Claude Code
        session.send_user_message(trimmed_text).await?;
        // Return a success response with turn info
        let session_id = session.get_session_id().await.unwrap_or_else(|| {
            format!("{}{}", CLAUDE_THREAD_PREFIX, workspace_id)
        });
        let turn_id = format!("turn-{}", chrono::Utc::now().timestamp_millis());
        Ok(json!({
            "result": {
                "turnId": turn_id,
                "threadId": session_id,
            }
        }))
    } else {
        let session = get_codex_session(&*state, &workspace_id).await?;
        let access_mode = access_mode.unwrap_or_else(|| "current".to_string());
        let sandbox_policy = match access_mode.as_str() {
            "full-access" => json!({
                "type": "dangerFullAccess"
            }),
            "read-only" => json!({
                "type": "readOnly"
            }),
            _ => json!({
                "type": "workspaceWrite",
                "writableRoots": [session.entry.path],
                "networkAccess": true
            }),
        };

        let approval_policy = if access_mode == "full-access" {
            "never"
        } else {
            "on-request"
        };

        let trimmed_text = text.trim();
        let mut input: Vec<Value> = Vec::new();
        if !trimmed_text.is_empty() {
            input.push(json!({ "type": "text", "text": trimmed_text }));
        }
        if let Some(paths) = images {
            for path in paths {
                let trimmed = path.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.starts_with("data:")
                    || trimmed.starts_with("http://")
                    || trimmed.starts_with("https://")
                {
                    input.push(json!({ "type": "image", "url": trimmed }));
                } else {
                    input.push(json!({ "type": "localImage", "path": trimmed }));
                }
            }
        }
        if input.is_empty() {
            return Err("empty user message".to_string());
        }

        let params = json!({
            "threadId": thread_id,
            "input": input,
            "cwd": session.entry.path,
            "approvalPolicy": approval_policy,
            "sandboxPolicy": sandbox_policy,
            "model": model,
            "effort": effort,
            "collaborationMode": collaboration_mode,
        });
        session.send_request("turn/start", params).await
    }
}

#[tauri::command]
pub(crate) async fn collaboration_mode_list(
    workspace_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "collaboration_mode_list",
            json!({ "workspaceId": workspace_id }),
        )
        .await;
    }

    let session = get_codex_session(&*state, &workspace_id).await?;
    session
        .send_request("collaborationMode/list", json!({}))
        .await
}

#[tauri::command]
pub(crate) async fn turn_interrupt(
    workspace_id: String,
    thread_id: String,
    turn_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "turn_interrupt",
            json!({ "workspaceId": workspace_id, "threadId": thread_id, "turnId": turn_id }),
        )
        .await;
    }

    // Determine backend from thread_id prefix
    let is_claude = thread_id.starts_with(CLAUDE_THREAD_PREFIX);

    if is_claude {
        let session = get_claude_code_session(&*state, &workspace_id).await?;
        // Send abort control message to Claude Code
        session.send_control("abort").await?;
        Ok(json!({
            "result": {
                "ok": true,
                "turnId": turn_id,
            }
        }))
    } else {
        let session = get_codex_session(&*state, &workspace_id).await?;
        let params = json!({
            "threadId": thread_id,
            "turnId": turn_id,
        });
        session.send_request("turn/interrupt", params).await
    }
}

#[tauri::command]
pub(crate) async fn start_review(
    workspace_id: String,
    thread_id: String,
    target: Value,
    delivery: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "start_review",
            json!({
                "workspaceId": workspace_id,
                "threadId": thread_id,
                "target": target,
                "delivery": delivery,
            }),
        )
        .await;
    }

    let session = get_codex_session(&*state, &workspace_id).await?;
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    params.insert("target".to_string(), target);
    if let Some(delivery) = delivery {
        params.insert("delivery".to_string(), json!(delivery));
    }
    session
        .send_request("review/start", Value::Object(params))
        .await
}

#[tauri::command]
pub(crate) async fn model_list(
    workspace_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "model_list",
            json!({ "workspaceId": workspace_id }),
        )
        .await;
    }

    let session = get_codex_session(&*state, &workspace_id).await?;
    let params = json!({});
    session.send_request("model/list", params).await
}

#[tauri::command]
pub(crate) async fn account_rate_limits(
    workspace_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "account_rate_limits",
            json!({ "workspaceId": workspace_id }),
        )
        .await;
    }

    let session = get_codex_session(&*state, &workspace_id).await?;
    session
        .send_request("account/rateLimits/read", Value::Null)
        .await
}

#[tauri::command]
pub(crate) async fn skills_list(
    workspace_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Value, String> {
    if remote_backend::is_remote_mode(&*state).await {
        return remote_backend::call_remote(
            &*state,
            app,
            "skills_list",
            json!({ "workspaceId": workspace_id }),
        )
        .await;
    }

    let session = get_codex_session(&*state, &workspace_id).await?;
    let params = json!({
        "cwd": session.entry.path
    });
    session.send_request("skills/list", params).await
}

#[tauri::command]
pub(crate) async fn respond_to_server_request(
    workspace_id: String,
    request_id: u64,
    result: Value,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    if remote_backend::is_remote_mode(&*state).await {
        remote_backend::call_remote(
            &*state,
            app,
            "respond_to_server_request",
            json!({ "workspaceId": workspace_id, "requestId": request_id, "result": result }),
        )
        .await?;
        return Ok(());
    }

    let session = get_codex_session(&*state, &workspace_id).await?;
    session.send_response(request_id, result).await
}

/// Gets the diff content for commit message generation
#[tauri::command]
pub(crate) async fn get_commit_message_prompt(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    // Get the diff from git
    let diff = crate::git::get_workspace_diff(&workspace_id, &state).await?;

    if diff.trim().is_empty() {
        return Err("No changes to generate commit message for".to_string());
    }

    let prompt = format!(
        "Generate a concise git commit message for the following changes. \
Follow conventional commit format (e.g., feat:, fix:, refactor:, docs:, etc.). \
Focus on the 'why' rather than the 'what'. Keep the summary line under 72 characters. \
Only output the commit message, nothing else.\n\n\
Changes:\n{diff}"
    );

    Ok(prompt)
}

#[tauri::command]
pub(crate) async fn remember_approval_rule(
    workspace_id: String,
    command: Vec<String>,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let command = command
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>();
    if command.is_empty() {
        return Err("empty command".to_string());
    }

    let (entry, parent_path) = {
        let workspaces = state.workspaces.lock().await;
        let entry = workspaces
            .get(&workspace_id)
            .ok_or("workspace not found")?
            .clone();
        let parent_path = entry
            .parent_id
            .as_ref()
            .and_then(|parent_id| workspaces.get(parent_id))
            .map(|parent| parent.path.clone());
        (entry, parent_path)
    };

    let codex_home = resolve_workspace_codex_home(&entry, parent_path.as_deref())
        .or_else(resolve_default_codex_home)
        .ok_or("Unable to resolve CODEX_HOME".to_string())?;
    let rules_path = rules::default_rules_path(&codex_home);
    rules::append_prefix_rule(&rules_path, &command)?;

    Ok(json!({
        "ok": true,
        "rulesPath": rules_path,
    }))
}

/// Generates a commit message in the background without showing in the main chat
#[tauri::command]
pub(crate) async fn generate_commit_message(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    // Get the diff from git
    let diff = crate::git::get_workspace_diff(&workspace_id, &state).await?;

    if diff.trim().is_empty() {
        return Err("No changes to generate commit message for".to_string());
    }

    let prompt = format!(
        "Generate a concise git commit message for the following changes. \
Follow conventional commit format (e.g., feat:, fix:, refactor:, docs:, etc.). \
Focus on the 'why' rather than the 'what'. Keep the summary line under 72 characters. \
Only output the commit message, nothing else.\n\n\
Changes:\n{diff}"
    );

    // Get the session (currently only Codex supports this)
    let session = get_codex_session(&*state, &workspace_id).await?;

    // Create a background thread
    let thread_params = json!({
        "cwd": session.entry.path,
        "approvalPolicy": "never"  // Never ask for approval in background
    });
    let thread_result = session.send_request("thread/start", thread_params).await?;

    // Handle error response
    if let Some(error) = thread_result.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error starting thread");
        return Err(error_msg.to_string());
    }

    // Extract threadId - try multiple paths since response format may vary
    let thread_id = thread_result
        .get("result")
        .and_then(|r| r.get("threadId"))
        .or_else(|| thread_result.get("result").and_then(|r| r.get("thread")).and_then(|t| t.get("id")))
        .or_else(|| thread_result.get("threadId"))
        .or_else(|| thread_result.get("thread").and_then(|t| t.get("id")))
        .and_then(|t| t.as_str())
        .ok_or_else(|| format!("Failed to get threadId from thread/start response: {:?}", thread_result))?
        .to_string();

    // Create channel for receiving events
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    // Register callback for this thread
    {
        let mut callbacks = session.background_thread_callbacks.lock().await;
        callbacks.insert(thread_id.clone(), tx);
    }

    // Start a turn with the commit message prompt
    let turn_params = json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": prompt }],
        "cwd": session.entry.path,
        "approvalPolicy": "never",
        "sandboxPolicy": { "type": "readOnly" },
    });
    let turn_result = session.send_request("turn/start", turn_params).await;
    let turn_result = match turn_result {
        Ok(result) => result,
        Err(error) => {
            // Clean up if turn fails to start
            {
                let mut callbacks = session.background_thread_callbacks.lock().await;
                callbacks.remove(&thread_id);
            }
            let archive_params = json!({ "threadId": thread_id.as_str() });
            let _ = session.send_request("thread/archive", archive_params).await;
            return Err(error);
        }
    };

    if let Some(error) = turn_result.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error starting turn");
        {
            let mut callbacks = session.background_thread_callbacks.lock().await;
            callbacks.remove(&thread_id);
        }
        let archive_params = json!({ "threadId": thread_id.as_str() });
        let _ = session.send_request("thread/archive", archive_params).await;
        return Err(error_msg.to_string());
    }

    // Collect assistant text from events
    let mut commit_message = String::new();
    let timeout_duration = Duration::from_secs(60);
    let collect_result = timeout(timeout_duration, async {
        while let Some(event) = rx.recv().await {
            let method = event.get("method").and_then(|m| m.as_str()).unwrap_or("");

            match method {
                "item/agentMessage/delta" => {
                    // Extract text delta from agent messages
                    if let Some(params) = event.get("params") {
                        if let Some(delta) = params.get("delta").and_then(|d| d.as_str()) {
                            commit_message.push_str(delta);
                        }
                    }
                }
                "turn/completed" => {
                    // Turn completed, we can stop listening
                    break;
                }
                "turn/error" => {
                    // Error occurred
                    let error_msg = event
                        .get("params")
                        .and_then(|p| p.get("error"))
                        .and_then(|e| e.as_str())
                        .unwrap_or("Unknown error during commit message generation");
                    return Err(error_msg.to_string());
                }
                _ => {
                    // Ignore other events (turn/started, item/started, item/completed, reasoning events, etc.)
                }
            }
        }
        Ok(())
    })
    .await;

    // Unregister callback
    {
        let mut callbacks = session.background_thread_callbacks.lock().await;
        callbacks.remove(&thread_id);
    }

    // Archive the thread to clean up
    let archive_params = json!({ "threadId": thread_id });
    let _ = session.send_request("thread/archive", archive_params).await;

    // Handle timeout or collection error
    match collect_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("Timeout waiting for commit message generation".to_string()),
    }

    let trimmed = commit_message.trim().to_string();
    if trimmed.is_empty() {
        return Err("No commit message was generated".to_string());
    }

    Ok(trimmed)
}
