pub mod app;
pub mod chat;
pub mod cli;
pub mod config;
pub mod headless;

pub mod bootstrap;

pub fn run() {
    app::run();
}
