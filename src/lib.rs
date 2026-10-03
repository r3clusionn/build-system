//! `kiln`: a build system with content-hashed incremental builds. Tasks in a TOML file expand
//! into steps (one command each), steps form a graph by the files they read and write, and a
//! step runs only when the hash of its command or anything it reads has changed.

pub mod config;
pub mod depfile;
pub mod exec;
pub mod glob;
pub mod graph;
pub mod state;
