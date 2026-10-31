//! Argument parsing, hand-rolled: the surface is four flags and one
//! subcommand — a dependency would out-weigh the grammar.

use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
gertie — a fast, native coding-agent harness

USAGE:
    gertie [OPTIONS]                 launch the desktop app
    gertie run [OPTIONS] [PROMPT]    run one turn headlessly; the answer
                                     goes to stdout, activity to stderr,
                                     and the session is saved
    gertie doctor                    check the setup: config, endpoint,
                                     key resolution, MCP, sessions

OPTIONS:
    --config <PATH>    config file (default: the standard config dir)
    -y, --yes          run: auto-approve every mutating tool call
    -h, --help         print this help
    -V, --version      print version

run approval: mutating calls print the tool and arguments on stderr and
wait — Enter/y approves, anything else denies. Without a terminal, runs
deny mutations unless --yes is given. Reads are always auto-approved.";

/// What the command line asked for.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Launch the desktop app, optionally with a config override.
    Gui { config: Option<PathBuf> },
    /// One headless turn. `prompt` None means: read it from stdin.
    Run {
        prompt: Option<String>,
        yes: bool,
        config: Option<PathBuf>,
    },
    /// The setup diagnostic. `clean_worktrees` applies the worktree
    /// cleanup policy (otherwise the same sweep is report-only).
    Doctor {
        config: Option<PathBuf>,
        clean_worktrees: bool,
    },
    /// Print usage and exit 0.
    Help,
    /// Print version and exit 0.
    Version,
}

/// A grammar error: message plus the usage text.
#[derive(Debug)]
pub struct UsageError(pub String);

pub fn parse(arguments: &[String]) -> Result<Command, UsageError> {
    let mut config: Option<PathBuf> = None;
    let mut yes = false;
    let mut positional: Vec<String> = Vec::new();
    let mut run = false;
    let mut doctor = false;
    let mut clean_worktrees = false;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        match argument {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-y" | "--yes" => yes = true,
            "--clean-worktrees" if !run => clean_worktrees = true,
            "doctor" if positional.is_empty() && !run && !doctor => doctor = true,
            "--config" => {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return Err(UsageError("--config needs a path".into()));
                };
                config = Some(PathBuf::from(value));
            }
            _ if argument.starts_with("--config=") => {
                let value = argument.strip_prefix("--config=").unwrap();
                if value.is_empty() {
                    return Err(UsageError("--config needs a path".into()));
                }
                config = Some(PathBuf::from(value));
            }
            "run" if positional.is_empty() && !run => run = true,
            _ if argument.starts_with('-') && !argument.is_empty() => {
                return Err(UsageError(format!("unknown option: {argument}")));
            }
            _ => positional.push(argument.to_string()),
        }
        index += 1;
    }

    if doctor {
        if !positional.is_empty() {
            return Err(UsageError(format!(
                "unexpected argument: {} (doctor takes no prompt)",
                positional[0]
            )));
        }
        return Ok(Command::Doctor {
            config,
            clean_worktrees,
        });
    }

    if clean_worktrees {
        return Err(UsageError(
            "--clean-worktrees belongs to doctor (the only command that cleans)".into(),
        ));
    }

    if !run {
        // Positional words without `run` are a mistake — the GUI takes
        // no prompt. Say so rather than silently ignoring them.
        if !positional.is_empty() {
            return Err(UsageError(format!(
                "unexpected argument: {} (did you mean `gertie run`?)",
                positional[0]
            )));
        }
        return Ok(Command::Gui { config });
    }

    let prompt = if positional.is_empty() {
        None
    } else {
        Some(positional.join(" "))
    };
    Ok(Command::Run {
        prompt,
        yes,
        config,
    })
}

/// Usage text (also the `--help` body).
pub fn usage() -> &'static str {
    USAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_arguments(arguments: &[&str]) -> Result<Command, UsageError> {
        parse(&arguments.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn bare_arguments_launch_the_gui() {
        assert_eq!(parse_arguments(&[]).unwrap(), Command::Gui { config: None });
    }

    #[test]
    fn run_takes_a_prompt_and_flags_in_any_order() {
        let command = parse_arguments(&["run", "--yes", "fix", "the", "tests"]).unwrap();
        assert_eq!(
            command,
            Command::Run {
                prompt: Some("fix the tests".into()),
                yes: true,
                config: None,
            }
        );
    }

    #[test]
    fn run_without_a_prompt_reads_stdin() {
        let command = parse_arguments(&["run", "-y"]).unwrap();
        assert!(matches!(
            command,
            Command::Run {
                prompt: None,
                yes: true,
                ..
            }
        ));
    }

    #[test]
    fn config_flag_both_spellings() {
        for spelling in [
            vec!["--config", "/tmp/x.toml"],
            vec!["--config=/tmp/x.toml"],
        ] {
            let command = parse_arguments(&spelling).unwrap();
            assert_eq!(
                command,
                Command::Gui {
                    config: Some("/tmp/x.toml".into())
                }
            );
        }
    }

    #[test]
    fn help_and_version_win_over_everything() {
        assert_eq!(parse_arguments(&["run", "--help"]).unwrap(), Command::Help);
        assert_eq!(parse_arguments(&["-V"]).unwrap(), Command::Version);
    }

    #[test]
    fn unknown_options_are_usage_errors() {
        assert!(parse_arguments(&["--wat"]).is_err());
        assert!(parse_arguments(&["--config"]).is_err());
        assert!(parse_arguments(&["--config="]).is_err());
    }

    #[test]
    fn doctor_takes_config_overrides() {
        let command = parse_arguments(&["doctor"]).unwrap();
        assert_eq!(
            command,
            Command::Doctor {
                config: None,
                clean_worktrees: false
            }
        );
        let command = parse_arguments(&["doctor", "--config", "/tmp/x.toml"]).unwrap();
        assert_eq!(
            command,
            Command::Doctor {
                config: Some("/tmp/x.toml".into()),
                clean_worktrees: false
            }
        );
        let command = parse_arguments(&["doctor", "--clean-worktrees"]).unwrap();
        assert_eq!(
            command,
            Command::Doctor {
                config: None,
                clean_worktrees: true
            }
        );
    }

    #[test]
    fn positionals_need_the_run_subcommand() {
        let error = parse_arguments(&["hello"]).unwrap_err();
        assert!(error.0.contains("did you mean"), "{}", error.0);
    }
}
