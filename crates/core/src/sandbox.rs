//! Filesystem sandbox: restricts file tool access to the startup directory
//! and its subdirectories. Prevents `../` escapes, absolute paths outside
//! the project, and symlink traversal.
//!
//! Set once at startup via `Sandbox::init()`. File tools call
//! `Sandbox::check(path)` before every filesystem operation.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

static SANDBOX_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

pub struct Sandbox;

impl Sandbox {
    /// Initialize (or re-initialize) the sandbox root. First call sets the
    /// root; subsequent calls update it so the GUI's "change directory"
    /// modal can re-point the sandbox before any tools run.
    ///
    /// Prefers `$THCLAWS_PROJECT_ROOT` if set — exported by SpawnTeammate so
    /// teammates spawned with `cd .worktrees/<name>` still treat the parent
    /// project as their writable region (matching Claude Code's
    /// `getOriginalCwd()` model). Without this override, a worktree teammate's
    /// sandbox would shrink to its worktree and shared artifacts at the
    /// project root would be denied.
    /// Falls back to current_dir for standalone (non-team) invocations.
    pub fn init() -> Result<()> {
        // A workspace host's agent shares the workspace's files (dev-plan/61);
        // a worktree teammate's project root is the next choice.
        let root_path = match (
            std::env::var("THCLAWS_WORKSPACE_ROOT"),
            std::env::var("THCLAWS_PROJECT_ROOT"),
        ) {
            (Ok(s), _) if !s.trim().is_empty() => PathBuf::from(s),
            (_, Ok(s)) if !s.is_empty() => PathBuf::from(s),
            _ => std::env::current_dir()?,
        };
        let root = root_path
            .canonicalize()
            .map_err(|e| Error::Config(format!("cannot canonicalize sandbox root: {e}")))?;
        *SANDBOX_ROOT.write().unwrap() = Some(root);
        Ok(())
    }

    /// Returns the active sandbox root directory.
    ///
    /// dev-plan/42: when a per-session working root is scoped (multiuser
    /// `--serve` — one `workspace-<id>/` per user), that takes priority
    /// over the process-global `SANDBOX_ROOT`. `SANDBOX_ROOT` is a single
    /// mutable global; in a shared multi-tenant process it can only hold
    /// one root, so resolving against it would let one user's path
    /// resolution land in another user's tree. The task-local root is
    /// per-session and follows the task across runtime threads. Single-
    /// tenant / desktop / CLI never scope it, so the global path is
    /// unchanged.
    pub fn root() -> Option<PathBuf> {
        if crate::workdir::workdir_is_scoped() {
            return Some(crate::workdir::current_workdir());
        }
        SANDBOX_ROOT.read().ok()?.clone()
    }

    /// Clear the global sandbox root back to its initial unset state.
    /// Tests that swap cwd via a `with_temp_cwd`-style helper must
    /// call this to drop the tempdir-scoped sandbox they set with
    /// `init()` — otherwise SANDBOX_ROOT keeps pointing at the
    /// restored saved_cwd and breaks every later tool test that
    /// expects the default unset state (no `init()` called in unit
    /// tests = allow-all branch). Regression caught 2026-05-30 from
    /// dev-plan/33 Task 16 image_gen tests cascading into ~38
    /// tool-test failures.
    pub fn reset() {
        if let Ok(mut w) = SANDBOX_ROOT.write() {
            *w = None;
        }
    }

    /// Validate a path for a write/mutate operation. In addition to the
    /// standard sandbox rules, this denies any path inside the `.thclaws/`
    /// directory at the sandbox root — that directory holds team state,
    /// settings, agent defs, and mailbox files and must not be rewritten by
    /// file tools. Teammate worktrees live at `.worktrees/<name>/` (sibling
    /// of `.thclaws/`) and are writable like any other project subdirectory.
    pub fn check_write(path: &str) -> Result<PathBuf> {
        let resolved = Self::check(path)?;
        if let Some(root) = Self::root() {
            return Self::enforce_write_policy(&root, resolved);
        }
        Ok(resolved)
    }

    fn enforce_write_policy(root: &Path, resolved: PathBuf) -> Result<PathBuf> {
        // finding 11: an agent's own folder sits under `.thclaws/bots/<slug>/`,
        // so the blanket deny below also locked an agent out of its own state —
        // Book Author could not write the `book.json` it reads every turn. That
        // folder IS the agent, so it is writable; the host's `.thclaws/` and a
        // sibling agent's folder stay denied.
        if let Some(agent) = crate::workdir::agent_dir() {
            // Both forms: `resolved` is canonical for a path that exists and a
            // normalized landing path for one that does not, and on macOS the
            // canonical form of a temp dir gains a `/private` prefix the raw
            // one lacks — comparing only one of them denies writes to new
            // files in the agent's own folder.
            let under = |a: &Path| resolved == a || resolved.starts_with(a);
            if under(&agent) || agent.canonicalize().is_ok_and(|a| under(&a)) {
                return Ok(resolved);
            }
        }
        let protected = root.join(".thclaws");
        if resolved == protected || resolved.starts_with(&protected) {
            return Err(Error::Tool(format!(
                "access denied: {} is inside .thclaws/ — that directory is reserved for team \
                 state (settings, agents, inboxes, tasks). Write shared artifacts to the \
                 project root or a subdirectory other than .thclaws/.",
                resolved.display()
            )));
        }
        Ok(resolved)
    }

    /// Validate and resolve a path. Returns the canonicalized absolute path
    /// if it's inside the sandbox, or an error if it escapes.
    ///
    /// Handles:
    /// - Relative paths: joined to sandbox root.
    /// - Absolute paths: checked directly.
    /// - `../` traversal: resolved by canonicalize, then boundary-checked.
    /// - Symlinks: canonicalize follows them, so a symlink pointing outside is denied.
    /// - New files (don't exist yet): parent directory is validated instead.
    pub fn check(path: &str) -> Result<PathBuf> {
        let Some(root) = Self::root() else {
            // No sandbox initialized — allow everything (backward compat).
            let p = Path::new(path);
            return if p.is_absolute() {
                Ok(p.to_path_buf())
            } else {
                Ok(crate::workdir::tool_base(path).join(p))
            };
        };
        // dev-plan/42: resolve relative paths against the per-session
        // working dir (task-local when scoped, else process cwd) — except an
        // agent's own `.thclaws/…`, which finding 11 resolves in its folder.
        let cwd = crate::workdir::tool_base(path);
        Self::validate_against(&root, &cwd, path)
    }

    /// Same algorithm as `check`, but rooted at an arbitrary directory
    /// instead of the global workspace `SANDBOX_ROOT`. Used by GUI Shell
    /// asset serving where the shell's folder is outside the workspace
    /// (e.g. `~/.config/thclaws/gui-shell/<id>/`) so the global check would
    /// reject it. Relative paths in `path` are resolved against `root`.
    ///
    /// Canonicalises `root` before validating so callers can pass
    /// non-canonical paths (e.g. tempdir paths on macOS where `/tmp`
    /// symlinks to `/private/tmp` — without this, the path's canonical
    /// form has `/private/` prefix while the un-canonical root doesn't,
    /// and the `starts_with` check spuriously fails).
    pub fn check_in(root: &Path, path: &str) -> Result<PathBuf> {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| Error::Tool(format!("cannot canonicalize shell root: {e}")))?;
        Self::validate_against(&canonical_root, &canonical_root, path)
    }

    /// dev-plan/35 multi-tenant: validate that `path` lives inside
    /// the user's writable subtree(s) under `project_root`. Two-stage
    /// check: (a) `check_in(project_root, path)` to confirm it's at
    /// least inside the project, (b)
    /// [`crate::multi_tenant::user_state::is_in_user_writable`] to
    /// confirm it's also in one of `<project>/output/users/<id>/` or
    /// `<project>/.thclaws/users/<id>/`. Rejects shared assets
    /// (AGENTS.md, kms/, settings.json) and other users' subtrees.
    ///
    /// `paths` is the resolved [`UserStatePaths`] for the
    /// authenticated user — same instance the IPC dispatch builds
    /// once per request.
    pub fn check_write_for_user(
        project_root: &Path,
        paths: &crate::multi_tenant::user_state::UserStatePaths,
        path: &str,
    ) -> Result<PathBuf> {
        let resolved = Self::check_in(project_root, path)?;
        if !crate::multi_tenant::user_state::is_in_user_writable(paths, &resolved) {
            return Err(Error::Tool(format!(
                "access denied: '{}' is outside the per-user writable subtree \
                 (allowed: '{}/' and '{}/' for this user). In multi-tenant \
                 mode, shared project assets (AGENTS.md, kms/, settings.json) \
                 and other users' subtrees are read-only.",
                resolved.display(),
                paths.output_root.display(),
                paths.thclaws_user_root.display(),
            )));
        }
        Ok(resolved)
    }

    fn validate_against(root: &Path, cwd: &Path, path: &str) -> Result<PathBuf> {
        // Resolve relative paths from cwd, not root. A teammate in
        // .worktrees/backend/ calling Write("src/server.ts") must land the
        // file in its worktree (where it stays on team/backend), not in the
        // workspace's src/ (which is on main). Joining to root would silently
        // route every relative write onto main, breaking branch isolation.
        let initial = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            cwd.join(path)
        };

        // Lexically resolve `..` and `.` without touching the FS. Required
        // because the parent-walk below checks each *existing* ancestor for
        // containment, but `cwd/../outside.txt` has cwd as its parent (which
        // is inside the sandbox) yet points outside. Normalizing first means
        // the eventual ancestor check is meaningful.
        let resolved = lexical_normalize(&initial);

        // Existing path: canonicalize so symlinks pointing outside are caught.
        if let Ok(canonical) = resolved.canonicalize() {
            return if canonical.starts_with(root) {
                Ok(canonical)
            } else {
                Err(Self::denied(&canonical, root))
            };
        }

        // Path doesn't exist yet (e.g. Write into a deep new tree like
        // `src/api/handlers/auth.ts` where `src/api/handlers/` isn't there).
        // The Write tool will `create_dir_all` the parent — we just need to
        // confirm the path lands inside the sandbox. `resolve_landing` walks
        // to the longest existing ancestor AND expands any symlink in the
        // tail: this used to assume a non-existent tail could hold no
        // symlink, which a dangling one disproves — it exists as a link, so
        // `ws/dangling -> /outside/x.txt` passed the check and the write
        // landed outside the workspace (issue #219).
        let landing = resolve_landing(&resolved);
        if landing.starts_with(root) {
            return Ok(landing);
        }
        Err(Self::denied(&landing, root))
    }

    fn denied(path: &Path, root: &Path) -> Error {
        // Keep the "access denied" prefix (tests + callers match on it) but
        // spell out that this is a workspace-boundary limit, not a
        // permission/approval gate. Weak models otherwise paraphrase the
        // terse old message as "rejected by the security policy even though
        // you approved" (issue #119), which sends users hunting in
        // settings.json for a permission that was never the problem.
        Error::Tool(format!(
            "access denied: '{}' is outside the workspace root '{}'. File \
             tools and the Bash working directory are confined to the \
             workspace — this is a path boundary, NOT a permission/approval \
             issue (approving a tool does not widen it). Use a path inside \
             the workspace, or relaunch thClaws with a root that contains \
             this path.",
            path.display(),
            root.display()
        ))
    }
}

/// Resolve `..` and `.` components lexically (no filesystem access).
/// Used by `validate_against` to make the parent-walk meaningful when the
/// target path doesn't exist yet — without this, `cwd/../outside.txt`
/// would falsely pass containment checks because cwd itself is inside the
/// sandbox. Shared with `subagent`'s `writePaths` scoping, which needs the
/// same resolution before its globs see a path.
/// A symlink chain longer than this is a loop, not a path.
const MAX_SYMLINK_HOPS: usize = 32;

/// Where a path will actually LAND on disk: `..`/`.` collapsed and every
/// symlink followed — including a **dangling** one, whose target does not
/// exist even though the link itself very much does.
///
/// `canonicalize` refuses a dangling final component, and the obvious
/// fallback — canonicalise the longest existing ancestor and re-join the
/// tail — rests on "the tail cannot contain a symlink, it does not exist".
/// A dangling symlink is exactly the counter-example: `ws/dangling ->
/// /outside/x.txt` made the tail `dangling`, the ancestor `ws`, and the
/// answer `ws/dangling`, which is inside the sandbox while the write landed
/// outside it (issue #219). So walk the tail a component at a time and
/// expand any link found there.
pub(crate) fn resolve_landing(path: &Path) -> PathBuf {
    let lexical = lexical_normalize(path);
    if let Ok(canonical) = lexical.canonicalize() {
        return canonical;
    }
    let mut ancestor = lexical.parent();
    let (base, tail) = loop {
        match ancestor {
            Some(p) => {
                if let Ok(canonical) = p.canonicalize() {
                    let tail = lexical
                        .strip_prefix(p)
                        .unwrap_or(Path::new(""))
                        .to_path_buf();
                    break (canonical, tail);
                }
                ancestor = p.parent();
            }
            None => return lexical,
        }
    };

    let mut out = base;
    let mut hops = 0usize;
    for comp in tail.components() {
        out.push(comp);
        while hops < MAX_SYMLINK_HOPS {
            let Ok(meta) = std::fs::symlink_metadata(&out) else {
                break;
            };
            if !meta.file_type().is_symlink() {
                break;
            }
            let Ok(target) = std::fs::read_link(&out) else {
                break;
            };
            out = if target.is_absolute() {
                target
            } else {
                out.parent().map(|p| p.join(&target)).unwrap_or(target)
            };
            out = lexical_normalize(&out);
            if let Ok(canonical) = out.canonicalize() {
                out = canonical;
            }
            hops += 1;
        }
    }
    out
}

pub(crate) fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {

    /// Issue #219: a dangling symlink exists as a LINK even though
    /// `canonicalize` refuses it, so treating the non-existent tail as
    /// symlink-free let a write leave the workspace entirely. Reproduced
    /// against the real binary before this fix: the file landed outside.
    #[test]
    fn a_dangling_symlink_cannot_carry_a_write_out_of_the_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let ws = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // The link exists; its target does not.
        let link = ws.join("dangling");
        std::os::unix::fs::symlink(outside.join("escaped.txt"), &link).unwrap();
        assert!(link.symlink_metadata().is_ok(), "the link itself exists");
        assert!(link.canonicalize().is_err(), "but it does not canonicalise");

        assert_eq!(
            resolve_landing(&link),
            outside.join("escaped.txt"),
            "the write lands where the link points, not where it sits"
        );
        assert!(
            Sandbox::validate_against(&ws, &ws, "dangling").is_err(),
            "so the sandbox must refuse it"
        );

        // A dangling link that stays inside is still allowed — the rule is
        // about where it lands, not that it dangles.
        let inside = ws.join("inside-link");
        std::os::unix::fs::symlink(ws.join("new.txt"), &inside).unwrap();
        assert_eq!(resolve_landing(&inside), ws.join("new.txt"));
        assert!(Sandbox::validate_against(&ws, &ws, "inside-link").is_ok());

        // An ordinary new file in a tree that does not exist yet still
        // resolves — that is the case the old code was written for.
        assert_eq!(
            resolve_landing(&ws.join("a/b/c.txt")),
            ws.join("a/b/c.txt"),
            "a genuinely absent tail is joined, not rejected"
        );

        // A symlink loop terminates instead of hanging.
        let a = ws.join("loop-a");
        let b = ws.join("loop-b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        let _ = resolve_landing(&a);
    }

    use super::*;
    use tempfile::tempdir;

    /// Helper: run a test with a temporary root directory, calling
    /// `validate_against` directly so tests don't fight over the global.
    fn with_sandbox<F>(f: F)
    where
        F: FnOnce(&Path),
    {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        f(&root);
    }

    #[test]
    fn relative_path_resolves_inside_sandbox() {
        with_sandbox(|root| {
            std::fs::write(root.join("hello.txt"), "hi").unwrap();
            let result = Sandbox::validate_against(root, root, "hello.txt").unwrap();
            assert!(result.starts_with(root));
            assert!(result.ends_with("hello.txt"));
        });
    }

    #[test]
    fn absolute_path_inside_sandbox_allowed() {
        with_sandbox(|root| {
            let file = root.join("abs.txt");
            std::fs::write(&file, "").unwrap();
            let result = Sandbox::validate_against(root, root, file.to_str().unwrap()).unwrap();
            assert!(result.starts_with(root));
        });
    }

    #[test]
    fn dotdot_escape_denied() {
        with_sandbox(|root| {
            let err = Sandbox::validate_against(root, root, "../../etc/passwd").unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("access denied") || msg.contains("not accessible"),
                "got: {msg}"
            );
        });
    }

    #[test]
    fn absolute_path_outside_denied() {
        with_sandbox(|root| {
            let err = Sandbox::validate_against(root, root, "/etc/passwd").unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("access denied") || msg.contains("not accessible"),
                "got: {msg}"
            );
        });
    }

    #[test]
    fn new_file_in_sandbox_allowed() {
        with_sandbox(|root| {
            let result = Sandbox::validate_against(root, root, "new_file.txt").unwrap();
            assert!(result.starts_with(root));
            assert!(result.ends_with("new_file.txt"));
        });
    }

    // Issue #119: the denied message must read as a workspace-boundary
    // limit, not a permission gate, so weak models stop paraphrasing it
    // as "rejected by the security policy even though you approved".
    #[test]
    fn denied_message_names_workspace_boundary_not_permission() {
        with_sandbox(|root| {
            let err = Sandbox::validate_against(root, root, "/etc/passwd").unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("access denied"), "keeps prefix; got: {msg}");
            assert!(
                msg.contains("outside the workspace root"),
                "names the boundary; got: {msg}"
            );
            assert!(
                msg.contains("NOT a permission"),
                "disclaims the permission framing; got: {msg}"
            );
        });
    }

    #[test]
    fn new_file_outside_denied() {
        with_sandbox(|root| {
            let outside = format!("{}/../outside.txt", root.display());
            let err = Sandbox::validate_against(root, root, &outside).unwrap_err();
            assert!(
                format!("{err}").contains("access denied")
                    || format!("{err}").contains("not accessible")
            );
        });
    }

    #[test]
    fn subdirectory_access_allowed() {
        with_sandbox(|root| {
            let sub = root.join("sub/dir");
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("deep.txt"), "").unwrap();
            let result = Sandbox::validate_against(root, root, "sub/dir/deep.txt").unwrap();
            assert!(result.starts_with(root));
        });
    }

    /// A worktree teammate's cwd is `.worktrees/<name>/` but its sandbox
    /// root is the workspace. Relative writes must land in the worktree
    /// (so they stay on `team/<name>`), not in the workspace root (which
    /// would put them on `main`).
    #[test]
    fn relative_path_resolves_from_cwd_not_root() {
        with_sandbox(|root| {
            let worktree = root.join(".worktrees/backend");
            std::fs::create_dir_all(worktree.join("src")).unwrap();
            let result = Sandbox::validate_against(root, &worktree, "src/server.ts").unwrap();
            assert!(
                result.starts_with(&worktree),
                "expected resolution under worktree, got {}",
                result.display()
            );
            assert!(result.ends_with("src/server.ts"));
        });
    }

    /// From a worktree cwd, escaping with ../../ to write a shared artifact
    /// at the workspace root is allowed (still inside the sandbox boundary).
    #[test]
    fn worktree_dotdot_into_workspace_allowed() {
        with_sandbox(|root| {
            let worktree = root.join(".worktrees/backend");
            std::fs::create_dir_all(root.join("docs")).unwrap();
            std::fs::create_dir_all(&worktree).unwrap();
            let result =
                Sandbox::validate_against(root, &worktree, "../../docs/api-spec.md").unwrap();
            assert!(
                result.starts_with(root) && !result.starts_with(&worktree),
                "expected resolution at workspace root, got {}",
                result.display()
            );
        });
    }

    /// Write tool calls `create_dir_all(parent)` so it can target a path
    /// whose intermediate directories don't exist yet. Sandbox::check_write
    /// must not reject those paths just because the immediate parent is
    /// missing — otherwise backend can't write `src/api/handlers/auth.ts`
    /// in a fresh worktree, which is the exact failure mode that bit us.
    #[test]
    fn deep_new_path_allowed_when_intermediate_dirs_missing() {
        with_sandbox(|root| {
            let result = Sandbox::validate_against(root, root, "src/api/handlers/auth.ts").unwrap();
            assert!(result.starts_with(root));
            assert!(result.ends_with("src/api/handlers/auth.ts"));
        });
    }

    /// Same case from a worktree cwd: backend's relative `Write("src/foo.ts")`
    /// where neither `src/` nor any parent exists in its worktree yet.
    #[test]
    fn worktree_deep_new_path_allowed() {
        with_sandbox(|root| {
            let worktree = root.join(".worktrees/backend");
            std::fs::create_dir_all(&worktree).unwrap();
            let result =
                Sandbox::validate_against(root, &worktree, "src/api/handlers/auth.ts").unwrap();
            assert!(result.starts_with(&worktree));
        });
    }

    /// `..` must not slip past the parent-walk into a non-existing path
    /// segment — `cwd/../outside.txt` could be falsely accepted if we only
    /// canonicalized the existing parent (cwd) without normalizing `..`
    /// in the resolved path first.
    #[test]
    fn dotdot_in_non_existing_path_is_normalized_and_denied() {
        with_sandbox(|root| {
            let outside = format!("{}/../outside.txt", root.display());
            let err = Sandbox::validate_against(root, root, &outside).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("access denied") || msg.contains("not accessible"),
                "got: {msg}"
            );
        });
    }

    /// From a worktree cwd, an absolute path pointing back into the workspace
    /// is also allowed — the canonical form is inside the sandbox.
    #[test]
    fn worktree_absolute_workspace_path_allowed() {
        with_sandbox(|root| {
            let worktree = root.join(".worktrees/backend");
            let docs = root.join("docs");
            std::fs::create_dir_all(&docs).unwrap();
            std::fs::create_dir_all(&worktree).unwrap();
            let target = docs.join("api-spec.md");
            let result =
                Sandbox::validate_against(root, &worktree, target.to_str().unwrap()).unwrap();
            assert!(result.starts_with(root));
        });
    }

    #[test]
    fn write_denied_inside_thclaws() {
        with_sandbox(|root| {
            let settings = root.join(".thclaws/settings.json");
            let err = Sandbox::enforce_write_policy(root, settings).unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("access denied"), "got: {msg}");
            assert!(msg.contains(".thclaws/"), "got: {msg}");
        });
    }

    #[test]
    fn write_allowed_inside_worktree() {
        with_sandbox(|root| {
            let file = root.join(".worktrees/backend/src/lib.rs");
            let result = Sandbox::enforce_write_policy(root, file.clone()).unwrap();
            assert_eq!(result, file);
        });
    }

    #[test]
    fn write_allowed_outside_thclaws() {
        with_sandbox(|root| {
            let file = root.join("src/main.rs");
            let result = Sandbox::enforce_write_policy(root, file.clone()).unwrap();
            assert_eq!(result, file);
        });
    }

    #[test]
    fn symlink_escape_denied() {
        with_sandbox(|root| {
            let link = root.join("escape_link");
            #[cfg(unix)]
            std::os::unix::fs::symlink("/tmp", &link).unwrap();
            #[cfg(unix)]
            {
                let err =
                    Sandbox::validate_against(root, root, "escape_link/something").unwrap_err();
                let msg = format!("{err}");
                assert!(
                    msg.contains("access denied") || msg.contains("not accessible"),
                    "got: {msg}"
                );
            }
        });
    }

    /// `check_in` works for roots completely unrelated to the global
    /// `SANDBOX_ROOT` — the GUI Shell case where the shell folder lives
    /// outside the workspace (e.g. `~/.config/thclaws/gui-shell/<id>/`).
    #[test]
    fn check_in_resolves_inside_arbitrary_root() {
        with_sandbox(|root| {
            std::fs::write(root.join("index.html"), "<!doctype html>").unwrap();
            let result = Sandbox::check_in(root, "index.html").unwrap();
            assert!(result.starts_with(root));
            assert!(result.ends_with("index.html"));
        });
    }

    #[test]
    fn check_in_denies_dotdot_escape() {
        with_sandbox(|root| {
            let err = Sandbox::check_in(root, "../../etc/passwd").unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("access denied") || msg.contains("not accessible"),
                "got: {msg}"
            );
        });
    }

    /// dev-plan/35 multi-tenant: a per-user write check accepts paths
    /// in the user's own `output/users/<id>/` and
    /// `.thclaws/users/<id>/` subtrees, and rejects shared assets +
    /// other users' subtrees.
    #[test]
    fn check_write_for_user_isolates_per_user_subtrees() {
        use crate::multi_tenant::{user_state::UserStatePaths, UserId};
        with_sandbox(|root| {
            // Real on-disk per-user subtrees so canonicalize() works
            // and the starts_with check meets the canonical root.
            std::fs::create_dir_all(root.join("output/users/alice")).unwrap();
            std::fs::create_dir_all(root.join(".thclaws/users/alice")).unwrap();
            std::fs::create_dir_all(root.join("output/users/bob")).unwrap();
            std::fs::create_dir_all(root.join(".thclaws/users/bob")).unwrap();

            let alice = UserStatePaths::new(root, &UserId::new_for_test("alice"));

            // OK: alice writes into her output subtree.
            let ok = Sandbox::check_write_for_user(root, &alice, "output/users/alice/img.png");
            assert!(ok.is_ok(), "alice → her output: {ok:?}");

            // OK: alice writes into her .thclaws subtree.
            let ok = Sandbox::check_write_for_user(
                root,
                &alice,
                ".thclaws/users/alice/storage/sess.json",
            );
            assert!(ok.is_ok(), "alice → her storage: {ok:?}");

            // REJECT: alice writes to shared AGENTS.md (read-only).
            let err = Sandbox::check_write_for_user(root, &alice, "AGENTS.md").unwrap_err();
            assert!(format!("{err}").contains("outside the per-user writable subtree"));

            // REJECT: alice writes to bob's output.
            let err = Sandbox::check_write_for_user(root, &alice, "output/users/bob/img.png")
                .unwrap_err();
            assert!(format!("{err}").contains("outside the per-user writable subtree"));

            // REJECT: alice writes to bob's storage.
            let err =
                Sandbox::check_write_for_user(root, &alice, ".thclaws/users/bob/storage/leak.json")
                    .unwrap_err();
            assert!(format!("{err}").contains("outside the per-user writable subtree"));

            // REJECT: alice writes to shared output.
            let err = Sandbox::check_write_for_user(root, &alice, "output/shared.png").unwrap_err();
            assert!(format!("{err}").contains("outside the per-user writable subtree"));
        });
    }

    // dev-plan/42 security proof: in a multiuser process two concurrent
    // sessions resolve against their OWN workspace. `Sandbox::root()`
    // returns the per-session task-local root and never falls through to
    // the shared process-global `SANDBOX_ROOT` (the scoped branch returns
    // first), and interleaving (yield mid-task on a multi-thread runtime)
    // can't make one session see another's root. Deliberately does NOT
    // touch the global root — that would pollute it for sibling tests
    // (the codebase convention; see `with_sandbox`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_sessions_isolate_workdir_roots() {
        let alice = tempdir().unwrap();
        let bob = tempdir().unwrap();
        let ap = alice.path().to_path_buf();
        let bp = bob.path().to_path_buf();

        // Two sessions, each scoped to its own workspace, yielding mid-task
        // to force interleaving across the runtime's worker threads. That
        // `root()` returns the scoped dir at all proves the scoped branch
        // fired instead of consulting the global.
        let a = crate::workdir::scope_workdir(ap.clone(), async {
            tokio::task::yield_now().await;
            Sandbox::root().unwrap()
        });
        let b = crate::workdir::scope_workdir(bp.clone(), async {
            tokio::task::yield_now().await;
            Sandbox::root().unwrap()
        });
        let (ra, rb) = tokio::join!(a, b);

        assert_eq!(ra, ap, "alice's turn resolves into alice's workspace");
        assert_eq!(rb, bp, "bob's turn resolves into bob's workspace");
        assert_ne!(ra, rb, "no cross-tenant leakage under interleaving");
    }
}
