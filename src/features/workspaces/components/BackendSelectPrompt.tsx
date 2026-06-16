import type { AgentBackend } from "../../../types";

type BackendSelectPromptProps = {
  onCancel: () => void;
  onSelect: (backend: AgentBackend) => void;
  title?: string;
  subtitle?: string;
};

export function BackendSelectPrompt({
  onCancel,
  onSelect,
  title = "Select Agent Backend",
  subtitle = "Choose which AI backend to use for this workspace.",
}: BackendSelectPromptProps) {
  return (
    <div className="worktree-modal" role="dialog" aria-modal="true">
      <div className="worktree-modal-backdrop" onClick={onCancel} />
      <div className="worktree-modal-card">
        <div className="worktree-modal-title">{title}</div>
        <div className="worktree-modal-subtitle">
          {subtitle}
        </div>
        <div className="worktree-modal-actions" style={{ flexDirection: "column", gap: "8px" }}>
          <button
            className="primary worktree-modal-button"
            onClick={() => onSelect("codex")}
            type="button"
            style={{ width: "100%" }}
          >
            Codex (OpenAI)
          </button>
          <button
            className="primary worktree-modal-button"
            onClick={() => onSelect("claudecode")}
            type="button"
            style={{ width: "100%" }}
          >
            Claude Code (Anthropic)
          </button>
          <button
            className="ghost worktree-modal-button"
            onClick={onCancel}
            type="button"
            style={{ width: "100%", marginTop: "8px" }}
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
