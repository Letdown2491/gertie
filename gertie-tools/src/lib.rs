//! gertie-tools: gertie's built-in tools and MCP client.
//!
//! M2 ships the six local tools (read_file, write_file, edit_file, bash,
//! glob, grep). Web tools arrive with M1-leftover scope; MCP with M3.
//! All paths are workspace-root-relative and may not escape the root —
//! one sandbox rule, applied identically by every tool.

pub mod mcp;
pub mod shape;

use gertie_core::{Tool, ToolResult};
use serde::Deserialize;
use std::future::Future;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

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
    // resolution — that is M3's finer-grained permissions work.)
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
    Ok(joined)
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
/// model's JSON into a result on the polling thread. Local file/process
/// I/O is short and synchronous by nature; the loop runs on a background
/// executor, so a bounded block costs nothing interactive (bash's timeout
/// is the hard ceiling).
fn blocking<'a, F>(run: F) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>
where
    F: FnOnce() -> ToolResult + Send + 'a,
{
    Box::pin(async move { run() })
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

rooted!(ReadFile, WriteFile, EditFile, Bash, Glob, Grep);

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
}

/// Hard-blocked command shapes, checked before the approval card. The
/// bar: a pattern must be something *no* legitimate workspace command
/// matches — a false positive here is worse than one more click, because
/// the vet result never reaches a human. Anything debatable stays in the
/// prompt tier.
const DENY_PATTERNS: &[(&str, &str)] = &[
    // `rm -rf /`, `rm -fr /*`, and spaced-flag variants aimed at the root.
    (
        r"\brm\s+(-[a-zA-Z]+\s+)*-[a-zA-Z]*[rf][a-zA-Z]*\s+(/|/\*)\s*(;|$)",
        "recursive delete at the filesystem root",
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

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    /// Seconds; 30 by default. Killed (and reported) when exceeded.
    #[serde(default)]
    timeout: Option<u64>,
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
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFile { root: root.into() }),
        Arc::new(WriteFile { root: root.into() }),
        Arc::new(EditFile { root: root.into() }),
        Arc::new(Bash { root: root.into() }),
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
        blocking(move || {
            let args: ReadArgs = parse(arguments, "read_file")?;
            let path = resolve(&self.root, &args.path)?;
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
        blocking(move || {
            let args: WriteArgs = parse(arguments, "write_file")?;
            let path = resolve(&self.root, &args.path)?;
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
         unless replace_all is set — the edit is rejected otherwise, with \
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
        blocking(move || {
            let args: EditArgs = parse(arguments, "edit_file")?;
            let path = resolve(&self.root, &args.path)?;
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", args.path))?;

            let count = text.matches(&args.old_text).count();
            if args.old_text.is_empty() {
                return Err("edit_file: old_text is empty".into());
            }
            if count == 0 {
                return Err(format!(
                    "edit_file: old_text not found in {} — read the file and anchor on exact text",
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
        for (pattern, reason) in DENY_PATTERNS {
            if regex::Regex::new(pattern)
                .map(|re| re.is_match(&command))
                .unwrap_or(false)
            {
                return Err(format!("command matches a hard-blocked pattern ({reason})"));
            }
        }
        Ok(())
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        blocking(move || {
            let args: BashArgs = parse(arguments, "bash")?;
            let timeout = std::time::Duration::from_secs(args.timeout.unwrap_or(30).clamp(1, 300));

            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(&args.command)
                .current_dir(&self.root)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|error| format!("bash: {error}"))?;

            let start = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut output = String::new();
                        if let Some(mut stdout) = child.stdout.take() {
                            let _ = stdout.read_to_string(&mut output);
                        }
                        if let Some(mut stderr) = child.stderr.take() {
                            let _ = stderr.read_to_string(&mut output);
                        }
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
                        let _ = child.kill();
                        let _ = child.wait();
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
        blocking(move || {
            let args: GlobArgs = parse(arguments, "glob")?;
            let full = resolve(&self.root, &args.pattern)?;
            let mut matches: Vec<String> = glob::glob(&full.to_string_lossy())
                .map_err(|error| format!("glob: bad pattern ({error})"))?
                .filter_map(|entry| entry.ok())
                .filter(|path| path.is_file())
                .map(|path| {
                    path.strip_prefix(&self.root)
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
        blocking(move || {
            let args: GrepArgs = parse(arguments, "grep")?;
            let pattern = regex::Regex::new(&args.pattern)
                .map_err(|error| format!("grep: bad pattern ({error})"))?;
            let base = match &args.path {
                Some(path) => resolve(&self.root, path)?,
                None => self.root.clone(),
            };

            let mut matches: Vec<String> = Vec::new();
            for entry in walkdir::WalkDir::new(&base)
                .follow_links(false)
                .into_iter()
                // Depth-0 is the search root itself — tempdir names start
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
                                .strip_prefix(&self.root)
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

    #[test]
    fn destructive_commands_are_hard_blocked_before_any_prompt() {
        let tool = Bash::new("/tmp".into());
        for command in [
            r#"{"command": "rm -rf /"}"#,
            r#"{"command": "cd /tmp && rm -fr /*"}"#,
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
