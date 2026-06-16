use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Manager};
use tokio::sync::Mutex;

use crate::backend::app_server::WorkspaceSession as CodexSession;
use crate::backend::claude_code::ClaudeCodeSession;
use crate::dictation::DictationState;
use crate::storage::{read_settings, read_workspaces};
use crate::types::{AppSettings, WorkspaceEntry};

/// Unified session type that can hold either a Codex or Claude Code session
pub(crate) enum AgentSession {
    Codex(Arc<CodexSession>),
    ClaudeCode(Arc<ClaudeCodeSession>),
}

impl AgentSession {
    pub(crate) fn as_codex(&self) -> Option<&Arc<CodexSession>> {
        match self {
            AgentSession::Codex(session) => Some(session),
            AgentSession::ClaudeCode(_) => None,
        }
    }

    pub(crate) fn as_claude_code(&self) -> Option<&Arc<ClaudeCodeSession>> {
        match self {
            AgentSession::Codex(_) => None,
            AgentSession::ClaudeCode(session) => Some(session),
        }
    }

    /// Kill the underlying child process for this session
    pub(crate) async fn kill_child(&self) {
        match self {
            AgentSession::Codex(session) => {
                let mut child = session.child.lock().await;
                let _ = child.kill().await;
            }
            AgentSession::ClaudeCode(session) => {
                let mut child = session.child.lock().await;
                let _ = child.kill().await;
            }
        }
    }
}

use crate::types::AgentBackend;

/// Sessions for a workspace - can have multiple backends connected simultaneously
pub(crate) type WorkspaceSessions = HashMap<AgentBackend, AgentSession>;

pub(crate) struct AppState {
    pub(crate) workspaces: Mutex<HashMap<String, WorkspaceEntry>>,
    /// Maps workspace_id -> backend -> session (supports multiple backends per workspace)
    pub(crate) sessions: Mutex<HashMap<String, WorkspaceSessions>>,
    pub(crate) terminal_sessions:
        Mutex<HashMap<String, Arc<crate::terminal::TerminalSession>>>,
    pub(crate) remote_backend: Mutex<Option<crate::remote_backend::RemoteBackend>>,
    pub(crate) storage_path: PathBuf,
    pub(crate) settings_path: PathBuf,
    pub(crate) app_settings: Mutex<AppSettings>,
    pub(crate) dictation: Mutex<DictationState>,
}

impl AppState {
    pub(crate) fn load(app: &AppHandle) -> Self {
        let data_dir = app
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| ".".into()));
        let storage_path = data_dir.join("workspaces.json");
        let settings_path = data_dir.join("settings.json");
        let workspaces = read_workspaces(&storage_path).unwrap_or_default();
        let app_settings = read_settings(&settings_path).unwrap_or_default();
        Self {
            workspaces: Mutex::new(workspaces),
            sessions: Mutex::new(HashMap::new()),
            terminal_sessions: Mutex::new(HashMap::new()),
            remote_backend: Mutex::new(None),
            storage_path,
            settings_path,
            app_settings: Mutex::new(app_settings),
            dictation: Mutex::new(DictationState::default()),
        }
    }
}
