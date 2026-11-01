fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match gertie::cli::parse(&arguments) {
        Ok(gertie::cli::Command::Help) => println!("{}", gertie::cli::usage()),
        Ok(gertie::cli::Command::Version) => println!("gertie {}", gertie::cli::VERSION),
        Ok(gertie::cli::Command::Gui { config }) => gertie::app::run_with(config),
        Ok(gertie::cli::Command::Run {
            prompt,
            yes,
            config,
            ephemeral,
        }) => std::process::exit(gertie::headless::run(prompt, yes, config, ephemeral)),
        Ok(gertie::cli::Command::Doctor {
            config,
            clean_worktrees,
            check_updates,
        }) => std::process::exit(gertie::doctor::run(config, clean_worktrees, check_updates)),
        Err(error) => {
            eprintln!("error: {}\n\n{}", error.0, gertie::cli::usage());
            std::process::exit(2);
        }
    }
}
