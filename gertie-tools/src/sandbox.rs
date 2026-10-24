//! Kernel sandboxing for bash children, via Landlock.
//!
//! Opt-in (`[security] sandbox_bash = true`), default off — the
//! permission gate plus the deny list are the everyday safety net; this
//! adds a kernel-enforced floor for the approved commands that still
//! misbehave (or were approved carelessly).
//!
//! Posture, fail-closed on purpose: read access to the system, write
//! access to the workspace and /tmp only. When the kernel can't enforce
//! (too old, blocked in a container), the spawn **fails** — gertie
//! never runs a sandboxed command unsandboxed just because the sandbox
//! was unavailable. `CompatLevel::HardRequirement` is what makes that
//! guarantee: best-effort would silently downgrade to no sandbox.

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetError, RulesetStatus, path_beneath_rules,
};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// System paths the child may read. Everything else is unreadable —
/// including the workspace, which gets explicit write access below.
/// Missing paths are filtered out first: with HardRequirement, a rule
/// for a path that can't be opened is an error, and not every distro
/// has /lib64 or /opt.
const READONLY_PATHS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/dev", "/opt", "/proc", "/sys",
];

pub struct Sandbox {
    /// The workspace gets read *and* write; brief files and tool work
    /// live here.
    root: PathBuf,
}

impl Sandbox {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Installs the sandbox into a child command via `pre_exec`: the
    /// ruleset is built and enforced in the child, after fork, before
    /// exec — restrictions survive exec, so `sh` and everything it
    /// runs inherits them. Any error aborts the spawn (fail-closed).
    pub fn apply(&self, command: &mut std::process::Command) -> Result<(), String> {
        let root = self.root.clone();
        // Safety: the closure only makes Landlock syscalls; it touches
        // no state that fork left inconsistent.
        unsafe {
            command.pre_exec(move || enforce(root.as_path()).map_err(std::io::Error::other));
        }
        Ok(())
    }
}

/// Builds and enforces the ruleset on the calling (child) process.
fn enforce(root: &Path) -> Result<(), String> {
    let existing_readonly: Vec<&str> = READONLY_PATHS
        .iter()
        .copied()
        .filter(|path| Path::new(path).exists())
        .collect();
    let write_paths = [root, Path::new("/tmp")];
    let build = || -> Result<_, RulesetError> {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V1))?
            .create()?
            .add_rules(path_beneath_rules(
                existing_readonly,
                AccessFs::from_read(ABI::V1),
            ))?
            // Redirects to /dev/null are ordinary shell hygiene.
            .add_rules(path_beneath_rules(
                &["/dev/null"],
                AccessFs::from_write(ABI::V1),
            ))?
            .add_rules(path_beneath_rules(write_paths, AccessFs::from_all(ABI::V1)))?
            .restrict_self()
    };
    let status = build().map_err(|error| format!("landlock: {error}"))?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        // Unreachable under HardRequirement (incompatibilities error
        // out), but the check documents the contract and costs nothing.
        return Err("landlock not fully enforced — refusing to run unsandboxed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `sh -c command` under the sandbox and returns the exit
    /// description. These tests need a Landlock-capable kernel
    /// (≥ 5.13) and a container that passes the syscalls through.
    fn sandboxed(command: &str) -> Result<std::process::Output, String> {
        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(tmp.path());
        Sandbox::new(tmp.path().to_path_buf()).apply(&mut cmd)?;
        cmd.output().map_err(|error| format!("spawn: {error}"))
    }

    #[test]
    fn sandboxed_children_can_read_the_system_and_write_the_workspace() {
        let output = match sandboxed("ls /usr/bin > out.txt && echo ok") {
            Ok(output) => output,
            // Kernel or container without Landlock: not a failure of
            // the tool — recorded as skipped.
            Err(_) => {
                eprintln!("skipped: sandbox unavailable");
                return;
            }
        };
        assert!(output.status.success(), "{output:?}");
        assert!(std::fs::read_to_string("/dev/null").is_ok()); // sanity on host
    }

    #[test]
    fn sandboxed_children_cannot_write_outside_the_workspace() {
        let output = match sandboxed("echo pwned > /etc/gertie-sandbox-test") {
            Ok(output) => output,
            Err(_) => {
                eprintln!("skipped: sandbox unavailable");
                return;
            }
        };
        // The write itself is EACCES; sh reports failure. The file must
        // not exist.
        assert!(!std::path::Path::new("/etc/gertie-sandbox-test").exists());
        assert!(!output.status.success());
    }
}
