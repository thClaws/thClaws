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
        let root_path = match std::env::var("THCLAWS_PROJECT_ROOT") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => std::env::current_dir()?,
        };
        let root = root_path
            .canonicalize()
            .map_err(|e| Error::Config(format!("cannot canonicalize sandbox root: {e}")))?;
        *SANDBOX_ROOT.write().unwrap() = Some(root);
        Ok(())
    }

    /// Returns a clone of the sandbox root directory.
    pub fn root() -> Option<PathBuf> {
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
                Ok(std::env::current_dir()?.join(p))
            };
        };
        let cwd =
            std::env::current_dir().map_err(|e| Error::Tool(format!("cannot read cwd: {e}")))?;
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
        // confirm the path lands inside the sandbox. Walk up to the longest
        // existing ancestor and canonicalize THAT (catches symlinks); since
        // the non-existing tail can't itself contain symlinks (it doesn't
        // exist), and we already lexically resolved `..`, joining is safe.
        let mut ancestor = resolved.parent();
        while let Some(p) = ancestor {
            if let Ok(canonical_anc) = p.canonicalize() {
                if !canonical_anc.starts_with(root) {
                    return Err(Self::denied(&canonical_anc, root));
                }
                let tail = resolved.strip_prefix(p).unwrap_or(Path::new(""));
                return Ok(canonical_anc.join(tail));
            }
            ancestor = p.parent();
        }

        Err(Error::Tool(format!(
            "path not accessible: {}",
            resolved.display()
        )))
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
/// sandbox.
fn lexical_normalize(p: &Path) -> PathBuf {
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
}
