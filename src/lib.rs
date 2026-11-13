//! Corrall library: everything the binary uses, exposed so integration
//! tests can drive the proxy in-process.

pub mod attach;
pub mod claude_setup;
pub mod cli;
pub mod codex;
pub mod config;
pub mod gnome;
pub mod manager;
pub mod migrate;
pub mod model;
pub mod oauth;
pub mod pool_match;
pub mod pools;
pub mod prober;
pub mod proxy;
pub mod quota;
pub mod security;
pub mod service;
pub mod session;
pub mod session_table;
pub mod status;
pub mod titles;
pub mod tui;
pub mod tui_screens;
pub mod update;
pub mod upstream;
pub mod warmer;
