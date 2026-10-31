//! Prompt templates: reusable prompts as flat files, invoked from the
//! composer as `/name`. The same two-root convention as skills, user
//! dir beside `config.toml`, project dir `.gertie/templates`, with the
//! project shadowing the user by name. Discovery problems are
//! warnings, never a failed launch.
//!
//! A template is a Markdown (or plain text) file whose stem is the
//! command name. Optional frontmatter carries a `description:` used
//! only when listing; the rest of the file is the prompt body. The
//! placeholder `{{args}}` receives whatever followed the command
//! (`/explain this fn`); without the placeholder, arguments are
//! appended after a blank line.
//!
//! Builtins (`/compact`, `/export`) are handled by the UI before
//! lookup here, and always win over same-named files.

use std::path::Path;

/// One discovered template: the command name, an optional listing
/// description, and the prompt body.
#[derive(Clone, Debug)]
pub struct Template {
    pub name: String,
    pub description: String,
    pub body: String,
}

impl Template {
    /// Renders the prompt: `{{args}}` substitution when present,
    /// appended arguments otherwise.
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        if self.body.contains("{{args}}") {
            self.body.replace("{{args}}", args)
        } else if args.is_empty() {
            self.body.trim().to_string()
        } else {
            format!("{}\n\n{}", self.body.trim(), args)
        }
    }
}

/// Splits a composer input into a slash invocation: `Some((name, args))`
/// when the text starts with `/name` and the name is a slug. Anything
/// else, a bare `/`, a path like `/usr/bin`, a leading space, is not
/// an invocation and returns `None`, so it sends as a normal message.
pub fn parse_invocation(input: &str) -> Option<(String, String)> {
    let input = input.strip_prefix('/')?;
    let (name, args) = match input.split_once([' ', '\t', '\n']) {
        Some((name, args)) => (name, args),
        None => (input, ""),
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((name.to_ascii_lowercase(), args.to_string()))
}

/// Finds a template by command name.
pub fn lookup<'a>(templates: &'a [Template], name: &str) -> Option<&'a Template> {
    templates.iter().find(|template| template.name == name)
}

/// Parses one template file. The stem is the name (slug-validated,
/// same rule as skills); frontmatter is optional and only the
/// `description:` field is read.
fn parse_template(path: &Path) -> Result<Template, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("{}: unreadable file name", path.display()))?;

    let mut description: Option<String> = None;
    let body = match raw.trim_start().strip_prefix("---") {
        Some(rest) => {
            let (frontmatter, body) = rest
                .split_once("---")
                .ok_or_else(|| format!("{}: unterminated frontmatter", path.display()))?;
            for line in frontmatter.lines() {
                if let Some(value) = line.strip_prefix("description:") {
                    description = Some(value.trim().to_string());
                }
            }
            body
        }
        None => raw.as_str(),
    };

    if name.chars().next().is_some_and(|c| c.is_ascii_digit())
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "{}: template name `{name}` is not a slug",
            path.display()
        ));
    }

    Ok(Template {
        name: name.to_ascii_lowercase(),
        description: description.unwrap_or_default(),
        body: body.to_string(),
    })
}

/// Discovers templates from two roots: the user dir first, the project
/// dir overriding by name. A file that fails to parse is a warning,
/// never a failed launch, same posture as skills.
pub fn discover(user_dir: &Path, project_dir: &Path) -> (Vec<Template>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut discovered: Vec<(bool, Template)> = Vec::new(); // (is_project, template)
    for (is_project, root) in [(false, user_dir), (true, project_dir)] {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue, // absent root: nothing to discover
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_file = path.is_file();
            let extension = path.extension().and_then(|ext| ext.to_str());
            if !(is_file && matches!(extension, Some("md") | Some("txt"))) {
                continue;
            }
            match parse_template(&path) {
                Ok(template) => discovered.push((is_project, template)),
                Err(error) => warnings.push(format!(
                    "template ({}): {error}",
                    if is_project { "project" } else { "user" }
                )),
            }
        }
    }
    discovered.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let mut templates: Vec<Template> = Vec::new();
    for (_, template) in discovered {
        // First-wins: the project's template shadows the user's.
        if templates.iter().any(|kept| kept.name == template.name) {
            continue;
        }
        templates.push(template);
    }
    // A stable, purely alphabetical listing, shadowing decided which
    // body survives, not where it sits.
    templates.sort_by(|a, b| a.name.cmp(&b.name));
    (templates, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_parsing_rejects_paths_and_bare_slashes() {
        assert_eq!(
            parse_invocation("/explain the borrow checker"),
            Some(("explain".into(), "the borrow checker".into()))
        );
        assert_eq!(
            parse_invocation("/compact"),
            Some(("compact".into(), String::new()))
        );
        // Not invocations: sent as normal messages.
        assert_eq!(parse_invocation("/usr/bin/env"), None);
        assert_eq!(parse_invocation("/"), None);
        assert_eq!(parse_invocation("hello /world"), None);
        assert_eq!(parse_invocation(""), None);
        // Case-insensitive names.
        assert_eq!(
            parse_invocation("/Explain x"),
            Some(("explain".into(), "x".into()))
        );
    }

    #[test]
    fn render_substitutes_or_appends_arguments() {
        let placeholder = Template {
            name: "a".into(),
            description: String::new(),
            body: "Fix this: {{args}}".into(),
        };
        assert_eq!(placeholder.render("the lint"), "Fix this: the lint");
        let plain = Template {
            name: "b".into(),
            description: String::new(),
            body: "Review the diff.".into(),
        };
        assert_eq!(plain.render(""), "Review the diff.");
        assert_eq!(
            plain.render("ignore tests"),
            "Review the diff.\n\nignore tests"
        );
    }

    #[test]
    fn discovery_shadows_user_by_project_and_warns_on_bad_files() {
        let user = std::env::temp_dir().join(format!("gertie-tpl-user-{}", std::process::id()));
        let project = std::env::temp_dir().join(format!("gertie-tpl-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();

        std::fs::write(
            user.join("review.md"),
            "---\ndescription: Review a diff.\n---\nReview it.\n",
        )
        .unwrap();
        // The project's version shadows the user's by name.
        std::fs::write(project.join("review.md"), "PROJECT review {{args}}.\n").unwrap();
        std::fs::write(
            user.join("explain.md"),
            "Explain {{args}} in plain language.\n",
        )
        .unwrap();
        // A non-slug stem is a warning, not a fatal.
        std::fs::write(user.join("bad name.md"), "nope\n").unwrap();
        // An ignored extension.
        std::fs::write(user.join("notes.rst"), "nope\n").unwrap();

        let (templates, warnings) = discover(&user, &project);
        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["explain", "review"]);
        let review = lookup(&templates, "review").unwrap();
        assert_eq!(review.body, "PROJECT review {{args}}.\n");
        assert!(lookup(&templates, "review").unwrap().description.is_empty());
        assert_eq!(lookup(&templates, "review").unwrap().name, "review");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad name.md"));

        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&project);
    }
}
