pub mod app;
pub mod chat;
pub mod cli;
pub mod config;
pub mod diff;
pub mod doctor;
pub mod eval;
pub mod eval_nav;
pub mod headless;
pub mod ink;
pub mod keymap;
pub mod markdown;
pub mod palette;
pub mod settings;
pub mod settings_edit;
pub mod tabs;
pub mod tools_ui;
pub mod worktree;

pub mod bootstrap;

pub fn run() {
    app::run();
}
