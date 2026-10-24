pub mod app;
pub mod chat;
pub mod cli;
pub mod config;
pub mod headless;
pub mod tabs;
pub mod worktree;

pub mod bootstrap;

pub fn run() {
    app::run();
}
