use serde_json::{json, Value};
use std::sync::Arc;
use tauri::State;

use crate::backend::claude_code::ClaudeCodeSession;
use crate::state::AppState;
use crate::types::AgentBackend;

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
        .ok_or("Claude Code backend not connected for this workspace")?;
    session
        .as_claude_code()
        .cloned()
        .ok_or_else(|| "session is not a Claude Code session".to_string())
}

/// Send a user message to a Claude Code session
#[tauri::command]
pub(crate) async fn claude_send_message(
    workspace_id: String,
    text: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let session = get_claude_code_session(&*state, &workspace_id).await?;
    session.send_user_message(&text).await?;
    Ok(json!({ "ok": true }))
}

/// Interrupt/abort the current Claude Code operation
#[tauri::command]
pub(crate) async fn claude_interrupt(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let session = get_claude_code_session(&*state, &workspace_id).await?;
    session.send_control("abort").await?;
    Ok(json!({ "ok": true }))
}

/// Get the current session ID for a Claude Code workspace
#[tauri::command]
pub(crate) async fn claude_get_session_id(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let session = get_claude_code_session(&*state, &workspace_id).await?;
    let session_id = session.get_session_id().await;
    Ok(json!({ "sessionId": session_id }))
}
