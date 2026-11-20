//! Puppy daemon library: platform paths and configuration.

pub mod config;
pub mod paths;
pub mod store;

pub use config::{Config, ConfigError};
pub use paths::{Paths, SocketName};
pub use store::{PersistedState, RuntimeSettings, Store, StoredDownload, StoredState};
