//! Skills: runtime SKILL.md support — the Agent Skills format (name,
//! description-as-triggers, markdown body), installed as directories and
//! loaded on demand. Progressive disclosure in three levels:
//!
//! 1. the system prompt carries each skill's name + one-line description
//!    (the context-load budget — description text says *when to use*,
//!    never a workflow summary),
//! 2. `load_skill` returns the full body when the model (or the user,
//!    via the model) wants the discipline,
//! 3. supporting files next to SKILL.md are read through the same tool,
//!    sandboxed to the skill's directory.
//!
//! Trust posture: skills are user-installed files — config-level trust —
//! but nothing here executes anything. A skill that tells the model to
//! run a command sends it through `bash` and its permission gate like
//! any other instruction.

use crate::{Tool, ToolResult, blocking};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One discovered skill: identity, trigger text, and where its files live.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    /// "Use when…" triggers only. Injected into the system prompt.
    pub description: String,
    /// Directory holding SKILL.md and supporting files.
    pub dir: PathBuf,
}

/// Limits from the Agent Skills anatomy spec: the description is
/// context load paid every turn; the body budget keeps a loaded skill
/// from eating the window.
const MAX_DESCRIPTION: usize = 1024;
const MAX_BODY_LINES: usize = 500;

/// Parses one SKILL.md. Frontmatter is the two fields we use, scanned
/// line by line — no YAML dependency, and unknown fields are tolerated.
fn parse_skill(dir: &Path) -> Result<Skill, String> {
    let raw = std::fs::read_to_string(dir.join("SKILL.md"))
        .map_err(|error| format!("{}: {error}", dir.display()))?;

    let body = raw
        .trim_start()
        .strip_prefix("---")
        .ok_or_else(|| format!("{}: SKILL.md has no frontmatter block", dir.display()))?;
    let (frontmatter, _) = body
        .split_once("---")
        .ok_or_else(|| format!("{}: unterminated frontmatter", dir.display()))?;

    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("description:") {
            description = Some(value.trim().to_string());
        }
    }
    let name = name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("{}: frontmatter has no name", dir.display()))?;
    let description = description
        .filter(|description| !description.is_empty())
        .ok_or_else(|| format!("{}: frontmatter has no description", dir.display()))?;

    if name.chars().next().is_some_and(|c| c.is_ascii_digit())
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "{}: skill name `{name}` is not a slug",
            dir.display()
        ));
    }

    Ok(Skill {
        name,
        description: crate::mcp::sanitize_description(description.as_str(), MAX_DESCRIPTION),
        dir: dir.to_path_buf(),
    })
}

/// Discovers skills from two roots: the user dir first, the project dir
/// overriding by name (a project pins its own discipline). A directory
/// that fails to parse is a warning, never a failed launch — same
/// posture as MCP servers.
pub fn discover(user_dir: &Path, project_dir: &Path) -> (Vec<Skill>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut discovered: Vec<(bool, Skill)> = Vec::new(); // (is_project, skill)
    for (is_project, root) in [(false, user_dir), (true, project_dir)] {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue, // absent root: nothing to discover
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            match parse_skill(&entry.path()) {
                // Sort key below: project first, then name — so the
                // dedup keeps the project's version of a shared name.
                Ok(skill) => discovered.push((is_project, skill)),
                Err(error) => warnings.push(format!(
                    "skill ({}): {error}",
                    if is_project { "project" } else { "user" }
                )),
            }
        }
    }
    discovered.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let mut skills: Vec<Skill> = Vec::new();
    for (_, skill) in discovered {
        // First-wins: the project's skill shadows the user's.
        if skills.iter().any(|kept| kept.name == skill.name) {
            continue;
        }
        skills.push(skill);
    }
    (skills, warnings)
}

/// The system-prompt index: one line per skill, name + trigger text.
/// This is the entire per-turn context cost of having skills installed.
pub fn index_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut index = String::from(
        "## Skills\n\n\
         Installed skills you can load with the load_skill tool when a \
         situation matches:\n\n",
    );
    for skill in skills {
        index.push_str(&format!("- **{}** — {}\n", skill.name, skill.description));
    }
    index
}

/// Loads a skill's body (or a supporting file beside it). Sandboxed to
/// the skill's own directory: skills live outside the workspace root,
/// so read_file can't reach them and this tool never strays either.
pub struct LoadSkill {
    skills: Arc<std::collections::BTreeMap<String, PathBuf>>,
}

impl LoadSkill {
    pub fn new(skills: &[Skill]) -> Self {
        Self {
            skills: Arc::new(
                skills
                    .iter()
                    .map(|skill| (skill.name.clone(), skill.dir.clone()))
                    .collect(),
            ),
        }
    }
}

#[derive(serde::Deserialize)]
struct LoadArgs {
    skill: String,
    /// Optional supporting file, relative to the skill directory.
    path: Option<String>,
}

impl Tool for LoadSkill {
    fn name(&self) -> &'static str {
        "load_skill"
    }

    fn description(&self) -> String {
        "Load an installed skill's full instructions. Call when a task \
         matches a skill from the index; pass `path` for a supporting \
         file listed by the skill itself."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {"type": "string", "description": "skill name from the index"},
                "path": {"type": "string", "description": "optional supporting file, relative to the skill directory"}
            },
            "required": ["skill"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        blocking(move || {
            let args: LoadArgs = crate::parse(arguments, "load_skill")?;
            let dir = self
                .skills
                .get(&args.skill)
                .ok_or_else(|| {
                    format!(
                        "unknown skill: {} (see the index in the system prompt)",
                        args.skill
                    )
                })?
                .clone();

            let target = match &args.path {
                None => dir.join("SKILL.md"),
                Some(path) => {
                    let target = dir.join(path);
                    // Sandbox: canonicalized target must stay inside the
                    // skill dir — same escape rule as every other tool.
                    let canonical = target
                        .canonicalize()
                        .map_err(|error| format!("skill file {path}: {error}"))?;
                    if !canonical.starts_with(&dir) {
                        return Err(format!("skill file {path}: escapes the skill directory"));
                    }
                    canonical
                }
            };

            let body = std::fs::read_to_string(&target)
                .map_err(|error| format!("{}: {error}", target.display()))?;
            if args.path.is_none() {
                // Budget check on the main file only; supporting files
                // are references the skill chose to ship.
                let lines = body.lines().count();
                if lines > MAX_BODY_LINES {
                    return Err(format!(
                        "SKILL.md is {lines} lines (budget {MAX_BODY_LINES}) — refusing to load; the skill needs trimming"
                    ));
                }
            }
            Ok(body)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_dir(name: &str, description: &str, body: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gertie-skill-test-{}-{}", name, std::process::id()));
        let dir = dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let dir = skill_dir("test-skill", "Use when testing", "# Steps\n1. do it");
        let (skills, warnings) = discover(dir.parent().unwrap(), Path::new("/nonexistent"));

        assert!(warnings.is_empty());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
        assert_eq!(skills[0].description, "Use when testing");
    }

    #[test]
    fn broken_skills_warn_without_failing_launch() {
        let root = std::env::temp_dir().join(format!("gertie-skill-bad-{}", std::process::id()));
        std::fs::create_dir_all(root.join("no-frontmatter")).unwrap();
        std::fs::write(root.join("no-frontmatter/SKILL.md"), "just text").unwrap();
        std::fs::create_dir_all(root.join("no-description")).unwrap();
        std::fs::write(
            root.join("no-description/SKILL.md"),
            "---\nname: no-description\n---\nbody",
        )
        .unwrap();

        let (skills, warnings) = discover(&root, Path::new("/nonexistent"));
        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn project_overrides_user_by_name() {
        let user = skill_dir("shared", "user version", "user body");
        let project = skill_dir("shared", "project version", "project body");
        let user_root = user.parent().unwrap().to_path_buf();
        let project_root = project.parent().unwrap().to_path_buf();

        let (skills, _) = discover(&user_root, &project_root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "project version");
    }

    #[test]
    fn load_returns_body_and_supporting_files_stay_inside_the_dir() {
        let dir = skill_dir("loader", "Use when loading", "the full discipline");
        std::fs::write(dir.join("reference.md"), "deep detail").unwrap();

        let tool = LoadSkill::new(&discover(dir.parent().unwrap(), Path::new("/x")).0);

        let body = await_sync(tool.run(r#"{"skill": "loader"}"#)).unwrap();
        assert!(body.contains("the full discipline"));

        let reference =
            await_sync(tool.run(r#"{"skill": "loader", "path": "reference.md"}"#)).unwrap();
        assert_eq!(reference, "deep detail");

        // Escape attempt: rejected.
        let escape = tool.run(r#"{"skill": "loader", "path": "../../etc/passwd"}"#);
        assert!(await_sync(escape).is_err());
        // Unknown skill: a model-visible error, not a panic.
        assert!(await_sync(tool.run(r#"{"skill": "nope"}"#)).is_err());
    }

    #[test]
    fn oversized_bodies_are_refused() {
        let dir = skill_dir("huge", "Use never", &"line\n".repeat(501));
        let tool = LoadSkill::new(&discover(dir.parent().unwrap(), Path::new("/x")).0);
        assert!(
            await_sync(tool.run(r#"{"skill": "huge"}"#))
                .unwrap_err()
                .contains("budget")
        );
    }

    fn await_sync(
        mut future: std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + '_>>,
    ) -> ToolResult {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            match std::pin::Pin::new(&mut future).poll(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }
}
