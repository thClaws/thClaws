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
    /// Initialize (or re-initialize) the sandbox to the current working
    /// directory. First call sets the root; subsequent calls update it so
    /// the GUI's "change directory" modal can re-point the sandbox before
    /// any tools run.
    pub fn init() -> Result<()> {
        let root = std::env::current_dir()?
            .canonicalize()
            .map_err(|e| Error::Config(format!("cannot canonicalize cwd: {e}")))?;
        *SANDBOX_ROOT.write().unwrap() = Some(root);
        Ok(())
    }

    /// Returns a clone of the sandbox root directory.
    pub fn root() -> Option<PathBuf> {
        SANDBOX_ROOT.read().ok()?.clone()
    }

    /// Validate a path for a write/mutate operation. In addition to the
    /// standard sandbox rules, this denies any path inside the `.thclaws/`
    /// directory at the sandbox root — that directory holds team state,
    /// settings, agent defs, and mailbox files and must not be rewritten by
    /// file tools.
    pub fn check_write(path: &str) -> Result<PathBuf> {
        let resolved = Self::check(path)?;
        if let Some(root) = Self::root() {
            let protected = root.join(".thclaws");
            if resolved == protected || resolved.starts_with(&protected) {
                return Err(Error::Tool(format!(
                    "access denied: {} is inside .thclaws/ — that directory is reserved for team \
                     state (settings, agents, inboxes, tasks). Write shared artifacts to the \
                     project root or a subdirectory other than .thclaws/.",
                    resolved.display()
                )));
            }
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
        Self::validate_against(&root, path)
    }

    fn validate_against(root: &Path, path: &str) -> Result<PathBuf> {
        let resolved = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            root.join(path)
        };

        // Existing file/dir: canonicalize resolves symlinks + ../
        if let Ok(canonical) = resolved.canonicalize() {
            return if canonical.starts_with(root) {
                Ok(canonical)
            } else {
                Err(Self::denied(&canonical, root))
            };
        }

        // File doesn't exist yet (e.g. Write creating a new file).
        // Validate the parent directory instead.
        if let Some(parent) = resolved.parent() {
            if let Ok(canonical_parent) = parent.canonicalize() {
                if canonical_parent.starts_with(root) {
                    let file_name = resolved
                        .file_name()
                        .ok_or_else(|| Error::Tool("invalid path: no filename".into()))?;
                    return Ok(canonical_parent.join(file_name));
                }
                return Err(Self::denied(&canonical_parent, root));
            }
        }

        Err(Error::Tool(format!(
            "path not accessible: {}",
            resolved.display()
        )))
    }

    fn denied(path: &Path, root: &Path) -> Error {
        Error::Tool(format!(
            "access denied: {} is outside the project directory {}",
            path.display(),
            root.display()
        ))
    }
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
            let result = Sandbox::validate_against(root, "hello.txt").unwrap();
            assert!(result.starts_with(root));
            assert!(result.ends_with("hello.txt"));
        });
    }

    #[test]
    fn absolute_path_inside_sandbox_allowed() {
        with_sandbox(|root| {
            let file = root.join("abs.txt");
            std::fs::write(&file, "").unwrap();
            let result = Sandbox::validate_against(root, file.to_str().unwrap()).unwrap();
            assert!(result.starts_with(root));
        });
    }

    #[test]
    fn dotdot_escape_denied() {
        with_sandbox(|root| {
            let err = Sandbox::validate_against(root, "../../etc/passwd").unwrap_err();
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
            let err = Sandbox::validate_against(root, "/etc/passwd").unwrap_err();
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
            let result = Sandbox::validate_against(root, "new_file.txt").unwrap();
            assert!(result.starts_with(root));
            assert!(result.ends_with("new_file.txt"));
        });
    }

    #[test]
    fn new_file_outside_denied() {
        with_sandbox(|root| {
            let outside = format!("{}/../outside.txt", root.display());
            let err = Sandbox::validate_against(root, &outside).unwrap_err();
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
            let result = Sandbox::validate_against(root, "sub/dir/deep.txt").unwrap();
            assert!(result.starts_with(root));
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
                let err = Sandbox::validate_against(root, "escape_link/something").unwrap_err();
                let msg = format!("{err}");
                assert!(
                    msg.contains("access denied") || msg.contains("not accessible"),
                    "got: {msg}"
                );
            }
        });
    }
}
