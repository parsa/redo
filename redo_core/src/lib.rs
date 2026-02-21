//! Core library.

pub mod builder;
pub mod cycles;
pub mod deps;
pub mod env;
pub mod action_cache;
pub mod remote_cache;
pub mod remote_exec;
pub mod helpers;
pub mod jobserver;
pub mod locks;
pub mod logs;
pub mod paths;
pub mod state;
pub mod version;

pub type Result<T> = anyhow::Result<T>;
