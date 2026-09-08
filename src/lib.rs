//! TeamClaude library: everything the binary uses, exposed so integration
//! tests can drive the proxy in-process.

pub mod cli;
pub mod codex;
pub mod config;
pub mod manager;
pub mod model;
pub mod oauth;
pub mod prober;
pub mod proxy;
pub mod quota;
pub mod security;
pub mod session;
pub mod status;
pub mod titles;
pub mod tui;
pub mod update;
pub mod upstream;
pub mod warmer;
