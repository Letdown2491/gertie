//! gertie-tools: gertie's built-in tools and MCP client.
//!
//! M2 ships the six local tools (read_file, write_file, edit_file, bash,
//! glob, grep). Web tools arrive with M1-leftover scope; MCP with M3.
//! All paths are workspace-root-relative and may not escape the root,
//! one sandbox rule, applied identically by every tool.

pub mod mcp;
pub mod memory;
pub mod sandbox;
pub mod shape;
pub mod skills;
pub mod templates;
pub mod web;

use gertie_core::{Tool, ToolResult};
use serde::Deserialize;
use std::future::Future;
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;

/// Cap on text a tool will return in one result: the model's context is
/// the real constraint, and a huge blob is never what it needs.
const RESULT_CHAR_LIMIT: usize = 16_000;

/// Resolves `path` against the workspace root, refusing escapes. Absolute
/// paths under the root are fine; `..` walkouts are not.
fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(path);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };
    // Lexical containment: whatever remains after the root's prefix may
    // only use `..` to cancel normals it first added. (No symlink
    // resolution, that is M3's finer-grained permissions work.)
    let suffix = joined
        .strip_prefix(root)
        .map_err(|_| format!("path escapes the workspace: {path}"))?;
    let mut offset: isize = 0;
    for component in suffix.components() {
        match component {
            std::path::Component::ParentDir => offset -= 1,
            std::path::Component::Normal(_) => offset += 1,
            std::path::Component::CurDir => {}
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!("unsupported path: {path}"));
            }
        }
        if offset < 0 {
            return Err(format!("path escapes the workspace: {path}"));
        }
    }
    // Symlink hardening: lexical containment is not enough. A symlink
    // inside the workspace pointing out of it would turn an auto-approved
    // read, worse, an unapproved write, into filesystem access the
    // permission gate never saw. Whatever exists must canonicalize back
    // into the root. New files take their (canonicalized) parent's place.
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("workspace root: {error}"))?;
    let within = |target: &Path| -> Result<PathBuf, String> {
        if !target.starts_with(&canonical_root) {
            return Err(format!("path escapes the workspace: {path}"));
        }
        Ok(target.to_path_buf())
    };
    if let Ok(resolved) = joined.canonicalize() {
        return within(&resolved);
    }
    match joined.parent() {
        Some(parent) if parent.exists() => {
            let resolved_parent = parent
                .canonicalize()
                .map_err(|error| format!("{}: {error}", parent.display()))?;
            let file_name = joined
                .file_name()
                .ok_or_else(|| format!("unsupported path: {path}"))?;
            within(&resolved_parent.join(file_name))
        }
        // Parent missing too: canonicalize through the nearest
        // *existing* ancestor and join the nonexistent tail back on.
        // Returning the uncanonicalized path would leave a window
        // where a symlink planted in the not-yet-existing chain
        // redirects the write after the check.
        _ => {
            let mut ancestor = joined.clone();
            let mut tail = Vec::new();
            let resolved = loop {
                match ancestor.canonicalize() {
                    Ok(existing) => {
                        let mut resolved = existing;
                        for part in tail.iter().rev() {
                            resolved = resolved.join(part);
                        }
                        break resolved;
                    }
                    Err(_) => {
                        let Some(last) = ancestor.file_name().map(std::path::PathBuf::from) else {
                            // Walked past the root: nothing exists to
                            // traverse through; the caller's own error
                            // (and create_dir_all) handles it.
                            return Ok(joined);
                        };
                        let Some(parent) = ancestor.parent().map(std::path::Path::to_path_buf)
                        else {
                            return Ok(joined);
                        };
                        tail.push(last);
                        ancestor = parent;
                    }
                }
            };
            within(&resolved)
        }
    }
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: &str, tool: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|error| {
        format!("{tool}: bad arguments ({error}); match the tool's parameter schema")
    })
}

fn limit(text: String) -> String {
    if text.chars().count() <= RESULT_CHAR_LIMIT {
        return text;
    }
    let truncated: String = text.chars().take(RESULT_CHAR_LIMIT).collect();
    format!("{truncated}\n… [truncated]")
}

/// Shared plumbing: every tool holds its workspace root and turns the
/// model's JSON into a result. Local file/process I/O is short and
/// synchronous by nature, but not *bounded* by anything but the
/// tool's own ceiling (bash: 300 s). The closure runs on its own OS
/// thread and hands the result back through a oneshot: a wedged call
/// pins one anonymous thread, never the executor's polling loop.
/// Cancellation drops the future and abandons the thread, each tool
/// still bounds its own work, so an orphan self-terminates. The
/// closure owns what it captures (call sites clone their roots and
/// arguments; all cheap) because a detached thread outlives the poll.
fn blocking<F>(run: F) -> Pin<Box<dyn Future<Output = ToolResult> + Send>>
where
    F: FnOnce() -> ToolResult + Send + 'static,
{
    Box::pin(async move {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = sender.send(run());
        });
        match receiver.await {
            Ok(result) => result,
            // The sender only disappears if the thread panicked.
            Err(_) => Err("tool thread panicked".into()),
        }
    })
}

macro_rules! rooted {
    ($($ty:ident),* $(,)?) => {
        $(
            impl $ty {
                pub fn new(root: PathBuf) -> Self {
                    Self { root }
                }
            }
        )*
    };
}

rooted!(ReadFile, WriteFile, EditFile, Glob, Grep);

impl Bash {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            sandbox: None,
        }
    }
}

/// The structured question's registry entry. The agent loop
/// intercepts `ask_user` calls before any tool's `run` (the answer
/// rides the permission broker as deny-with-feedback, not a tool
/// result), so this type exists for the schema and the registry:
/// `run` never executes.
pub struct AskUser;

impl Tool for AskUser {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> String {
        "Ask the user a concrete question with clickable options when \
         the answer changes what you do next and you genuinely cannot \
         decide from context. Two to four short, distinct options; \
         allow_custom lets them type their own answer. Never use this \
         to escape thinking, to ask permission (the permission cards \
         do that), or to outsource a choice the context already \
         answers. For open-ended questions, ask in prose instead."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "the question, one sentence"},
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "two to four concrete answers"
                },
                "allow_custom": {"type": "boolean", "description": "let the user type their own answer"}
            },
            "required": ["question", "options"]
        })
    }

    fn run<'a>(
        &'a self,
        _arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        // Unreachable: the loop answers the ask before dispatch
        // reaches the registry.
        Box::pin(async { Err("ask_user is answered by the UI, never run".into()) })
    }
}

pub struct ReadFile {
    root: PathBuf,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

pub struct WriteFile {
    root: PathBuf,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct EditFile {
    root: PathBuf,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

pub struct Bash {
    root: PathBuf,
    /// Kernel sandbox (Landlock) for children; None = unsandboxed (the
    /// default, the permission gate is the safety net).
    sandbox: Option<Arc<sandbox::Sandbox>>,
}

/// Hard-blocked command shapes, checked before the approval card. The
/// bar: a pattern must be something *no* legitimate workspace command
/// matches, a false positive here is worse than one more click, because
/// the vet result never reaches a human. Anything debatable stays in the
/// prompt tier.
const DENY_PATTERNS: &[(&str, &str)] = &[
    // `rm -rf /`, `rm -fr /*`, and spaced-flag variants aimed at the root.
    (
        r"\brm\s+(-[a-zA-Z]+\s+)*-[a-zA-Z]*[rf][a-zA-Z]*\s+(/|/\*)\s*(;|$)",
        "recursive delete at the filesystem root",
    ),
    // The home directory and the workspace root deserve the same guard:
    // `rm -rf ~`, `rm -rf .`, `rm -rf ..`, bare `rm -rf *`.
    (
        r"\brm\s+(-[a-zA-Z]+\s+)*-[a-zA-Z]*[rf][a-zA-Z]*\s+~?/?\s*(;|$|&&|\|\|)",
        "recursive delete of the home directory",
    ),
    (
        r"\brm\s+(-[a-zA-Z]+\s+)*-[a-zA-Z]*[rf][a-zA-Z]*\s+\.{1,2}/?\s*(;|$|&&|\|\|)",
        "recursive delete of the current directory",
    ),
    (
        r"\brm\s+(-[a-zA-Z]+\s+)*-[a-zA-Z]*[rf][a-zA-Z]*\s+\*\s*(;|$)",
        "bare recursive delete of the workspace",
    ),
    (r"\bmkfs(\.\w+)?\b", "filesystem formatting"),
    (r"\bdd\b[^|;]*\bof=/dev/", "raw block-device write"),
    (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;", "fork bomb"),
    (r"chmod\s+(-R\s+)?777\s+/\s*(;|$)", "world-writable root"),
    (
        r">\s*/dev/(sd[a-z]|nvme\d+n\d+|hd[a-z])\b",
        "block-device clobber",
    ),
];

/// The deny list, compiled once. The vet runs on every bash call (the
/// hottest security path in the app), and `Regex::new` is not cheap:
/// a process-lifetime `OnceLock` buys lock-free reads after the first
/// call. A malformed pattern here is a compile-time review failure
/// anyway; the lock panics loudly rather than silently disabling the
/// guard.
fn deny_patterns() -> &'static [(regex::Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        DENY_PATTERNS
            .iter()
            .map(|(pattern, reason)| {
                (
                    regex::Regex::new(pattern).unwrap_or_else(|error| {
                        panic!("DENY_PATTERNS[{pattern}] is invalid: {error}")
                    }),
                    *reason,
                )
            })
            .collect()
    })
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    /// Seconds; 30 by default. Killed (and reported) when exceeded.
    #[serde(default)]
    timeout: Option<u64>,
}

/// True for env names that read as credentials: a substring test on
/// the uppercased name (API_KEY, TOKEN, SECRET, PASSWORD, PASSWD,
/// CREDENTIAL). Deliberately not AUTH alone: the ssh agent socket is
/// a path, not a key, and git over ssh breaks without it.
fn is_secret_shaped(key: &str) -> bool {
    // Names that merely contain a shape word and are known innocent:
    // TOKENIZERS_PARALLELISM tunes a tokenizer library, and stripping
    // it would silently degrade a working server.
    const NOT_SECRET: &[&str] = &["TOKENIZERS_PARALLELISM"];
    if NOT_SECRET.iter().any(|innocent| key.eq_ignore_ascii_case(innocent)) {
        return false;
    }
    let upper = key.to_ascii_uppercase();
    ["API_KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|shape| upper.contains(shape))
}

/// The environment a bash child starts from. Not the parent's: the
/// parent carries every provider key the config resolved, and a
/// command that needed a compiler should not inherit the credential
/// list. The allowlist is the working set a toolchain needs, PWD
/// names the workspace the command runs in. Walks vars_os: env is
/// arbitrary bytes on Linux, and vars() would panic on a value that
/// is not UTF-8.
fn bash_child_env(root: &Path) -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "SHLVL",
        "SSH_AUTH_SOCK",
        "XDG_RUNTIME_DIR",
    ];
    // vars_os, and entries that are not valid UTF-8 are skipped
    // rather than mangled: a command that genuinely needs one takes
    // it through its own tooling.
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            let value = value.to_str()?;
            KEEP.contains(&key).then(|| (key.to_string(), value.to_string()))
        })
        .collect();
    // Guaranteed, not passthrough: a command needs a place for its
    // temporaries and needs to know where it stands.
    env.push(("PWD".to_string(), root.display().to_string()));
    if !env.iter().any(|(key, _)| key == "TMPDIR") {
        env.push(("TMPDIR".to_string(), "/tmp".to_string()));
    }
    env
}

/// The environment an MCP stdio server starts from: the parent's,
/// minus the credential-shaped keys and minus whatever names the
/// config itself declared as credential carriers (`api_key_env`:
/// MY_LLM_CRED is a plausible spelling and no shape matches it).
/// Servers are third-party code; they need PATH and friends, they do
/// not need the LLM keys. A server that genuinely needs a credential
/// takes it through its own `env` config, which lands after the
/// filters: explicit user intent wins, and a same-named inherited
/// key cannot shadow it.
fn mcp_child_env(
    configured: &[(String, String)],
    deny_names: &[String],
) -> Vec<(String, String)> {
    // Same vars_os walk, same skip-not-mangle rule for non-UTF-8.
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            let value = value.to_str()?;
            (!is_secret_shaped(key) && !deny_names.iter().any(|deny| deny == key))
                .then(|| (key.to_string(), value.to_string()))
        })
        .collect();
    for (key, value) in configured {
        env.retain(|(existing, _)| existing != key);
        env.push((key.clone(), value.clone()));
    }
    env
}

pub struct Glob {
    root: PathBuf,
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
}

pub struct Grep {
    root: PathBuf,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    /// Subdirectory to search, root by default.
    #[serde(default)]
    path: Option<String>,
}

/// Builds the six local tools against one workspace root.
pub fn builtin_tools(root: &Path) -> gertie_core::ToolRegistry {
    builtin_tools_with(root, None)
}

/// Same, with an optional bash sandbox (opt-in Landlock isolation).
pub fn builtin_tools_with(
    root: &Path,
    sandbox: Option<Arc<sandbox::Sandbox>>,
) -> gertie_core::ToolRegistry {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFile { root: root.into() }),
        Arc::new(WriteFile { root: root.into() }),
        Arc::new(EditFile { root: root.into() }),
        Arc::new(Bash {
            root: root.into(),
            sandbox,
        }),
        Arc::new(Glob { root: root.into() }),
        Arc::new(Grep { root: root.into() }),
    ];
    let map: std::collections::BTreeMap<String, Arc<dyn Tool>> = tools
        .into_iter()
        .map(|tool| (tool.name().to_string(), tool))
        .collect();
    Arc::new(map)
}

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> String {
        "Read a file from the workspace. Returns the content with 1-based \
         line numbers. Use offset/limit for long files."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "workspace-relative path"},
                "offset": {"type": "integer", "description": "1-based first line"},
                "limit": {"type": "integer", "description": "max lines to read"}
            },
            "required": ["path"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let root = self.root.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: ReadArgs = parse(&arguments, "read_file")?;
            let path = resolve(&root, &args.path)?;
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", args.path))?;

            let skip = args.offset.unwrap_or(1).saturating_sub(1) as usize;
            let numbered: Vec<String> = text
                .lines()
                .skip(skip)
                .enumerate()
                .take(args.limit.unwrap_or(u64::MAX) as usize)
                .map(|(index, line)| format!("{:>6}  {line}", skip + index + 1))
                .collect();
            Ok(limit(numbered.join("\n")))
        })
    }
}

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> String {
        "Create or overwrite a file with the given content. The prior \
         version is checkpointed automatically; use edit_file to change \
         part of an existing file."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "workspace-relative path"},
                "content": {"type": "string", "description": "full file content"}
            },
            "required": ["path", "content"]
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let root = self.root.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: WriteArgs = parse(&arguments, "write_file")?;
            let path = resolve(&root, &args.path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            let existed = path.exists();
            std::fs::write(&path, &args.content)
                .map_err(|error| format!("{}: {error}", args.path))?;
            Ok(format!(
                "{} {} ({} bytes)",
                if existed { "overwrote" } else { "created" },
                args.path,
                args.content.len()
            ))
        })
    }
}

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> String {
        "Replace exact text in a file. old_text must match exactly once \
         unless replace_all is set, the edit is rejected otherwise, with \
         the match count, so you can correct the anchor. Checkpointed \
         automatically."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "workspace-relative path"},
                "old_text": {"type": "string", "description": "exact text to replace"},
                "new_text": {"type": "string", "description": "replacement text"},
                "replace_all": {"type": "boolean", "description": "replace every occurrence (default false)"}
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let root = self.root.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: EditArgs = parse(&arguments, "edit_file")?;
            let path = resolve(&root, &args.path)?;
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", args.path))?;

            let count = text.matches(&args.old_text).count();
            if args.old_text.is_empty() {
                return Err("edit_file: old_text is empty".into());
            }
            if count == 0 {
                return Err(format!(
                    "edit_file: old_text not found in {}, read the file and anchor on exact text",
                    args.path
                ));
            }
            if count > 1 && !args.replace_all {
                return Err(format!(
                    "edit_file: old_text matches {count} times in {}; \
                     include more surrounding text or set replace_all",
                    args.path
                ));
            }

            let edited = if args.replace_all {
                text.replace(&args.old_text, &args.new_text)
            } else {
                text.replacen(&args.old_text, &args.new_text, 1)
            };
            std::fs::write(&path, &edited).map_err(|error| format!("{}: {error}", args.path))?;
            Ok(format!(
                "edited {} ({count} occurrence{})",
                args.path,
                if count == 1 { "" } else { "s" }
            ))
        })
    }
}

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> String {
        "Run a shell command in the workspace via `sh -c`. Output \
         (stdout and stderr) is captured and truncated; long-running \
         commands are killed at the timeout. Requires approval."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "the shell command"},
                "timeout": {"type": "integer", "description": "seconds before the command is killed (default 30)"}
            },
            "required": ["command"]
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn vet(&self, arguments: &str) -> Result<(), String> {
        let command = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| {
                value
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();
        for (re, reason) in deny_patterns() {
            if re.is_match(&command) {
                return Err(format!("command matches a hard-blocked pattern ({reason})"));
            }
        }
        Ok(())
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let root = self.root.clone();
        let sandbox = self.sandbox.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: BashArgs = parse(&arguments, "bash")?;
            let timeout = std::time::Duration::from_secs(args.timeout.unwrap_or(30).clamp(1, 300));

            let mut child = {
                let mut command = std::process::Command::new("sh");
                command
                    .arg("-c")
                    .arg(&args.command)
                    .current_dir(&root)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    // One process group: the timeout takes the whole
                    // tree home, sh alone would die and leave its
                    // children running unsupervised.
                    .process_group(0);
                // A clean sheet: the parent env is the credential
                // list, the child gets the working set (bash_child_env).
                command.env_clear();
                for (key, value) in bash_child_env(&root) {
                    command.env(key, value);
                }
                if let Some(sandbox) = &sandbox {
                    sandbox.apply(&mut command)?;
                }
                command.spawn().map_err(|error| format!("bash: {error}"))?
            };

            // Drain the pipes while the command runs. A pipe holds
            // about 64 KB and a writer that fills it never exits, so
            // reading only after exit was a deadlock that masqueraded
            // as a timeout (any chatty `cargo test` hit it). Each
            // stream caps itself: the model sees limit() output
            // anyway, memory should not be the command's to set.
            const DRAIN_CAP: u64 = 2 * 1024 * 1024;
            // How long a finished command's stragglers get to close
            // the pipes before the group kill takes them. A
            // backgrounded child (`sleep 5 &`) inherits the write end
            // and can hold it open long after sh exited; without the
            // grace the result waits on the child's whole lifetime.
            const DRAIN_GRACE: std::time::Duration =
                std::time::Duration::from_millis(400);

            fn drain<R: std::io::Read + Send + 'static>(
                stream: Option<R>,
                done: std::sync::mpsc::Sender<()>,
            ) -> std::thread::JoinHandle<Vec<u8>> {
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    if let Some(stream) = stream {
                        let _ = stream.take(DRAIN_CAP).read_to_end(&mut buf);
                    }
                    let _ = done.send(());
                    buf
                })
            }
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let stdout = drain(child.stdout.take(), done_tx.clone());
            let stderr = drain(child.stderr.take(), done_tx);
            // The channel counts the drains, not the threads: both
            // senders dropped means both streams are at EOF.
            // Collect both drains: the pipes get a short grace to
            // reach EOF on their own (the ordinary case, immediate);
            // stragglers holding the write end past the grace get the
            // group kill, which is what produces the EOF.
            fn close_drains(
                child: &mut std::process::Child,
                done_rx: &std::sync::mpsc::Receiver<()>,
                stdout: std::thread::JoinHandle<Vec<u8>>,
                stderr: std::thread::JoinHandle<Vec<u8>>,
            ) -> String {
                let deadline = std::time::Instant::now() + DRAIN_GRACE;
                let mut pending = 2;
                while pending > 0 {
                    match done_rx.recv_timeout(
                        deadline.saturating_duration_since(std::time::Instant::now()),
                    ) {
                        Ok(()) => pending -= 1,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // SAFETY: the group id is the child's own
                            // (process_group(0)), a group this process
                            // created for exactly this command. The
                            // stragglers holding the pipe ends are in
                            // it; killing them is what produces the
                            // EOF the drains are waiting on.
                            unsafe {
                                libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                            }
                            break;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                let mut output = String::new();
                output
                    .push_str(&String::from_utf8_lossy(&stdout.join().unwrap_or_default()));
                output
                    .push_str(&String::from_utf8_lossy(&stderr.join().unwrap_or_default()));
                output
            }

            let start = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let output =
                            close_drains(&mut child, &done_rx, stdout, stderr);
                        // Shape once, here: the model-visible string and
                        // the persisted string are the same string.
                        let summary = limit(shape::shape(&args.command, output.trim_end()));
                        if !status.success() {
                            return Ok(format!("exit {status}:\n{summary}"));
                        }
                        return Ok(if summary.is_empty() {
                            "(no output)".into()
                        } else {
                            summary
                        });
                    }
                    Ok(None) if start.elapsed() < timeout => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Ok(None) => {
                        // The group, not just sh: children of the
                        // command must not outlive the kill and
                        // finish their work unsupervised.
                        // SAFETY: the group id is the child's own
                        // (process_group(0) above), a group this
                        // process created for exactly this command.
                        unsafe {
                            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                        }
                        let _ = child.wait();
                        let _ = stdout.join();
                        let _ = stderr.join();
                        return Err(format!("bash: timed out after {}s", timeout.as_secs()));
                    }
                    Err(error) => return Err(format!("bash: {error}")),
                }
            }
        })
    }
}

impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> String {
        "Find files by glob pattern relative to the workspace root, \
         e.g. `src/**/*.rs`. Returns workspace-relative paths, sorted."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "glob pattern, workspace-relative"}
            },
            "required": ["pattern"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let root = self.root.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: GlobArgs = parse(&arguments, "glob")?;
            let full = resolve(&root, &args.pattern)?;
            let mut matches: Vec<String> = glob::glob(&full.to_string_lossy())
                .map_err(|error| format!("glob: bad pattern ({error})"))?
                .filter_map(|entry| entry.ok())
                .filter(|path| path.is_file())
                .map(|path| {
                    path.strip_prefix(&root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            matches.sort();
            Ok(limit(if matches.is_empty() {
                "(no matches)".into()
            } else {
                matches.join("\n")
            }))
        })
    }
}

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> String {
        "Search file contents with a regular expression across the \
         workspace (skips binary files, hidden dirs, target/, and \
         node_modules/). Returns `path:line: text` matches."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "regular expression"},
                "path": {"type": "string", "description": "subdirectory to search (default: workspace root)"}
            },
            "required": ["pattern"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let root = self.root.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: GrepArgs = parse(&arguments, "grep")?;
            let pattern = regex::Regex::new(&args.pattern)
                .map_err(|error| format!("grep: bad pattern ({error})"))?;
            let base = match &args.path {
                Some(path) => resolve(&root, path)?,
                None => root.clone(),
            };

            let mut matches: Vec<String> = Vec::new();
            for entry in walkdir::WalkDir::new(&base)
                .follow_links(false)
                .into_iter()
                // Depth-0 is the search root itself, tempdir names start
                // with '.', and it must never filter itself out.
                .filter_entry(|entry| {
                    entry.depth() == 0 || {
                        let name = entry.file_name().to_string_lossy();
                        !matches!(
                            name.as_ref(),
                            ".git" | "target" | "node_modules" | ".pixi" | ".venv"
                        ) && !name.starts_with('.')
                    }
                })
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file())
            {
                let Ok(mut file) = std::fs::File::open(entry.path()) else {
                    continue;
                };
                let mut content = String::new();
                if file.read_to_string(&mut content).is_err() {
                    continue; // binary or non-UTF-8: not grep's business
                }
                for (index, line) in content.lines().enumerate() {
                    if pattern.is_match(line) {
                        // A single minified line would eat the context
                        // window; cap each match.
                        let line: String = line.trim_end().chars().take(200).collect();
                        matches.push(format!(
                            "{}:{}: {}",
                            entry
                                .path()
                                .strip_prefix(&root)
                                .unwrap_or(entry.path())
                                .display(),
                            index + 1,
                            line
                        ));
                        if matches.len() >= 200 {
                            return Ok(limit(format!(
                                "{}\n… [stopped at 200 matches]",
                                matches.join("\n")
                            )));
                        }
                    }
                }
            }
            Ok(limit(if matches.is_empty() {
                "(no matches)".into()
            } else {
                matches.join("\n")
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn await_sync<F: Future>(future: F) -> F::Output {
        // Local tool futures complete on their first poll (no awaits
        // inside), so a manual poll loop suffices in tests.
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn resolve_containment_math() {
        let dir = workspace();
        let root = dir.path().to_path_buf();
        assert!(resolve(&root, "a.txt").is_ok());
        assert!(resolve(&root, "src/../lib.rs").is_ok());
        assert!(resolve(&root, "../../etc/passwd").is_err());
        assert!(resolve(&root, "/etc/passwd").is_err());
        // Absolute under the root is allowed.
        assert!(resolve(&root, &root.join("a.txt").to_string_lossy()).is_ok());
    }

    #[test]
    fn a_new_path_resolves_through_the_nearest_existing_ancestor() {
        let dir = workspace();
        // The root reached through a symlink, the spelling the caller
        // uses is not the disk's.
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(dir.path(), &link).unwrap();
            // Deeply new: the path, its parent, and its grandparent do
            // not exist. The old fallback returned the uncanonicalized
            // spelling, a window for a later-planted symlink. It must
            // come back canonicalized: nearest existing ancestor
            // (the root, through the link) with the tail joined on.
            let resolved = resolve(&link, "a/b/c.txt").unwrap();
            let canonical_root = dir.path().canonicalize().unwrap();
            assert!(
                resolved.starts_with(&canonical_root),
                "resolved through the link's spelling: {resolved:?}"
            );
            assert_eq!(resolved, canonical_root.join("a").join("b").join("c.txt"));
        }
    }

    #[test]
    fn read_file_numbers_lines_and_respects_offset() {
        let dir = workspace();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let tool = ReadFile::new(dir.path().into());

        let all = await_sync(tool.run(r#"{"path": "a.txt"}"#)).unwrap();
        assert!(all.contains("1  one"));
        assert!(all.contains("3  three"));

        let tail = await_sync(tool.run(r#"{"path": "a.txt", "offset": 2}"#)).unwrap();
        assert!(tail.contains("2  two"));
        assert!(!tail.contains("one"));
    }

    #[test]
    fn paths_may_not_escape_the_workspace() {
        let dir = workspace();
        let tool = ReadFile::new(dir.path().into());
        let result = await_sync(tool.run(r#"{"path": "../../etc/passwd"}"#));
        assert!(result.unwrap_err().contains("escapes the workspace"));
    }

    #[test]
    fn symlinks_may_not_smuggle_paths_out_of_the_workspace() {
        let dir = workspace();
        let root = dir.path().to_path_buf();
        std::os::unix::fs::symlink("/etc", dir.path().join("escape")).unwrap();
        // Read through the symlink: refused even though the lexical path
        // never leaves the root.
        let reader = ReadFile::new(root.clone());
        let result = await_sync(reader.run(r#"{"path": "escape/passwd"}"#));
        assert!(result.unwrap_err().contains("escapes the workspace"));

        // A symlinked *file* equally.
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("host")).unwrap();
        let result = await_sync(reader.run(r#"{"path": "host"}"#));
        assert!(result.unwrap_err().contains("escapes the workspace"));

        // And the canonical view of the root itself stays readable.
        std::fs::write(dir.path().join("real.txt"), "here").unwrap();
        assert!(resolve(&root, "real.txt").is_ok());
    }

    #[test]
    fn edit_file_rejects_ambiguous_and_missing_anchors() {
        let dir = workspace();
        std::fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();
        let tool = EditFile::new(dir.path().into());

        let ambiguous =
            await_sync(tool.run(r#"{"path": "a.txt", "old_text": "x", "new_text": "y"}"#))
                .unwrap_err();
        assert!(ambiguous.contains("2 times"));

        let missing =
            await_sync(tool.run(r#"{"path": "a.txt", "old_text": "zzz", "new_text": "y"}"#))
                .unwrap_err();
        assert!(missing.contains("not found"));

        await_sync(
            tool.run(r#"{"path": "a.txt", "old_text": "x", "new_text": "y", "replace_all": true}"#),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "y\ny\n"
        );
    }

    #[test]
    fn bash_captures_output_and_exit_status() {
        let dir = workspace();
        let tool = Bash::new(dir.path().into());

        let hello = await_sync(tool.run(r#"{"command": "echo hello"}"#)).unwrap();
        assert_eq!(hello, "hello");

        let failed = await_sync(tool.run(r#"{"command": "echo boom >&2; exit 3"}"#)).unwrap();
        assert!(failed.starts_with("exit ") && failed.contains("boom"));
    }

    #[test]
    fn bash_times_out() {
        let dir = workspace();
        let tool = Bash::new(dir.path().into());
        let result = await_sync(tool.run(r#"{"command": "sleep 5", "timeout": 1}"#));
        assert!(result.unwrap_err().contains("timed out"));
    }

    /// A command that out-writes the pipe buffer (~64 KB) used to
    /// wedge: the loop read the pipes only after the child exited,
    /// and a child blocked on a full pipe never exits. The drain has
    /// to run while the command runs.
    #[test]
    fn a_chatty_command_comes_home_before_the_timeout() {
        let dir = workspace();
        let tool = Bash::new(dir.path().into());
        let output = await_sync(
            tool.run(r#"{"command": "echo done-marker; head -c 204800 /dev/zero | tr '\\0' 'a'", "timeout": 3}"#),
        )
        .expect("200 KB of output must not deadlock the pipe");
        assert!(output.contains("done-marker"));
        assert!(output.contains("aaaa"), "the streamed body must be captured");
    }

    /// A backgrounded child inherits the stdout pipe and holds it open
    /// past the parent sh's exit: waiting on the drains must not turn
    /// `sleep 5 & echo done` into a 5-second tool call. The command
    /// finished; stragglers still holding the pipe get the group kill
    /// after a short grace, and the result comes home promptly.
    #[test]
    fn a_backgrounded_child_does_not_hang_the_result() {
        let dir = workspace();
        let tool = Bash::new(dir.path().into());
        let started = std::time::Instant::now();
        let output = await_sync(
            tool.run(r#"{"command": "sleep 5 & echo done", "timeout": 3}"#),
        )
        .expect("a backgrounded child must not hang the finished command");
        let elapsed = started.elapsed();
        assert!(output.contains("done"), "{output}");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "the result waited {elapsed:?} on a child that inherited the pipe"
        );
    }

    /// Environment variables are arbitrary bytes on Linux: vars()
    /// panics on a non-UTF-8 value, and the panic would fire inside
    /// every bash spawn and MCP connect. The env builders walk
    /// vars_os and never panic.
    #[cfg(unix)]
    #[test]
    fn non_utf8_env_does_not_panic() {
        use std::os::unix::ffi::OsStringExt as _;
        // SAFETY: single-threaded use here; names are unique to this test.
        unsafe {
            std::env::set_var(
                std::ffi::OsString::from_vec(b"GERTIE_AUDIT_RAW".to_vec()),
                std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'x']),
            );
        }
        let env = bash_child_env("/workspace/probe".as_ref());
        assert!(!env.iter().any(|(key, _)| key == "GERTIE_AUDIT_RAW"));
        let mcp = mcp_child_env(&[], &[]);
        assert!(!mcp.iter().any(|(key, _)| key == "GERTIE_AUDIT_RAW"));
    }

    /// Endpoints resolve keys from env names the config names
    /// (api_key_env), and those names need not look secret-shaped:
    /// MY_LLM_CRED is a plausible spelling. Whatever the config
    /// named as a credential is denied to MCP servers by name, on
    /// top of the shape filter.
    #[test]
    fn mcp_env_honors_configured_deny_names() {
        // SAFETY: single-threaded use here; names are unique to this test.
        unsafe {
            std::env::set_var("GERTIE_AUDIT_CRED", "hunter2");
        }
        let env = mcp_child_env(&[], &["GERTIE_AUDIT_CRED".to_string()]);
        assert!(
            env.iter().all(|(key, _)| key != "GERTIE_AUDIT_CRED"),
            "a configured credential name reached the server"
        );
        // A name that is only shape-matched still goes through the shape.
        let env = mcp_child_env(&[], &[]);
        assert!(env.iter().all(|(key, _)| !is_secret_shaped(key)));
    }

    /// The timeout kills the whole tree: a backgrounded grandchild
    /// must not outlive the kill and finish its work unsupervised.
    #[test]
    fn a_timed_out_command_takes_its_children_with_it() {
        let dir = workspace();
        let marker = dir.path().join("marker");
        let tool = Bash::new(dir.path().into());
        let result = await_sync(tool.run(
            r#"{"command": "(sleep 2 && touch marker) & sleep 30", "timeout": 1}"#,
        ));
        assert!(result.unwrap_err().contains("timed out"));
        // The grandchild wanted to land at 2s. If the process group
        // died at ~1s, nothing lands. Poll past that window.
        for _ in 0..30 {
            if marker.exists() {
                panic!("the grandchild survived the timeout and wrote its marker");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// A bash child starts from a clean sheet: the parent carries
    /// every provider key the config resolved, and a command that
    /// only needed a compiler should not inherit the credential list.
    /// The working set (PATH, HOME) still travels, so builds run.
    #[test]
    fn bash_children_start_from_a_clean_environment() {
        let dir = workspace();
        let tool = Bash::new(dir.path().into());
        // SAFETY: single-threaded use here; the name is unique to this test.
        unsafe {
            std::env::set_var("GERTIE_AUDIT_LEAK_PROBE", "bash-hunter2");
        }
        let leaked =
            await_sync(tool.run(r#"{"command": "echo $GERTIE_AUDIT_LEAK_PROBE", "timeout": 3}"#))
                .unwrap();
        assert!(
            !leaked.contains("bash-hunter2"),
            "the child saw the parent's environment: {leaked}"
        );
        let working = await_sync(
            tool.run(r#"{"command": "echo PATH=$PATH HOME=$HOME", "timeout": 3}"#),
        )
        .unwrap();
        assert!(!working.contains("PATH= HOME="), "the working set must travel");
    }

    /// The allowlist is the working set, nothing else: seen here as a
    /// pure function so the list is legible in one place. PWD and
    /// TMPDIR are guaranteed, not passthrough.
    #[test]
    fn bash_env_allowlist_keeps_the_working_set() {
        // SAFETY: single-threaded use here; the name is allowlist
        // space and the value unique to this test.
        unsafe {
            std::env::set_var("TERM", "GERTIE_AUDIT_VT100");
        }
        let env = bash_child_env("/workspace/probe".as_ref());
        let names: Vec<&str> = env.iter().map(|(key, _)| key.as_str()).collect();
        for key in ["PATH", "HOME", "PWD", "TMPDIR", "SSH_AUTH_SOCK", "TERM"] {
            assert!(names.contains(&key), "{key} must survive the allowlist");
        }
        assert!(
            env.iter().any(|(key, value)| key == "TERM" && value == "GERTIE_AUDIT_VT100"),
            "a set working-set value passes through"
        );
        assert!(
            !env.iter().any(|(key, _)| is_secret_shaped(key)),
            "no secret-shaped key rides the allowlist"
        );
        let pwd = env.iter().find(|(key, _)| key == "PWD").map(|(_, v)| v.as_str());
        assert_eq!(pwd, Some("/workspace/probe"));
        let tmpdir = env
            .iter()
            .find(|(key, _)| key == "TMPDIR")
            .map(|(_, v)| v.as_str());
        assert_eq!(tmpdir, Some("/tmp"));
    }

    /// MCP servers are third-party code and still need a working
    /// environment; the filter drops the credential-shaped keys and
    /// keeps the rest, and a server's own configured env is explicit
    /// user intent, it lands verbatim after the filter.
    #[test]
    fn mcp_env_strips_secret_shaped_keys_but_stays_working() {
        // SAFETY: single-threaded use here; names are unique to this test.
        unsafe {
            std::env::set_var("GERTIE_AUDIT_MCP_API_KEY", "mcp-hunter2");
            std::env::set_var("GERTIE_AUDIT_AWS_SECRET_ACCESS_KEY", "mcp-hunter2");
            std::env::set_var("GERTIE_AUDIT_HARMLESS", "fine");
        }
        let env = mcp_child_env(&[], &[]);
        assert!(
            env.iter().all(|(_, value)| *value != "mcp-hunter2"),
            "a secret-shaped key reached the server"
        );
        assert!(env.iter().any(|(k, v)| k == "GERTIE_AUDIT_HARMLESS" && v == "fine"));
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        let configured = mcp_child_env(&[("SERVER_TOKEN".to_string(), "by-request".to_string())], &[]);
        assert!(
            configured
                .iter()
                .any(|(k, v)| k == "SERVER_TOKEN" && v == "by-request"),
            "the server's own env lands verbatim"
        );
    }

    /// The shape test itself: what counts as a credential name, and
    /// the working-set names that must never match it (AUTH alone is
    /// not a credential: the ssh agent socket is a path, not a key).
    #[test]
    fn secret_shaped_env_keys_are_recognized() {
        for key in [
            "MY_API_KEY",
            "AUTH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "GERTIE_OLLAMA_API_KEY",
            "CI_JOB_TOKEN",
            "client_secret_file",
        ] {
            assert!(is_secret_shaped(key), "{key} should read as a credential");
        }
        for key in [
            "PATH",
            "HOME",
            "LANG",
            "TERM",
            "SSH_AUTH_SOCK",
            "HTTP_PROXY",
            "NO_COLOR",
            "XDG_RUNTIME_DIR",
        ] {
            assert!(!is_secret_shaped(key), "{key} should read as working set");
        }
    }

    #[test]
    fn destructive_commands_are_hard_blocked_before_any_prompt() {
        let tool = Bash::new("/tmp".into());
        for command in [
            r#"{"command": "rm -rf /"}"#,
            r#"{"command": "cd /tmp && rm -fr /*"}"#,
            r#"{"command": "rm -rf ~"}"#,
            r#"{"command": "rm -rf ~/"}"#,
            r#"{"command": "sudo rm -rf /"}"#,
            r#"{"command": "rm -rf ."}"#,
            r#"{"command": "rm -rf .."}"#,
            r#"{"command": "rm -rf *"}"#,
            r#"{"command": "mkfs.ext4 /dev/sda1"}"#,
            r#"{"command": "dd if=zero of=/dev/sda"}"#,
            r#"{"command": ":(){ :|:& };:"}"#,
            r#"{"command": "chmod -R 777 /"}"#,
        ] {
            let blocked = tool.vet(command).unwrap_err();
            assert!(blocked.contains("hard-blocked"), "{command}: {blocked}");
        }
    }

    #[test]
    fn legitimate_commands_pass_the_vet() {
        let tool = Bash::new("/tmp".into());
        for command in [
            r#"{"command": "rm -rf ./build"}"#,
            r#"{"command": "cat README.md | grep reboot"}"#,
            r#"{"command": "ls -la && cargo test"}"#,
            r#"{"command": "echo shutdown hooks in src/main.rs"}"#,
        ] {
            tool.vet(command)
                .unwrap_or_else(|e| panic!("{command}: {e}"));
        }
    }

    #[test]
    fn glob_and_grep_find_what_they_should() {
        let dir = workspace();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "// helper\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "# notes\n").unwrap();
        let glob_tool = Glob::new(dir.path().into());
        let grep_tool = Grep::new(dir.path().into());

        let rs_files = await_sync(glob_tool.run(r#"{"pattern": "src/*.rs"}"#)).unwrap();
        assert_eq!(rs_files, "src/lib.rs\nsrc/main.rs");

        let hits = await_sync(grep_tool.run(r#"{"pattern": "helper"}"#)).unwrap();
        assert!(hits.contains("src/lib.rs:1"));
    }
}
