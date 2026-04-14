#![allow(dead_code)]

mod config;
mod types;
mod agent_trait;
mod ipc;
mod daemon;
mod opencode;
mod discovery;
mod ssh;
mod tui;

fn main() {
    println!("ocwatch - OpenCode session monitor");
}
