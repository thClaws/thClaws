import type { IPCMessage } from "../hooks/useIPC";

/** The backend frame for a gui-shell's `{ns:"thclaws-shell"}` message, or
 *  null when it must not be forwarded. The shell is third-party script: it
 *  names a `gui_shell_*` arm and nothing else, never which shell it is, and
 *  never answers its own approval prompts — those go to the user. */
export function shellFrameToIpc(
  data: { type?: unknown; requestId?: unknown; sessionId?: unknown; payload?: unknown },
  shellId: string,
  defaultSessionId: string,
): IPCMessage | null {
  if (typeof data.type !== "string" || !/^[a-z][a-z_]*$/.test(data.type)) return null;
  if (data.type === "approval_respond") return null;
  const payload =
    data.payload && typeof data.payload === "object" && !Array.isArray(data.payload)
      ? { ...(data.payload as Record<string, unknown>) }
      : {};
  delete payload.preferInline;
  return {
    ...payload,
    type: `gui_shell_${data.type}`,
    id: data.requestId,
    sessionId: typeof data.sessionId === "string" ? data.sessionId : defaultSessionId,
    shellId,
  };
}
