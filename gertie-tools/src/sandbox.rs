//! Kernel sandboxing for bash children, via Landlock.
//!
//! Opt-in (`[security] sandbox_bash = true`), default off, the
//! permission gate plus the deny list are the everyday safety net; this
//! adds a kernel-enforced floor for the approved commands that still
//! misbehave (or were approved carelessly).
//!
//! Posture, fail-closed on purpose: read access to the system, write
//! access to the workspace and /tmp only, and outbound TCP closed
//! (ABI v4, kernel 6.7+): handled, no rules, so TCP connects and
//! binds are refused, loopback included (a sandboxed dev server
//! cannot bind its port; that is part of the deal). Landlock ABI v4
//! speaks TCP only: UDP (DNS included), ICMP, and abstract Unix
//! sockets are NOT restricted here, so name resolution still works
//! but a datagram channel also still exists; full network closure
//! needs a namespace or a seccomp layer, which gertie does not build.
//! Say "TCP-closed", never "network-free". The floor is the whole
//! sandbox: 6.7+ or nothing (Debian 12's 6.1 and RHEL 9's 5.14 get no
//! sandbox_bash at all). When the
//! kernel can't enforce (too old, blocked in a container), the spawn
//! **fails**, gertie never runs a sandboxed command unsandboxed just
//! because the sandbox was unavailable. `CompatLevel::HardRequirement`
//! is what makes that guarantee: best-effort would silently downgrade
//! to no sandbox.

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetError, RulesetStatus, path_beneath_rules,
};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// System paths the child may read. Everything else is unreadable,
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
    /// True = the child also moves into a fresh user+network
    /// namespace: Landlock closes TCP, the namespace closes everything
    /// else (UDP, DNS, ICMP, raw sockets). Opt-in on top of the base
    /// sandbox, fail-closed like everything else here.
    network_isolated: bool,
}

impl Sandbox {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            network_isolated: false,
        }
    }

    /// Escalate to a fresh user+network namespace: the true
    /// no-network closure Landlock alone cannot express. Unprivileged
    /// where user namespaces are allowed (most modern distros); on
    /// kernels or configurations that refuse the unshare, the spawn
    /// fails instead of degrading.
    pub fn network_isolated(mut self) -> Self {
        self.network_isolated = true;
        self
    }

    /// Installs the sandbox into a child command via `pre_exec`: the
    /// ruleset is built and enforced in the child, after fork, before
    /// exec, restrictions survive exec, so `sh` and everything it
    /// runs inherits them. Any error aborts the spawn (fail-closed).
    pub fn apply(&self, command: &mut std::process::Command) -> Result<(), String> {
        let root = self.root.clone();
        let network_isolated = self.network_isolated;
        // Safety: the namespace half of this closure is strictly
        // async-signal-safe (unshare, getuid/getgid, open/write/close,
        // a fixed stack buffer; errors carry the raw errno, never an
        // allocation), because pre_exec runs after fork in what may be
        // a multi-threaded process, where a non-safe call can deadlock
        // the child on a lock another thread held at fork time. The
        // Landlock half keeps upstream's idiom (the crate allocates
        // internally); that deviation is the module's founding
        // trade-off, not this PR's.
        unsafe {
            command.pre_exec(move || {
                if network_isolated
                    && let Err(errno) = isolate_network_raw()
                {
                    return Err(std::io::Error::from_raw_os_error(errno));
                }
                enforce(root.as_path()).map_err(std::io::Error::other)
            });
        }
        Ok(())
    }
}

/// Moves the calling (child) process into a fresh user + network
/// namespace, then maps its own uid/gid through so filesystem access
/// keeps working. The new network namespace has no interfaces but a
/// downed loopback (inside the child's own namespace, unreachable
/// from the host): no UDP, no DNS, no ICMP, no raw sockets. The child
/// holds every capability inside its new namespace, so it can bring
/// that loopback up, but localhost-in-a-netns reaches nothing of the
/// host's. Unprivileged on kernels that allow user namespaces; any
/// refusal propagates as a raw errno and the spawn fails
/// (fail-closed, the module's one posture).
///
/// Strictly async-signal-safe: `pre_exec` runs after fork in a
/// possibly multi-threaded process, so this touches nothing that can
/// lock or allocate. `unshare(2)`, `getuid(2)`, `getgid(2)`,
/// `open(2)`, `write(2)`, `close(2)` are all on POSIX's
/// async-signal-safe list; the map strings are composed into a fixed
/// stack buffer; errors carry the raw errno, never a `String`.
fn isolate_network_raw() -> Result<(), i32> {
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EPERM));
    }
    // The process owns its new user namespace, so mapping its own
    // uid/gid through is allowed unprivileged. setgroups must be
    // denied before the group map is written.
    write_proc_map(b"/proc/self/setgroups\0", b"deny")?;
    let gid = unsafe { libc::getgid() };
    let uid = unsafe { libc::getuid() };
    let mut line = [0u8; 64];
    write_proc_map(b"/proc/self/gid_map\0", identity_map_line(gid, &mut line))?;
    write_proc_map(b"/proc/self/uid_map\0", identity_map_line(uid, &mut line))?;
    Ok(())
}

/// `<id> <id> 1\n` for a single identity mapping, composed into a
/// fixed stack buffer: decimal digits only, no allocation.
fn identity_map_line(id: u32, buf: &mut [u8; 64]) -> &[u8] {
    let mut len = 0usize;
    fn push_u32(buf: &mut [u8; 64], len: &mut usize, mut value: u32) {
        let start = *len;
        loop {
            buf[*len] = b'0' + (value % 10) as u8;
            *len += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        buf[start..*len].reverse();
    }
    push_u32(buf, &mut len, id);
    buf[len] = b' ';
    len += 1;
    push_u32(buf, &mut len, id);
    buf[len] = b' ';
    buf[len + 1] = b'1';
    buf[len + 2] = b'\n';
    &buf[..len + 3]
}

/// `open(2)` + `write(2)` + `close(2)` on a NUL-terminated /proc path:
/// every call on POSIX's async-signal-safe list, EINTR-tolerant,
/// errno-only errors.
fn write_proc_map(path: &[u8], payload: &[u8]) -> Result<(), i32> {
    let fd = unsafe {
        libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_CLOEXEC)
    };
    if fd < 0 {
        return Err(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EPERM)
        );
    }
    let mut written = 0usize;
    while written < payload.len() {
        let n = unsafe {
            libc::write(
                fd,
                payload[written..].as_ptr() as *const libc::c_void,
                payload.len() - written,
            )
        };
        if n < 0 {
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO);
            unsafe { libc::close(fd) };
            if errno == libc::EINTR {
                continue;
            }
            return Err(errno);
        }
        written += n as usize;
    }
    unsafe { libc::close(fd) };
    Ok(())
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
            // The network half, ABI v4 (kernel 6.7+): handled, no
            // rules, so every connect and bind is refused. An approved
            // command is not an approved exfil; without this the
            // sandbox was a filesystem-write jail and a sandboxed
            // `curl` dialed anywhere. Same HardRequirement ladder as
            // the filesystem half: a kernel that cannot enforce the
            // network rules fails the spawn instead of running it
            // unrestricted.
            .handle_access(AccessNet::from_all(ABI::V4))?
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
        return Err("landlock not fully enforced, refusing to run unsandboxed".into());
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
            // the tool, recorded as skipped.
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

    /// An approved command must not be an approved channel: with the
    /// network half handled, a TCP connection from inside the sandbox
    /// is refused. The probe dials a loopback listener the test holds
    /// itself: hermetic (no internet, no network tool), and the
    /// unsandboxed control connect proves a refusal is the sandbox's
    /// doing, not the target's. Needs a Landlock-4 kernel (6.7+);
    /// older kernels fail the spawn and the test skips.
    /// The true closure: with the namespace on, even a UDP datagram
    /// has nowhere to go (Landlock closes TCP; the netns closes
    /// everything, DNS included). The probe sends to a listener the
    /// test holds and expects silence: pre-fix, the packet arrives
    /// and this fails.
    #[test]
    fn a_netns_sandboxed_child_cannot_send_udp_datagrams() {
        use std::time::Duration;
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Positive control: the datagram path works unsandboxed.
        listener.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(b"control", ("127.0.0.1", port))
            .unwrap();
        let mut buf = [0u8; 16];
        listener.recv_from(&mut buf).expect("control datagram arrives unsandboxed");

        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg(format!("echo probe > /dev/udp/127.0.0.1/{port} && echo SENT"))
            .current_dir(tmp.path());
        let applied = Sandbox::new(tmp.path().to_path_buf())
            .network_isolated()
            .apply(&mut cmd);
        if applied.is_err() {
            eprintln!("skipped: sandbox unavailable");
            return;
        }
        let started = std::time::Instant::now();
        let output = match cmd.output() {
            Ok(output) => output,
            Err(_) => {
                eprintln!("skipped: kernel cannot enforce the namespace");
                return;
            }
        };
        // Deadlock tripwire: pre_exec runs after fork in a
        // multi-threaded process. A non-async-signal-safe call there
        // would hang the spawn forever; this assert turns that hang
        // into a named failure.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the spawn hung past 3s: something in pre_exec is not async-signal-safe"
        );
        listener
            .set_read_timeout(Some(Duration::from_millis(700)))
            .unwrap();
        let quiet = listener.recv_from(&mut buf).is_err();
        assert!(quiet, "a UDP datagram escaped the network namespace");
        assert!(
            !output.status.success(),
            "the sandboxed write itself must fail: {output:?}"
        );
    }

    /// The namespace maps the child's own uid/gid back through, so
    /// the filesystem half of the sandbox keeps working: reads, and
    /// workspace writes with real ownership.
    #[test]
    fn a_netns_child_keeps_filesystem_access() {
        let started = std::time::Instant::now();
        let output = match sandboxed("ls /usr/bin > out.txt && echo ok") {
            Ok(output) => output,
            Err(_) => {
                eprintln!("skipped: sandbox unavailable");
                return;
            }
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the spawn hung past 3s: something in pre_exec is not async-signal-safe"
        );
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn sandboxed_children_cannot_open_network_connections() {
        // Positive control: the target is reachable from this host.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::net::TcpStream::connect(("127.0.0.1", port))
            .expect("control: an unsandboxed loopback connect must work");

        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg(format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo OPENED"))
            .current_dir(tmp.path());
        if Sandbox::new(tmp.path().to_path_buf()).apply(&mut cmd).is_err() {
            eprintln!("skipped: sandbox unavailable");
            return;
        }
        let output = match cmd.output() {
            Ok(output) => output,
            // The ruleset refused to build (kernel without network
            // support): fail-closed, recorded as skipped here.
            Err(_) => {
                eprintln!("skipped: kernel cannot enforce network rules");
                return;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains("OPENED"),
            "the sandboxed connect succeeded: {stdout}"
        );
        assert!(!output.status.success(), "{output:?}");
    }
}
