//! dev-plan/49: OS-level confinement of the Bash subprocess.
//!
//! The `pre_tool_use` hook (dev-plan/48) screens a command *string* — a
//! tripwire that obfuscation (`$(printf rm)`, `eval`, `base64|sh`) defeats —
//! and `bash.rs` only confines the `cwd` argument, not the command's file ops
//! (`echo x > /abs/path` escapes). This module adds the **hard** layer: it
//! wraps the `sh -c <command>` invocation so the OS itself enforces "writes
//! only inside the workspace + a cache/tmp allowlist; sensitive dotfiles
//! unreadable", regardless of how the command is written.
//!
//! - **macOS:** `sandbox-exec` (Seatbelt) profile — allow-by-default, then
//!   `(deny file-write*)` except the write-roots, plus `(deny file-read*)` on
//!   secrets.
//! - **Linux:** **Landlock** (an LSM needing no user namespace, so it works
//!   where `bwrap` is AppArmor-blocked) — the engine re-execs itself as a hidden
//!   `__confine` helper that installs a write-confinement ruleset then `exec`s
//!   `sh -c`. Falls back to `bubblewrap`, then unconfined.
//! - **Other / can't enforce:** passthrough (logged once) — never a hard fail.
//!   A confiner is probed at runtime (binary/ABI present ≠ usable).
//!
//! Modes (settings.json `bash.sandbox`): `workspace` (**default** — workspace +
//! tmp + package-manager caches), `strict` (workspace + tmp only), or `off`.
//! The mode is process-global (a pod-level policy); the workspace
//! root is resolved **per call** from [`crate::sandbox::Sandbox::root`] /
//! [`crate::workdir`] so multi-tenant sessions confine to their own folder.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfineMode {
    #[default]
    Off,
    Workspace,
    Strict,
}

impl ConfineMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "workspace" | "on" => Self::Workspace,
            "strict" => Self::Strict,
            _ => Self::Off,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Workspace => "workspace",
            Self::Strict => "strict",
        }
    }
}

/// Resolved policy for one spawn: which roots may be written, which paths must
/// not be readable.
#[derive(Debug, Clone)]
pub struct ConfinePolicy {
    pub mode: ConfineMode,
    pub write_roots: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
    /// Read ALLOWLIST (dev-plan/45): `Some` ⇒ reads/execs are restricted to
    /// these roots (plus the write roots) at the OS level. Landlock has no
    /// deny-rule, only grants — so cross-user read masking is expressed as
    /// an allowlist. Active in multiuser pods only: the grant set is the
    /// user's own workspace + system dirs, deliberately WITHOUT `/proc`
    /// (blocks `/proc/<pid>/environ` credential exfil between same-uid
    /// processes) and WITHOUT the shared workspace parent (blocks reading
    /// co-tenants' `workspace-<id>/`). `None` ⇒ reads unrestricted (desktop
    /// behaviour unchanged; the dotfile `deny_read` list above still applies
    /// via Seatbelt/bwrap).
    pub read_roots: Option<Vec<PathBuf>>,
}

struct ConfineSettings {
    mode: ConfineMode,
    extra_write: Vec<PathBuf>,
    extra_deny_read: Vec<PathBuf>,
}

/// Process-global confinement settings, lazily read from the layered config
/// once (the mode is a pod-level policy; it doesn't change mid-process). A
/// test override short-circuits the load.
fn settings() -> &'static ConfineSettings {
    static S: OnceLock<ConfineSettings> = OnceLock::new();
    S.get_or_init(|| {
        // The test harness asserts raw filesystem behavior — never confine it.
        if cfg!(test) {
            return ConfineSettings {
                mode: ConfineMode::Off,
                extra_write: Vec::new(),
                extra_deny_read: Vec::new(),
            };
        }
        let cfg = crate::config::AppConfig::load().unwrap_or_default();
        // `THCLAWS_BASH_SANDBOX` env wins over config (CI / power-user escape
        // hatch, e.g. `=off`).
        let mode = std::env::var("THCLAWS_BASH_SANDBOX")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| ConfineMode::parse(&s))
            .unwrap_or_else(|| ConfineMode::parse(&cfg.bash_sandbox));
        ConfineSettings {
            mode,
            extra_write: cfg.bash_sandbox_write_paths.iter().map(expand).collect(),
            extra_deny_read: cfg.bash_sandbox_deny_read.iter().map(expand).collect(),
        }
    })
}

pub fn mode() -> ConfineMode {
    settings().mode
}

/// `(mode, enforced)` for the audit record: `enforced` is false when the
/// mode is `off`, the platform has no confiner, or the confiner bailed at
/// runtime and Bash fell back to unconfined.
pub fn enforcement_state() -> (ConfineMode, bool) {
    let m = mode();
    let platform_ok = cfg!(any(target_os = "macos", target_os = "linux"));
    let failed = CONFINE_RUNTIME_FAILED.load(std::sync::atomic::Ordering::Relaxed);
    (m, m != ConfineMode::Off && platform_ok && !failed)
}

fn expand(s: &String) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = crate::util::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}

/// Resolve the active policy for the current call, or `None` when confinement
/// is off. The workspace root is resolved here (not cached) so multi-tenant
/// sessions get their own writable root.
pub fn current_policy() -> Option<ConfinePolicy> {
    let s = settings();
    if s.mode == ConfineMode::Off {
        return None;
    }
    let workspace = crate::sandbox::Sandbox::root().unwrap_or_else(crate::workdir::current_workdir);
    Some(build_policy(
        s.mode,
        &workspace,
        &s.extra_write,
        &s.extra_deny_read,
        crate::workdir::is_multiuser(),
    ))
}

/// System roots granted for reads in read-mask (multiuser) mode. Everything a
/// toolchain legitimately reads — binaries, libs, config, cgroup limits —
/// WITHOUT `/proc` (env exfil) and WITHOUT the workspace parent (co-tenants).
const READ_MASK_SYSTEM_ROOTS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/var", "/run", "/srv",
    "/sys", "/nix",
];

/// Pure policy builder (testable without the global): workspace + tmp always
/// writable; `workspace` mode adds the package-manager cache allowlist;
/// `strict` adds nothing. Secret dotfiles are always read-denied.
/// `read_mask` (multiuser pods) additionally restricts reads to an allowlist
/// — see [`ConfinePolicy::read_roots`].
pub fn build_policy(
    mode: ConfineMode,
    workspace: &Path,
    extra_write: &[PathBuf],
    extra_deny_read: &[PathBuf],
    read_mask: bool,
) -> ConfinePolicy {
    let mut write_roots: Vec<PathBuf> = vec![canon(workspace)];
    // tmp is always writable (build tools, $TMPDIR).
    if let Some(t) = std::env::var_os("TMPDIR") {
        write_roots.push(PathBuf::from(t));
    }
    write_roots.push(PathBuf::from("/tmp"));
    write_roots.push(PathBuf::from("/private/tmp")); // macOS resolves /tmp here

    // Character device nodes tons of tools open read/write (git, compilers,
    // anything redirecting to /dev/null). Writing these is not a filesystem
    // escape — /dev/null discards, /dev/tty is the existing terminal — so
    // they're allowed in every confined mode. Raw block devices (/dev/disk*)
    // are deliberately NOT here.
    for dev in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd",
        "/dev/dtracehelper", // macOS — some runtimes open it
        "/dev/ptmx",
    ] {
        write_roots.push(PathBuf::from(dev));
    }

    // GPU/accelerator nodes — NVML/CUDA open these read-write, so confined
    // commands on DGX/Jetson fail NVML init ("Unknown Error") without them.
    // Char devices again, not a filesystem escape.
    if let Ok(rd) = std::fs::read_dir("/dev") {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("nvidia") {
                write_roots.push(e.path());
            }
        }
    }
    write_roots.push(PathBuf::from("/dev/dri"));

    let home = crate::util::home_dir();
    if mode == ConfineMode::Workspace {
        if let Some(h) = &home {
            // Package-manager / toolchain caches so pip/npm/cargo/etc. work.
            for c in [
                ".cache",
                ".npm",
                ".pnpm-store",
                ".yarn",
                ".cargo",
                ".rustup",
                ".pyenv",
                ".local/share/virtualenvs",
                ".local/share/uv",
                ".gradle",
                ".m2",
                "Library/Caches", // macOS
            ] {
                write_roots.push(h.join(c));
            }
        }
    }
    write_roots.extend(extra_write.iter().cloned());

    // Read-deny secrets in both confined modes.
    let mut deny_read: Vec<PathBuf> = Vec::new();
    if let Some(h) = &home {
        for d in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/thclaws",
            ".config/gcloud",
            ".config/gh",
            ".kube",
            ".docker",
            ".netrc",
        ] {
            deny_read.push(h.join(d));
        }
    }
    deny_read.extend(extra_deny_read.iter().cloned());

    // Drop non-absolute, canonicalize existing paths (so symlinks like macOS
    // /var → /private/var and /tmp → /private/tmp match the resolved write
    // target the OS sandbox sees), then dedup.
    let mut write_roots: Vec<PathBuf> = write_roots
        .into_iter()
        .filter(|p| p.is_absolute())
        .map(|p| canon(&p))
        .collect();
    write_roots.sort();
    write_roots.dedup();
    let mut deny_read: Vec<PathBuf> = deny_read
        .into_iter()
        .filter(|p| p.is_absolute())
        .map(|p| canon(&p))
        .collect();
    deny_read.sort();
    deny_read.dedup();

    let read_roots = read_mask.then(|| {
        let mut roots: Vec<PathBuf> = READ_MASK_SYSTEM_ROOTS
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect();
        // The pod's own home (toolchain dotdirs, venvs). In a multiuser pod
        // this is the shared runtime user's home, not a tenant's.
        if let Some(h) = crate::util::home_dir() {
            roots.push(canon(&h));
        }
        roots.sort();
        roots.dedup();
        roots
    });

    ConfinePolicy {
        mode,
        write_roots,
        deny_read,
        read_roots,
    }
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// When confinement is active and `output` shows a permission/write error,
/// return a one-line hint so the model knows it might be the OS sandbox (not a
/// real perms problem) and how to allow it. `None` when off / no such error.
pub fn denied_hint(output: &str) -> Option<String> {
    hint_for_mode(mode(), output)
}

fn hint_for_mode(m: ConfineMode, output: &str) -> Option<String> {
    if m == ConfineMode::Off {
        return None;
    }
    // EPERM (macOS Seatbelt / EACCES (Landlock) / EROFS (bwrap ro-bind).
    const MARKERS: &[&str] = &[
        "Operation not permitted",
        "Permission denied",
        "Read-only file system",
    ];
    if !MARKERS.iter().any(|k| output.contains(k)) {
        return None;
    }
    Some(format!(
        "[bash.sandbox={}] note: a permission error above may be the OS sandbox blocking a \
         WRITE outside the workspace (+ /tmp + package-manager caches) — not a real permissions \
         problem. To allow it: write inside the workspace, add the path to settings.json \
         `bash.sandbox_write_paths`, or set `bash.sandbox` to `off`.",
        m.as_str()
    ))
}

/// Internal re-exec entry point. Call FIRST in every binary's `main()`: if argv
/// is the hidden `__confine` re-exec (Linux Landlock path — see `linux::wrap`),
/// this applies the Landlock ruleset and `exec`s the real command (never
/// returns); otherwise it returns immediately so normal startup proceeds. A
/// no-op on non-Linux.
pub fn maybe_handle_confine_subcommand() {
    #[cfg(target_os = "linux")]
    linux::maybe_handle_subcommand();
}

/// Emitted to stderr by the `__confine` helper when it CANNOT install OS
/// confinement and bails *before* running the command (it exits with
/// `EXIT_NO_ENFORCE`). The Bash chokepoint detects this and re-runs the
/// command unconfined — the workspace/container is still the boundary in
/// environments where the kernel confiner is unavailable, e.g. some
/// container kernels where the Landlock syscalls return `EINVAL`. Control
/// chars make a collision with real command output effectively impossible.
pub const NO_ENFORCE_SENTINEL: &str = "\u{1}thclaws-confine-unenforced\u{1}";

/// True when `out` carries the no-enforce sentinel (the confiner bailed
/// without running the command, so the caller should re-run unconfined).
pub fn output_shows_no_enforce(out: &str) -> bool {
    out.contains(NO_ENFORCE_SENTINEL)
}

/// Set once the confiner has bailed at runtime (no-enforce sentinel seen).
/// Future commands then skip the doomed confined attempt rather than paying
/// an extra process spawn per command.
static CONFINE_RUNTIME_FAILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Called by the Bash chokepoint when it observes the no-enforce sentinel,
/// so subsequent `shell_command_async` calls skip the failing confiner.
pub fn mark_no_enforce() {
    CONFINE_RUNTIME_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
}

// Only the Linux Landlock path consults this (macOS uses build-time
// sandbox-exec detection, not a runtime sentinel), so it's dead code
// elsewhere.
#[cfg(target_os = "linux")]
fn confine_runtime_failed() -> bool {
    CONFINE_RUNTIME_FAILED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The chokepoint `bash.rs` calls instead of `util::shell_command_async`:
/// returns a confined `sh -c <command>` when a policy is active and a confiner
/// is available on this OS, else a plain `sh -c <command>` (unchanged).
pub fn shell_command_async(command: &str) -> tokio::process::Command {
    match current_policy() {
        Some(policy) => confined_command(command, &policy)
            .unwrap_or_else(|| crate::util::shell_command_async(command)),
        None => crate::util::shell_command_async(command),
    }
}

/// Build the confined command for an explicit policy (testable). `None` when no
/// confiner is available on this OS (caller falls back to unconfined).
pub fn confined_command(command: &str, policy: &ConfinePolicy) -> Option<tokio::process::Command> {
    #[cfg(target_os = "macos")]
    {
        macos::wrap(command, policy)
    }
    #[cfg(target_os = "linux")]
    {
        linux::wrap(command, policy)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (command, policy);
        fallback_to_screening_once("no OS confiner exists on this platform");
        None
    }
}

fn binary_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let p = dir.join(name);
            if p.is_file() {
                Some(p)
            } else {
                None
            }
        })
    })
}

/// Loud, one-time warning when confinement was requested but the OS confiner
/// can't be used — we run UNCONFINED (never break the command). Security-
/// relevant: an operator who set a confined mode must know it isn't enforced.
fn fallback_to_screening_once(reason: &str) {
    static WARNED: OnceLock<()> = OnceLock::new();
    let m = mode().as_str();
    let r = reason.to_string();
    WARNED.get_or_init(move || {
        eprintln!(
            "\x1b[33m[bash.sandbox={m}] OS confinement requested but {r} — \
             running Bash UNCONFINED (command-screening only). \
             See dev-plan/49 for enabling the confiner on this host.\x1b[0m"
        );
    });
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub fn wrap(command: &str, policy: &ConfinePolicy) -> Option<tokio::process::Command> {
        if binary_on_path("sandbox-exec").is_none() {
            fallback_to_screening_once("sandbox-exec not found");
            return None;
        }
        if !works() {
            fallback_to_screening_once("sandbox-exec cannot run on this host");
            return None;
        }
        let profile = seatbelt_profile(policy);
        let (shell, flag) = crate::util::shell_invocation();
        let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(profile).arg(shell).arg(flag).arg(command);
        Some(c)
    }

    /// Probe once that sandbox-exec actually runs (binary present ≠ usable).
    fn works() -> bool {
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    /// Generate a Seatbelt profile: allow-by-default, deny all writes except
    /// the write-roots, deny reads of the secret paths.
    pub fn seatbelt_profile(policy: &ConfinePolicy) -> String {
        let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        if !policy.write_roots.is_empty() {
            p.push_str("(allow file-write*\n");
            for r in &policy.write_roots {
                p.push_str(&format!("  (subpath \"{}\")\n", sb_escape(r)));
            }
            p.push_str(")\n");
        }
        if !policy.deny_read.is_empty() {
            p.push_str("(deny file-read*\n");
            for r in &policy.deny_read {
                p.push_str(&format!("  (subpath \"{}\")\n", sb_escape(r)));
            }
            p.push_str(")\n");
        }
        p
    }

    fn sb_escape(p: &Path) -> String {
        p.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;

    /// Exit code the `__confine` helper uses to say "Landlock isn't enforcing
    /// here" so the parent's probe falls back instead of running unconfined.
    const EXIT_NO_ENFORCE: i32 = 78;

    // Landlock FS access rights (uapi bits). The V1 *write* set — handling only
    // these enforces write confinement while leaving read/exec unrestricted.
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_REMOVE_DIR: u64 = 1 << 4;
    const FS_REMOVE_FILE: u64 = 1 << 5;
    const FS_MAKE_CHAR: u64 = 1 << 6;
    const FS_MAKE_DIR: u64 = 1 << 7;
    const FS_MAKE_REG: u64 = 1 << 8;
    const FS_MAKE_SOCK: u64 = 1 << 9;
    const FS_MAKE_FIFO: u64 = 1 << 10;
    const FS_MAKE_BLOCK: u64 = 1 << 11;
    const FS_MAKE_SYM: u64 = 1 << 12;
    const WRITE_ACCESS: u64 = FS_WRITE_FILE
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM;
    // Read/exec bits for the multiuser read-mask (dev-plan/45). Handling
    // these makes every unlisted path unreadable — including `/proc`
    // (same-uid `/proc/<pid>/environ` credential exfil) and co-tenants'
    // `workspace-<id>/` trees.
    const FS_EXECUTE: u64 = 1 << 0;
    const FS_READ_FILE: u64 = 1 << 2;
    const FS_READ_DIR: u64 = 1 << 3;
    const READ_ACCESS: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    /// Prefer **Landlock** — a filesystem LSM that needs NO user namespace, so
    /// it's unaffected by the AppArmor unprivileged-userns restriction that
    /// breaks bwrap on stock Ubuntu 24.04. We re-exec ourselves as the hidden
    /// `__confine` helper, which installs the ruleset then `exec`s `sh -c`.
    /// Falls back to bwrap, then unconfined. NOTE: the Landlock path is *write*
    /// confinement only — `deny_read` (secret masking) is honored by bwrap /
    /// macOS Seatbelt but not here (a future ABI refinement).
    pub fn wrap(command: &str, policy: &ConfinePolicy) -> Option<tokio::process::Command> {
        if landlock_enforces() {
            if let Some(c) = landlock_reexec(command, policy) {
                return Some(c);
            }
        }
        bwrap_wrap(command, policy)
    }

    /// Re-exec `<self> __confine --write <root> … [--read <root> …] -- sh -c <command>`.
    fn landlock_reexec(command: &str, policy: &ConfinePolicy) -> Option<tokio::process::Command> {
        let exe = std::env::current_exe().ok()?;
        let (shell, flag) = crate::util::shell_invocation();
        let mut c = tokio::process::Command::new(exe);
        c.arg("__confine");
        for r in &policy.write_roots {
            if r.exists() {
                c.arg("--write").arg(r);
            }
        }
        if let Some(read_roots) = &policy.read_roots {
            for r in read_roots {
                if r.exists() {
                    c.arg("--read").arg(r);
                }
            }
        }
        c.arg("--").arg(shell).arg(flag).arg(command);
        Some(c)
    }

    /// Probe once that Landlock actually confines on this kernel by running our
    /// own `__confine --selftest`.
    fn landlock_enforces() -> bool {
        // A prior command already proved Landlock can't enforce here (e.g.
        // EINVAL on this container kernel) — skip it from now on.
        if super::confine_runtime_failed() {
            return false;
        }
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| {
            let Ok(exe) = std::env::current_exe() else {
                return false;
            };
            std::process::Command::new(exe)
                .arg("__confine")
                .arg("--selftest")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    /// Handle the internal `__confine` re-exec: `--selftest` (exit 0 iff
    /// Landlock confines) or `--write P … -- argv…` (install ruleset, exec argv).
    pub fn maybe_handle_subcommand() {
        let mut it = std::env::args_os().skip(1);
        match it.next() {
            Some(a) if a.to_str() == Some("__confine") => {}
            _ => return,
        }
        let rest: Vec<std::ffi::OsString> = it.collect();
        if rest.first().and_then(|s| s.to_str()) == Some("--selftest") {
            std::process::exit(selftest());
        }
        let mut write_roots: Vec<PathBuf> = Vec::new();
        let mut read_roots: Vec<PathBuf> = Vec::new();
        let mut argv: Vec<std::ffi::OsString> = Vec::new();
        let mut i = 0;
        while i < rest.len() {
            if rest[i].to_str() == Some("--write") && i + 1 < rest.len() {
                write_roots.push(PathBuf::from(&rest[i + 1]));
                i += 2;
            } else if rest[i].to_str() == Some("--read") && i + 1 < rest.len() {
                read_roots.push(PathBuf::from(&rest[i + 1]));
                i += 2;
            } else if rest[i].to_str() == Some("--") {
                argv = rest[i + 1..].to_vec();
                break;
            } else {
                i += 1;
            }
        }
        if argv.is_empty() {
            eprintln!("thclaws __confine: no command after --");
            std::process::exit(2);
        }
        let read_mask = (!read_roots.is_empty()).then_some(read_roots.as_slice());
        match apply_landlock(&write_roots, read_mask) {
            Ok(true) => {}
            // Confiner can't enforce (Landlock absent, or EINVAL on some
            // container kernels). Emit the sentinel so the Bash chokepoint
            // re-runs the command unconfined instead of surfacing exit 78 +
            // a scary "landlock setup failed" with no output.
            Ok(false) => {
                eprintln!("{}", super::NO_ENFORCE_SENTINEL);
                std::process::exit(EXIT_NO_ENFORCE);
            }
            Err(e) => {
                eprintln!("thclaws __confine: landlock unavailable ({e}) — running unconfined");
                eprintln!("{}", super::NO_ENFORCE_SENTINEL);
                std::process::exit(EXIT_NO_ENFORCE);
            }
        }
        let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
        eprintln!("thclaws __confine: exec {:?}: {err}", argv[0]);
        std::process::exit(127);
    }

    /// Install a Landlock ruleset confining writes to `write_roots` and — when
    /// `read_roots` is `Some` (multiuser read-mask) — reads/execs to
    /// `read_roots ∪ write_roots`. `Ok(true)` = enforced; `Ok(false)` =
    /// Landlock unavailable on this kernel (fall back).
    fn apply_landlock(
        write_roots: &[PathBuf],
        read_roots: Option<&[PathBuf]>,
    ) -> std::io::Result<bool> {
        #[repr(C)]
        struct RulesetAttr {
            handled_access_fs: u64,
        }
        #[repr(C, packed)]
        struct PathBeneathAttr {
            allowed_access: u64,
            parent_fd: i32,
        }
        let handled = match read_roots {
            Some(_) => WRITE_ACCESS | READ_ACCESS,
            None => WRITE_ACCESS,
        };
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        let rs = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0_u32,
            )
        };
        if rs < 0 {
            return Ok(false); // ENOSYS / not supported → fall back
        }
        let rs = rs as libc::c_int;
        // File-only access bits. A path_beneath rule on a NON-directory
        // (e.g. the /dev/null … device write-roots) must carry ONLY these
        // — handing Landlock a dir-only bit (MAKE_*/REMOVE_*/READ_DIR) on a
        // file is EINVAL, which fails the WHOLE ruleset and silently drops
        // us to unconfined. This bit us on the hosted runners: /dev/null in
        // write_roots made every confined spawn fall back to no-enforce.
        const FILE_ONLY_ACCESS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE;
        let add_rule = |rs: libc::c_int, root: &PathBuf, access: u64| -> std::io::Result<bool> {
            let cpath = match std::ffi::CString::new(root.as_os_str().as_bytes()) {
                Ok(c) => c,
                Err(_) => return Ok(true), // unrepresentable path — skip
            };
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                return Ok(true); // nonexistent path — skip
            }
            // Directory? Non-dirs get the file-only access subset.
            let is_dir = unsafe {
                let mut st: libc::stat = std::mem::zeroed();
                libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR
            };
            let allowed = if is_dir {
                access
            } else {
                access & FILE_ONLY_ACCESS
            };
            let pb = PathBeneathAttr {
                allowed_access: allowed,
                parent_fd: fd,
            };
            let r = unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    rs,
                    RULE_PATH_BENEATH,
                    &pb as *const PathBeneathAttr,
                    0_u32,
                )
            };
            unsafe { libc::close(fd) };
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(true)
        };
        // Write roots must stay readable/executable when reads are handled
        // (a venv or build tree inside the workspace is both).
        let write_access = match read_roots {
            Some(_) => WRITE_ACCESS | READ_ACCESS,
            None => WRITE_ACCESS,
        };
        for root in write_roots {
            if let Err(e) = add_rule(rs, root, write_access) {
                unsafe { libc::close(rs) };
                return Err(e);
            }
        }
        for root in read_roots.unwrap_or(&[]) {
            if let Err(e) = add_rule(rs, root, READ_ACCESS) {
                unsafe { libc::close(rs) };
                return Err(e);
            }
        }
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        let r = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rs, 0_u32) };
        unsafe { libc::close(rs) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(true)
    }

    /// Self-test: grant write on a temp dir PLUS a device node (`/dev/null`
    /// — the real policy always includes it; a dir-only access bit on a
    /// non-directory used to EINVAL the whole ruleset), restrict, confirm
    /// inside-write succeeds and a sibling-dir write is denied. Exit 0 iff
    /// it confines.
    fn selftest() -> i32 {
        let base = std::env::temp_dir();
        let ws = base.join(format!("thclaws-ll-ws-{}", std::process::id()));
        let out = base.join(format!("thclaws-ll-out-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&ws);
        let _ = std::fs::create_dir_all(&out);
        let roots = [ws.clone(), PathBuf::from("/dev/null")];
        let enforced = matches!(apply_landlock(&roots, None), Ok(true));
        let inside_ok = enforced && std::fs::write(ws.join("ok"), b"x").is_ok();
        let outside_denied = enforced && std::fs::write(out.join("bad"), b"x").is_err();
        // /dev/null must remain writable (a device write-root the file-only
        // access mask should have preserved).
        let devnull_ok = enforced && std::fs::write("/dev/null", b"x").is_ok();
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&out);
        if inside_ok && outside_denied && devnull_ok {
            0
        } else {
            EXIT_NO_ENFORCE
        }
    }

    /// bubblewrap fallback (read-masking + older kernels). Needs an
    /// unprivileged user namespace — on AppArmor-restricted hosts this fails the
    /// probe and we run unconfined. `--dev /dev` is a minimal devtmpfs (basic
    /// nodes only) — accelerator devices must be re-bound explicitly.
    fn bwrap_wrap(command: &str, policy: &ConfinePolicy) -> Option<tokio::process::Command> {
        if binary_on_path("bwrap").is_none() {
            fallback_to_screening_once(
                "no usable confiner (Landlock not enforcing, bwrap not found)",
            );
            return None;
        }
        if !bwrap_works() {
            fallback_to_screening_once(
                "no usable confiner (Landlock not enforcing; bwrap cannot create a user \
                 namespace — AppArmor unprivileged-userns restriction?)",
            );
            return None;
        }
        let (shell, flag) = crate::util::shell_invocation();
        let mut c = tokio::process::Command::new("bwrap");
        c.arg("--ro-bind").arg("/").arg("/");
        c.arg("--dev").arg("/dev");
        // --dev hides GPU nodes → NVML breaks on DGX/Jetson; re-expose them.
        if let Ok(rd) = std::fs::read_dir("/dev") {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("nvidia") {
                    let p = e.path();
                    c.arg("--dev-bind-try").arg(&p).arg(&p);
                }
            }
        }
        c.arg("--dev-bind-try").arg("/dev/dri").arg("/dev/dri");
        c.arg("--proc").arg("/proc");
        c.arg("--tmpfs").arg("/tmp");
        for r in &policy.write_roots {
            if r == Path::new("/tmp") || r == Path::new("/private/tmp") || r.starts_with("/dev") {
                continue;
            }
            if r.exists() {
                c.arg("--bind").arg(r).arg(r);
            }
        }
        for d in &policy.deny_read {
            if d.exists() {
                c.arg("--tmpfs").arg(d);
            }
        }
        c.arg("--die-with-parent");
        c.arg(shell).arg(flag).arg(command);
        Some(c)
    }

    fn bwrap_works() -> bool {
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| {
            std::process::Command::new("bwrap")
                .args(["--ro-bind", "/", "/", "--dev", "/dev", "true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes() {
        assert_eq!(ConfineMode::parse("workspace"), ConfineMode::Workspace);
        assert_eq!(ConfineMode::parse("STRICT"), ConfineMode::Strict);
        assert_eq!(ConfineMode::parse(""), ConfineMode::Off);
        assert_eq!(ConfineMode::parse("nonsense"), ConfineMode::Off);
    }

    #[test]
    fn no_enforce_sentinel_detection() {
        // Real command output never trips it.
        assert!(!output_shows_no_enforce("total 0\nfoo.txt\n"));
        assert!(!output_shows_no_enforce("[exit code 78]"));
        // The exact sentinel the __confine helper emits does.
        assert!(output_shows_no_enforce(&format!(
            "[stderr]\n{NO_ENFORCE_SENTINEL}\n[exit code 78]"
        )));
        // Sentinel uses control chars, so it can't collide with normal text.
        assert!(NO_ENFORCE_SENTINEL.contains('\u{1}'));
    }

    #[test]
    fn denied_hint_fires_only_when_confined_and_on_perms_error() {
        // off → never hints
        assert!(hint_for_mode(ConfineMode::Off, "Permission denied").is_none());
        // confined + perms error → hint
        let h = hint_for_mode(
            ConfineMode::Workspace,
            "sh: cannot create /x: Permission denied",
        );
        assert!(h
            .as_deref()
            .unwrap_or("")
            .contains("bash.sandbox=workspace"));
        assert!(hint_for_mode(ConfineMode::Strict, "Operation not permitted").is_some());
        assert!(hint_for_mode(ConfineMode::Workspace, "Read-only file system").is_some());
        // confined but clean output → no hint
        assert!(hint_for_mode(ConfineMode::Workspace, "build succeeded\n[exit code: 0]").is_none());
    }

    #[test]
    fn policy_has_workspace_tmp_and_secrets_denied() {
        let ws = std::env::temp_dir();
        let pol = build_policy(ConfineMode::Workspace, &ws, &[], &[], false);
        // /tmp is writable (canonicalized — /private/tmp on macOS).
        let tmp = canon(Path::new("/tmp"));
        assert!(
            pol.write_roots.iter().any(|p| *p == tmp),
            "tmp must be writable, got {:?}",
            pol.write_roots
        );
        assert!(
            pol.write_roots.iter().any(|p| p.starts_with(&canon(&ws))),
            "workspace must be writable"
        );
        // strict mode drops the cache allowlist.
        let strict = build_policy(ConfineMode::Strict, &ws, &[], &[], false);
        if let Some(h) = crate::util::home_dir() {
            assert!(
                !strict.write_roots.iter().any(|p| *p == h.join(".cargo")),
                "strict must not whitelist caches"
            );
        }
    }

    #[test]
    fn read_mask_allowlists_system_but_never_proc() {
        let ws = std::env::temp_dir();
        // Desktop (no mask): reads unrestricted.
        let pol = build_policy(ConfineMode::Workspace, &ws, &[], &[], false);
        assert!(pol.read_roots.is_none());
        // Multiuser mask: system roots granted, /proc and the workspace
        // PARENT deliberately absent (co-tenant + /proc/<pid>/environ
        // masking — the dev-plan/45 boundary).
        let pol = build_policy(ConfineMode::Workspace, &ws, &[], &[], true);
        let roots = pol.read_roots.as_deref().expect("mask on");
        assert!(!roots.is_empty());
        assert!(
            roots.iter().all(|p| !p.starts_with("/proc")),
            "/proc must never be read-granted, got {roots:?}"
        );
        #[cfg(target_os = "linux")]
        assert!(
            roots.iter().any(|p| p == Path::new("/usr")),
            "system dirs must be readable"
        );
        // The workspace itself is covered by the WRITE roots (which the
        // Landlock arm grants read+exec on when the mask is active).
        assert!(pol.write_roots.iter().any(|p| p.starts_with(&canon(&ws))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_shape() {
        let ws = std::env::temp_dir();
        let pol = build_policy(ConfineMode::Workspace, &ws, &[], &[], false);
        let prof = macos::seatbelt_profile(&pol);
        assert!(prof.contains("(deny file-write*)"));
        assert!(prof.contains("(allow file-write*"));
        assert!(prof.contains("(deny file-read*"));
    }

    /// 49.2 acceptance (macOS): a write INSIDE the workspace succeeds; writes
    /// OUTSIDE are OS-blocked — even when obfuscated. The boundary is the
    /// kernel, not string-matching.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn workspace_mode_blocks_outside_writes_even_obfuscated() {
        if binary_on_path("sandbox-exec").is_none() {
            return; // confiner unavailable on this box — skip
        }
        let ws = tempfile::tempdir().unwrap();
        let pol = build_policy(ConfineMode::Workspace, ws.path(), &[], &[], false);

        // INSIDE → allowed.
        let inside = ws.path().join("ok.txt");
        let out = confined_command(&format!("echo hi > {}", inside.display()), &pol)
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success() && inside.exists(),
            "write inside workspace must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // OUTSIDE (directly in $HOME — not a cache, not tmp, not the workspace).
        let home = crate::util::home_dir().unwrap();
        let outside = home.join(format!("confine_escape_probe_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let o = outside.display().to_string();
        for cmd in [
            format!("echo hi > {o}"),                          // plain
            format!("b=$(printf '%s' {o}); echo hi > \"$b\""), // var-indirection
            format!("bash -c 'echo hi > {o}'"),                // nested shell
        ] {
            let out = confined_command(&cmd, &pol)
                .unwrap()
                .output()
                .await
                .unwrap();
            assert!(
                !outside.exists(),
                "escape was NOT blocked by the OS sandbox: `{cmd}` created {o}"
            );
            let _ = std::fs::remove_file(&outside);
            let _ = out;
        }
    }

    /// 49.5 acceptance (macOS): the `workspace` allowlist lets the real
    /// commands shell-heavy agents run actually work — git init, in-workspace
    /// writes via python/node, /tmp, and the package-manager caches — while a
    /// direct $HOME write is still blocked. Guards on tool presence so it's a
    /// safe CI regression check for the allowlist.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn workspace_mode_allows_real_toolchain_commands() {
        if binary_on_path("sandbox-exec").is_none() {
            return;
        }
        // The probes below write to `$HOME/.cache` and `$HOME/.cargo` and
        // expect the policy to permit them — so they need HOME to still mean
        // what it meant when the policy was built. HOME is PROCESS-WIDE, and
        // other tests repoint it at a tempdir (`memory`'s `scoped_home`); when
        // that overlapped, the subprocess inherited the hijacked HOME, wrote
        // outside the allowlist, and the assertion failed with "Operation not
        // permitted" on a path the policy never covered. Hold the suite's env
        // lock for the duration.
        let _env = crate::kms::test_env_lock();
        let ws = tempfile::tempdir().unwrap();
        let pol = build_policy(ConfineMode::Workspace, ws.path(), &[], &[], false);
        let pid = std::process::id();

        // (command, must_succeed). Each runs with cwd = workspace.
        let mut cases: Vec<(String, bool)> = vec![
            ("echo x > ws.txt && test -f ws.txt".into(), true), // workspace write
            (
                "mkdir -p node_modules/p && echo {} > node_modules/p/package.json".into(),
                true,
            ),
            (
                format!("echo x > \"$HOME/.cache/thclaws_confine_probe_{pid}\""),
                true,
            ), // pip cache
            (
                format!("echo x > \"$HOME/.cargo/thclaws_confine_probe_{pid}\""),
                true,
            ), // cargo cache
            (
                format!("echo x > \"${{TMPDIR:-/tmp}}/thclaws_confine_probe_{pid}\""),
                true,
            ), // tmp
            (
                format!("echo x > \"$HOME/thclaws_confine_ESCAPE_{pid}\""),
                false,
            ), // escape → blocked
        ];
        if binary_on_path("git").is_some() {
            cases.push(("git init -q . && test -d .git".into(), true));
        }
        if binary_on_path("python3").is_some() {
            cases.push((
                "python3 -c \"open('py.txt','w').write('x')\" && test -f py.txt".into(),
                true,
            ));
        }
        if binary_on_path("node").is_some() {
            cases.push((
                "node -e \"require('fs').writeFileSync('node.txt','x')\" && test -f node.txt"
                    .into(),
                true,
            ));
        }

        let mut failures = vec![];
        for (cmd, must_succeed) in &cases {
            let mut c = confined_command(cmd, &pol).unwrap();
            c.current_dir(ws.path());
            let out = c.output().await.unwrap();
            let ok = out.status.success();
            if ok != *must_succeed {
                failures.push(format!(
                    "`{cmd}` → success={ok}, expected {must_succeed}. stderr: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        // Clean up the cache/home probes the allowed cases created.
        if let Some(h) = crate::util::home_dir() {
            for p in [".cache", ".cargo"] {
                let _ =
                    std::fs::remove_file(h.join(p).join(format!("thclaws_confine_probe_{pid}")));
            }
            let _ = std::fs::remove_file(h.join(format!("thclaws_confine_ESCAPE_{pid}")));
        }
        assert!(
            failures.is_empty(),
            "allowlist gaps:\n{}",
            failures.join("\n")
        );
    }
}
