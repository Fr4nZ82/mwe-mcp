// SPDX-License-Identifier: AGPL-3.0-or-later
//! Workdir permission auditing — defence-in-depth for the on-disk wiki bytes.
//!
//! Per-reader redaction (see [redaction policy](../../../wiki/design-notes/redaction-policy.md))
//! is enforced when the server *renders* a response, but the markdown and
//! `engine.db` under the workdir are cleartext on disk. The ACL is therefore
//! only a real boundary when the operating system keeps every principal other
//! than the `mwe-mcp` process away from the workdir bytes: a co-located
//! consumer that can read the files reads the un-redacted union of every
//! fragment, bypassing the governance entirely. Deployment guidance lives in
//! `INTEGRATING.md` ("Deployment security — where to run the consumer").
//!
//! This module turns "remember to lock down the workdir" into something the
//! server reports. [`audit`] inspects the workdir root and its byte stores and
//! flags any path reachable by group or world. `mwe-mcp doctor` prints the
//! findings; `mwe-mcp serve` warns at boot. It is **advisory, not a gate** —
//! the legitimate single-principal dev box is loose on purpose, so the server
//! still starts; the point is that a misconfiguration is impossible to miss
//! rather than silently insecure.

use std::path::{Path, PathBuf};

/// How exposed a workdir path is to principals other than its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// World (other) can reach the bytes, or a non-owner can write them —
    /// any local user bypasses the per-reader ACL.
    High,
    /// Only the owning group can read/traverse the bytes. Safe **iff** that
    /// group is exactly the server's own, which cannot be verified from the
    /// mode bits alone — hence flagged, at lower severity than world access.
    Medium,
}

impl Severity {
    /// Short uppercase tag for report lines.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::High => "HIGH",
            Self::Medium => "WARN",
        }
    }
}

/// One workdir path whose mode bits let a non-owner principal reach it.
#[derive(Debug, Clone)]
pub struct PermFinding {
    /// The offending path.
    pub path: PathBuf,
    /// The low permission bits (e.g. `0o775`).
    pub mode: u32,
    /// Severity of the exposure.
    pub severity: Severity,
}

impl PermFinding {
    /// Render the mode as the familiar `rwxrwxr-x` triad string.
    #[must_use]
    pub fn mode_string(&self) -> String {
        let mut s = String::with_capacity(9);
        for shift in [6, 3, 0] {
            let triad = (self.mode >> shift) & 0o7;
            s.push(if triad & 0o4 != 0 { 'r' } else { '-' });
            s.push(if triad & 0o2 != 0 { 'w' } else { '-' });
            s.push(if triad & 0o1 != 0 { 'x' } else { '-' });
        }
        s
    }
}

/// The one-line fix an operator can paste to lock the workdir down to its
/// owner. Mirrors the strongest same-host posture in `INTEGRATING.md`.
#[must_use]
pub fn remediation(workdir: &Path) -> String {
    format!("chmod -R go-rwx {}", workdir.display())
}

/// Classification of the account `mwe-mcp serve` is running under, for the
/// dedicated-user startup gate (the production half of the trust boundary —
/// see `INTEGRATING.md` and roadmap group 14).
///
/// The advisory [`audit`] catches *other* users reaching the workdir, but
/// 0700 does nothing against the **same** uid: a co-located agent running as
/// the same login user reads the cleartext bytes regardless. The only fix is
/// to run the daemon as a dedicated account no interactive user shares — this
/// type lets `serve` enforce that at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserClass {
    /// A dedicated, non-root account with no login shell (a system/service
    /// user, e.g. `mwe-mcp`) — the co-location boundary can hold. Carries the
    /// resolved user name for logging, or a `uid N` placeholder when no
    /// `/etc/passwd` entry exists (a uid with no registered login is not an
    /// interactive account, so it is treated as dedicated).
    Dedicated {
        /// The resolved account name (or a `uid N` / placeholder).
        user: String,
    },
    /// Running as **root** (uid 0) — never run the daemon as root.
    Root,
    /// A **login-capable** account (a real login shell): a process this same
    /// user runs — including a co-located agent's file tool — can read the
    /// cleartext workdir, so the per-reader ACL is bypassable.
    LoginAccount {
        /// The login account name.
        user: String,
        /// Its login shell (the signal that classified it as interactive).
        shell: String,
    },
}

impl UserClass {
    /// Whether the daemon may boot unattended under this account.
    #[must_use]
    pub const fn is_dedicated(&self) -> bool {
        matches!(self, Self::Dedicated { .. })
    }
}

/// Classify the current effective user for the dedicated-user gate.
///
/// Linux-only (reads `/proc/self/status` + `/etc/passwd`, no FFI so the crate
/// stays `#![forbid(unsafe_code)]`); other targets return [`UserClass::Dedicated`]
/// since the gate's macOS/Windows form is tracked separately (roadmap 14f). A
/// uid we cannot resolve is treated as dedicated — the human-mistake case
/// (running as your own login account) always has a `/etc/passwd` entry with a
/// real shell and is caught.
#[cfg(target_os = "linux")]
#[must_use]
pub fn classify_current_user() -> UserClass {
    let Some(euid) = effective_uid() else {
        return UserClass::Dedicated {
            user: "<unknown-uid>".to_owned(),
        };
    };
    if euid == 0 {
        return UserClass::Root;
    }
    match passwd_lookup(euid) {
        Some((user, shell)) if is_login_shell(&shell) => UserClass::LoginAccount { user, shell },
        Some((user, _)) => UserClass::Dedicated { user },
        None => UserClass::Dedicated {
            user: format!("uid {euid}"),
        },
    }
}

/// Non-Linux: the gate's form for this OS is tracked separately (roadmap 14f);
/// do not block here.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn classify_current_user() -> UserClass {
    UserClass::Dedicated {
        user: "<non-linux>".to_owned(),
    }
}

/// Effective uid from `/proc/self/status` (the second field of the `Uid:`
/// line), or `None` when `/proc` is unreadable.
#[cfg(target_os = "linux")]
fn effective_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    // `Uid:\t<real>\t<effective>\t<saved>\t<fs>`
    line.split_whitespace().nth(2)?.parse().ok()
}

/// `(name, shell)` for `uid` from `/etc/passwd`, or `None` when absent.
#[cfg(target_os = "linux")]
fn passwd_lookup(uid: u32) -> Option<(String, String)> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    let uid_field = uid.to_string();
    for line in passwd.lines() {
        // `name:passwd:uid:gid:gecos:home:shell`
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 7 && fields[2] == uid_field {
            return Some((fields[0].to_owned(), fields[6].to_owned()));
        }
    }
    None
}

/// Whether `shell` is an interactive login shell. A `nologin`/`false` shell
/// (or an empty one) marks a non-interactive service account.
#[cfg(target_os = "linux")]
fn is_login_shell(shell: &str) -> bool {
    let shell = shell.trim();
    if shell.is_empty() {
        return false;
    }
    let base = shell.rsplit('/').next().unwrap_or(shell);
    !(base == "nologin" || base == "false")
}

/// Classify a path's low permission bits, or `None` when it is owner-only.
///
/// - **High** — any group/other *write* bit (`0o022`), or any *world* bit
///   (`0o007`): a non-owner can tamper, or every local user can read.
/// - **Medium** — group has read/execute but world has nothing and no
///   non-owner can write: reachable only by the owning group.
#[cfg(unix)]
const fn severity_for(mode: u32) -> Option<Severity> {
    let group_other_write = mode & 0o022 != 0;
    let world_any = mode & 0o007 != 0;
    let group_any = mode & 0o070 != 0;
    if group_other_write || world_any {
        Some(Severity::High)
    } else if group_any {
        Some(Severity::Medium)
    } else {
        None
    }
}

/// Audit the workdir and its sensitive byte stores for non-owner reachability.
///
/// The workdir root directory is the master gate — with no group/other execute
/// bit, nobody can traverse into it regardless of inner file modes — but the
/// `engine.db` / `wikis/` / `media/` stores are checked too so a loose root
/// surfaces every leaking path, not just the directory.
///
/// Returns an empty vector when everything is owner-only (or on non-Unix, where
/// POSIX mode bits do not apply and Windows ACL auditing is not implemented).
#[cfg(unix)]
#[must_use]
pub fn audit(workdir: &Path) -> Vec<PermFinding> {
    use std::os::unix::fs::PermissionsExt;

    let mut targets = vec![workdir.to_path_buf()];
    for child in ["engine.db", "wikis", "media"] {
        let p = workdir.join(child);
        if p.exists() {
            targets.push(p);
        }
    }

    let mut findings = Vec::new();
    for path in targets {
        // symlink_metadata: never follow a symlink out of the workdir.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let mode = meta.permissions().mode() & 0o777;
        if let Some(severity) = severity_for(mode) {
            findings.push(PermFinding {
                path,
                mode,
                severity,
            });
        }
    }
    findings
}

/// Non-Unix stub: Windows uses ACLs rather than POSIX mode bits, and auditing
/// them is a separate mechanism not yet implemented.
#[cfg(not(unix))]
#[must_use]
pub fn audit(_workdir: &Path) -> Vec<PermFinding> {
    Vec::new()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn severity_classification() {
        // Owner-only is clean.
        assert_eq!(severity_for(0o700), None);
        assert_eq!(severity_for(0o600), None);
        // World read/exec → HIGH (any local user reads the cleartext bytes).
        assert_eq!(severity_for(0o755), Some(Severity::High));
        assert_eq!(severity_for(0o775), Some(Severity::High)); // the live dogfood case
        assert_eq!(severity_for(0o644), Some(Severity::High));
        // Group write but no world bit → HIGH (a non-owner can tamper).
        assert_eq!(severity_for(0o770), Some(Severity::High));
        // Group read/exec only, world nothing → MEDIUM.
        assert_eq!(severity_for(0o750), Some(Severity::Medium));
        assert_eq!(severity_for(0o740), Some(Severity::Medium));
    }

    #[test]
    fn mode_string_renders_triads() {
        let f = PermFinding {
            path: PathBuf::from("/tmp/x"),
            mode: 0o775,
            severity: Severity::High,
        };
        assert_eq!(f.mode_string(), "rwxrwxr-x");
        let f = PermFinding {
            path: PathBuf::from("/tmp/x"),
            mode: 0o644,
            severity: Severity::High,
        };
        assert_eq!(f.mode_string(), "rw-r--r--");
    }

    #[test]
    fn audit_flags_a_world_readable_workdir() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("mwe-ws-sec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let findings = audit(&dir);
        assert!(
            findings
                .iter()
                .any(|f| f.path == dir && f.severity == Severity::High),
            "expected a HIGH finding for the 0o755 workdir, got {findings:?}"
        );

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            audit(&dir).is_empty(),
            "0o700 workdir should produce no findings"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_class_is_dedicated() {
        assert!(
            UserClass::Dedicated {
                user: "mwe-mcp".to_owned()
            }
            .is_dedicated()
        );
        assert!(!UserClass::Root.is_dedicated());
        assert!(
            !UserClass::LoginAccount {
                user: "frodo".to_owned(),
                shell: "/bin/bash".to_owned(),
            }
            .is_dedicated()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn login_shell_detection() {
        // Real login shells → a co-located process as this user can read the workdir.
        assert!(is_login_shell("/bin/bash"));
        assert!(is_login_shell("/usr/bin/zsh"));
        // Service-account shells → non-interactive, dedicated.
        assert!(!is_login_shell("/usr/sbin/nologin"));
        assert!(!is_login_shell("/sbin/nologin"));
        assert!(!is_login_shell("/bin/false"));
        assert!(!is_login_shell("/usr/bin/false"));
        assert!(!is_login_shell(""));
    }
}
