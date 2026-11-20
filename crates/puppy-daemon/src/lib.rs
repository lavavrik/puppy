//! Puppy daemon library: platform paths and configuration.

pub mod config;
pub mod paths;

pub use config::{Config, ConfigError};
pub use paths::{Paths, SocketName};
