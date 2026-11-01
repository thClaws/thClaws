import { useEffect, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

/**
 * Audit 2026-11-01 #9: when no OS sandbox can enforce `bash.sandbox`
 * (Windows; Linux without Landlock/bwrap) every Bash command and hook runs
 * with the user's full rights. The backend used to say so only in a log
 * file while settings still read `workspace`, so this stays up, not
 * dismissible, for as long as that is true.
 */
export function ConfinementBanner() {
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type !== "confinement_status") return;
      setNotice(
        msg.unconfined === true && typeof msg.notice === "string" ? msg.notice : null,
      );
    });
    send({ type: "confinement_status" });
    return unsub;
  }, []);

  if (!notice) return null;
  return (
    <div
      role="alert"
      className="px-3 py-1.5 text-xs text-center"
      style={{
        background: "var(--bg-secondary)",
        color: "var(--text-primary)",
        borderBottom: "2px solid var(--warning, #b45309)",
      }}
      title={notice}
    >
      <strong>Unconfined:</strong> {notice}
    </div>
  );
}
