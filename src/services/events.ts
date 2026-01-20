import { listen } from "@tauri-apps/api/event";
import type { AppServerEvent, DictationEvent, DictationModelStatus } from "../types";

export type Unsubscribe = () => void;

export type TerminalOutputEvent = {
  workspaceId: string;
  terminalId: string;
  data: string;
};

export type MenuEventPayload = {
  id: number;
};

type SubscriptionOptions = {
  onError?: (error: unknown) => void;
};

function subscribeEvent<T>(
  eventName: string,
  onEvent: (payload: T) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  let active = true;
  let unlisten: Unsubscribe | null = null;

  listen<T>(eventName, (event) => {
    onEvent(event.payload);
  })
    .then((handler) => {
      if (active) {
        unlisten = handler;
      } else {
        handler();
      }
    })
    .catch((error) => {
      options?.onError?.(error);
    });

  return () => {
    active = false;
    if (unlisten) {
      try {
        unlisten();
      } catch {
        // Ignore double-unlisten when tearing down.
      }
    }
  };
}

export function subscribeAppServerEvents(
  onEvent: (event: AppServerEvent) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<AppServerEvent>("app-server-event", onEvent, options);
}

export function subscribeDictationDownload(
  onEvent: (event: DictationModelStatus) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<DictationModelStatus>("dictation-download", onEvent, options);
}

export function subscribeDictationEvents(
  onEvent: (event: DictationEvent) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<DictationEvent>("dictation-event", onEvent, options);
}

export function subscribeTerminalOutput(
  onEvent: (event: TerminalOutputEvent) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<TerminalOutputEvent>("terminal-output", onEvent, options);
}

export function subscribeUpdaterCheck(
  onEvent: () => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<void>("updater-check", () => {
    onEvent();
  }, options);
}

export function subscribeMenuNewAgent(
  onEvent: (event: MenuEventPayload) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<MenuEventPayload>("menu-new-agent", onEvent, options);
}

export function subscribeMenuNewWorktreeAgent(
  onEvent: (event: MenuEventPayload) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<MenuEventPayload>("menu-new-worktree-agent", onEvent, options);
}

export function subscribeMenuNewCloneAgent(
  onEvent: (event: MenuEventPayload) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<MenuEventPayload>("menu-new-clone-agent", onEvent, options);
}

export function subscribeMenuAddWorkspace(
  onEvent: (event: MenuEventPayload) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<MenuEventPayload>("menu-add-workspace", onEvent, options);
}

export function subscribeMenuOpenSettings(
  onEvent: (event: MenuEventPayload) => void,
  options?: SubscriptionOptions,
): Unsubscribe {
  return subscribeEvent<MenuEventPayload>("menu-open-settings", onEvent, options);
}
