import { useEffect, useState } from "react";
import {
  ChevronRight,
  X,
  BookOpen,
  FileText,
  Link2,
  Network,
  Plus,
} from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { KmsCreateModal, type KmsCreateMode } from "./KmsCreateModal";

/// M6.39.9: right-edge KMS browser. Activated by clicking a KMS row's
/// title in the left sidebar. Lists `pages/*.md` and `sources/*.md`
/// files; clicking an entry opens [`KmsViewerOverlay`] over the main
/// pane. Mirrors the layout style of `ResearchSidebar` /
/// `TodoSidebar` — fixed-width right column, dismiss/restore via
/// chevron tab.
///
/// State protocol:
///   parent passes `kmsName` (the KMS being browsed) + `onClose`
///   (clears parent's `browsingKms` state) + `onOpenFile` (parent
///   tracks the file the viewer overlay should display).
///
/// The component subscribes to `kms_browse_result` envelopes for
/// the matching `kmsName` and re-renders when the listing arrives.
/// On mount or `kmsName` change, it sends `kms_browse` to fetch a
/// fresh listing.

type BrowseFile = {
  name: string;
  bytes: number;
};

type FileKind = "page" | "source";

export type ViewerTarget = {
  kms: string;
  kind: FileKind;
  name: string;
};

interface Props {
  kmsName: string;
  onClose: () => void;
  onOpenFile: (target: ViewerTarget) => void;
  onOpenGraph: (kms: string) => void;
  graphActive: boolean;
  /// The file currently open in the viewer overlay. When this row
  /// appears in the listing, it gets accent styling so the user can
  /// see at a glance which entry corresponds to what's on screen.
  /// Null when no file is open.
  selected: ViewerTarget | null;
}

export function KmsBrowserSidebar({
  kmsName,
  onClose,
  onOpenFile,
  onOpenGraph,
  graphActive,
  selected,
}: Props) {
  /// Compare a file row against the active viewer target. Limited to
  /// rows in the currently-browsed KMS — opening a file from KMS-A
  /// while browsing KMS-B should NOT highlight a same-named entry in
  /// KMS-B.
  const isSelected = (kind: FileKind, name: string) =>
    selected !== null &&
    selected.kms === kmsName &&
    selected.kind === kind &&
    selected.name === name;
  const [pages, setPages] = useState<BrowseFile[] | null>(null);
  const [sources, setSources] = useState<BrowseFile[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState(false);
  // Page create / rename / delete modal (null = closed). The backend
  // re-emits kms_browse_result on success, so the existing subscription
  // refreshes the list.
  const [modal, setModal] = useState<KmsCreateMode | null>(null);
  // Right-click context menu on a page row (null = closed). Anchored to
  // the cursor; its right edge pins to the click x so it never spills
  // off the right-edge panel.
  const [pageMenu, setPageMenu] = useState<{
    name: string;
    x: number;
    y: number;
  } | null>(null);

  useEffect(() => {
    setPages(null);
    setSources([]);
    setError(null);
    setDismissed(false);
    const unsub = subscribe((msg) => {
      if (
        msg.type === "kms_browse_result" &&
        (msg.kms as string) === kmsName
      ) {
        if (msg.ok) {
          setPages((msg.pages as BrowseFile[]) ?? []);
          setSources((msg.sources as BrowseFile[]) ?? []);
          setError(null);
        } else {
          setError((msg.error as string) ?? "browse failed");
          setPages([]);
        }
      }
    });
    send({ type: "kms_browse", name: kmsName });
    return unsub;
  }, [kmsName]);

  if (dismissed) {
    return (
      <button
        type="button"
        onClick={() => setDismissed(false)}
        className="flex items-center justify-center shrink-0 border-l"
        style={{
          width: "20px",
          background: "var(--bg-secondary)",
          borderColor: "var(--border)",
          color: "var(--text-secondary)",
          cursor: "pointer",
        }}
        title={`Browse KMS: ${kmsName}`}
      >
        <ChevronRight size={14} style={{ transform: "rotate(180deg)" }} />
      </button>
    );
  }

  return (
    <div
      className="flex flex-col shrink-0 border-l"
      style={{
        width: "260px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{ borderColor: "var(--border)" }}
      >
        <div
          className="text-[10px] uppercase tracking-wider flex items-center gap-2 truncate"
          style={{ color: "var(--text-secondary)" }}
        >
          <BookOpen size={11} />
          <span className="truncate" title={kmsName}>
            KMS: {kmsName}
          </span>
        </div>
        <div className="flex items-center gap-1 shrink-0">
          <button
            type="button"
            onClick={() => setModal({ kind: "page", kms: kmsName })}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="New blank page in this KMS"
          >
            <Plus size={14} />
          </button>
          <button
            type="button"
            onClick={() => setDismissed(true)}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Hide (chevron tab restores)"
          >
            <ChevronRight size={14} />
          </button>
          <button
            type="button"
            onClick={onClose}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Close browser"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      <div className="flex-1 overflow-auto">
        {error && (
          <div
            className="px-3 py-3 text-xs"
            style={{ color: "var(--danger, #e06c75)" }}
          >
            {error}
          </div>
        )}
        {pages === null && !error && (
          <div
            className="px-3 py-3 text-xs italic"
            style={{ color: "var(--text-secondary)" }}
          >
            Loading…
          </div>
        )}
        {pages !== null && (
          <>
            <button
              type="button"
              onClick={() => onOpenGraph(kmsName)}
              className="flex items-center gap-2 w-full px-3 py-2 text-xs font-medium border-b transition-colors"
              style={{
                color: graphActive
                  ? "var(--accent, #61afef)"
                  : "var(--text-primary)",
                background: graphActive
                  ? "color-mix(in srgb, var(--accent, #61afef) 12%, transparent)"
                  : "transparent",
                borderColor: "var(--border)",
              }}
              onMouseEnter={(e) => {
                if (!graphActive)
                  (e.currentTarget as HTMLButtonElement).style.background =
                    "rgba(255,255,255,0.04)";
              }}
              onMouseLeave={(e) => {
                if (!graphActive)
                  (e.currentTarget as HTMLButtonElement).style.background =
                    "transparent";
              }}
              title="Open Obsidian-style graph view"
            >
              <Network size={13} />
              <span>Graph View</span>
              {graphActive && (
                <span
                  className="ml-auto text-[9px] uppercase tracking-wider"
                  style={{ opacity: 0.7 }}
                >
                  open
                </span>
              )}
            </button>
            <Section
              icon={<FileText size={11} />}
              title={`Pages (${pages.length})`}
            >
              {pages.length === 0 ? (
                <div
                  className="px-3 py-1 text-xs italic"
                  style={{ color: "var(--text-secondary)" }}
                >
                  No pages yet
                </div>
              ) : (
                pages.map((p) => (
                  <FileRow
                    key={p.name}
                    file={p}
                    active={isSelected("page", p.name)}
                    onClick={() =>
                      onOpenFile({ kms: kmsName, kind: "page", name: p.name })
                    }
                    onContextMenu={(e) => {
                      e.preventDefault();
                      setPageMenu({ name: p.name, x: e.clientX, y: e.clientY });
                    }}
                  />
                ))
              )}
            </Section>
            <Section
              icon={<Link2 size={11} />}
              title={`Sources (${sources.length})`}
            >
              {sources.length === 0 ? (
                <div
                  className="px-3 py-1 text-xs italic"
                  style={{ color: "var(--text-secondary)" }}
                >
                  No cached sources
                </div>
              ) : (
                sources.map((s) => (
                  <FileRow
                    key={s.name}
                    file={s}
                    active={isSelected("source", s.name)}
                    onClick={() =>
                      onOpenFile({
                        kms: kmsName,
                        kind: "source",
                        name: s.name,
                      })
                    }
                  />
                ))
              )}
            </Section>
          </>
        )}
      </div>
      {pageMenu && (
        <>
          <div
            className="fixed inset-0 z-[55]"
            onClick={() => setPageMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setPageMenu(null);
            }}
          />
          <div
            className="fixed z-[56] rounded border shadow-lg text-xs py-1"
            style={{
              right: Math.max(8, window.innerWidth - pageMenu.x),
              top: pageMenu.y,
              minWidth: "150px",
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
          >
            <button
              type="button"
              className="block w-full text-left px-3 py-1.5 hover:bg-white/10"
              onClick={() => {
                setModal({ kind: "rename", kms: kmsName, name: pageMenu.name });
                setPageMenu(null);
              }}
            >
              Rename…
            </button>
            <button
              type="button"
              className="block w-full text-left px-3 py-1.5 hover:bg-white/10"
              style={{ color: "var(--danger, #e06c75)" }}
              onClick={() => {
                setModal({ kind: "delete", kms: kmsName, name: pageMenu.name });
                setPageMenu(null);
              }}
            >
              Delete…
            </button>
          </div>
        </>
      )}
      {modal && (
        <KmsCreateModal mode={modal} onClose={() => setModal(null)} />
      )}
    </div>
  );
}

function Section({
  icon,
  title,
  children,
}: {
  icon: React.ReactNode;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="mb-2">
      <div
        className="flex items-center gap-1.5 px-3 py-1.5 font-semibold uppercase tracking-wider"
        style={{
          color: "var(--text-secondary)",
          fontSize: "10px",
          borderBottom: "1px solid var(--border)",
        }}
      >
        {icon}
        {title}
      </div>
      <div className="py-1">{children}</div>
    </div>
  );
}

function FileRow({
  file,
  onClick,
  active = false,
  onContextMenu,
}: {
  file: BrowseFile;
  onClick: () => void;
  active?: boolean;
  onContextMenu?: (e: React.MouseEvent) => void;
}) {
  /// Active row styling: 2px accent left-border + tinted bg + accent
  /// text + slightly heavier weight. The chosen tint (`color-mix`
  /// with the accent) reads as a soft highlight on both light and
  /// dark themes — same visual rhythm as the "Graph View" active
  /// button above.
  const activeBg = active
    ? "color-mix(in srgb, var(--accent, #61afef) 14%, transparent)"
    : "transparent";
  const textColor = active
    ? "var(--accent, #61afef)"
    : "var(--text-primary)";
  return (
    <button
      type="button"
      onClick={onClick}
      onContextMenu={onContextMenu}
      className="flex items-baseline justify-between w-full text-left px-3 py-1"
      style={{
        color: textColor,
        background: activeBg,
        borderLeft: active
          ? "2px solid var(--accent, #61afef)"
          : "2px solid transparent",
        fontWeight: active ? 600 : 400,
        cursor: "pointer",
      }}
      onMouseEnter={(e) => {
        if (!active)
          (e.currentTarget as HTMLButtonElement).style.background =
            "rgba(255,255,255,0.05)";
      }}
      onMouseLeave={(e) => {
        if (!active)
          (e.currentTarget as HTMLButtonElement).style.background = activeBg;
      }}
      title={active ? `${file.name} (currently viewing)` : file.name}
    >
      <span className="truncate flex-1 text-xs">{file.name}</span>
      <span
        className="ml-2 shrink-0"
        style={{
          color: active
            ? "var(--accent, #61afef)"
            : "var(--text-secondary)",
          fontSize: "9px",
          opacity: active ? 0.85 : 1,
        }}
      >
        {formatBytes(file.bytes)}
      </span>
    </button>
  );
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
  return `${(n / (1024 * 1024)).toFixed(1)}MB`;
}
