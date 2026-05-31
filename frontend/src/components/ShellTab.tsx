import { useState } from "react";
import { ArrowLeft } from "lucide-react";
import { ShellPicker } from "./ShellPicker";
import { ShellView } from "./ShellView";

// dev-plan/33 Tier 2 — single-instance container for the Shell tab.
// Holds the picker until the user selects a shell, then mounts the
// iframe via ShellView. A small "Back" header button returns to the
// picker (the shell session persists; reopening from picker resumes
// it in Tier 2b once per-shell session ids are wired).
//
// Multi-instance shell tabs (N concurrent shells in N tabs) is Task 13.

interface ShellTabProps {
  active: boolean;
}

export function ShellTab({ active }: ShellTabProps) {
  const [selected, setSelected] = useState<string | null>(null);
  // Once the user has gone back to the picker (via the breadcrumb),
  // we want the grid even if settings.json::guiShell.tabDefault is set
  // — otherwise they'd be looped straight back to the default.
  const [skipDefault, setSkipDefault] = useState(false);

  if (selected === null) {
    return (
      <ShellPicker
        onSelect={setSelected}
        honourDefault={!skipDefault}
      />
    );
  }

  return (
    <div className="w-full h-full flex flex-col">
      <div
        className="flex items-center gap-2 px-3 py-1.5 text-xs border-b"
        style={{
          background: "var(--bg-secondary)",
          borderColor: "var(--border)",
          color: "var(--text-secondary)",
        }}
      >
        <button
          onClick={() => {
            setSkipDefault(true);
            setSelected(null);
          }}
          className="flex items-center gap-1 hover:underline"
          title="Return to shell picker"
        >
          <ArrowLeft size={12} /> shells
        </button>
        <span style={{ color: "var(--text-secondary)" }}>/</span>
        <span className="font-mono" style={{ color: "var(--text-primary)" }}>
          {selected}
        </span>
      </div>
      <div className="flex-1 min-h-0">
        <ShellView active={active} shellId={selected} />
      </div>
    </div>
  );
}
