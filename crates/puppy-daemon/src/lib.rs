//! Puppy daemon library: platform paths, configuration, state store, the download manager
//! and the local-socket server.

pub mod budget;
pub mod config;
pub mod lifecycle;
pub mod manager;
pub mod notify;
pub mod paths;
pub mod server;
pub mod store;

pub use config::{Config, ConfigError, NotificationsConfig, TorrentConfig};
pub use manager::{Manager, ManagerError};
pub use paths::{Paths, SocketName};
pub use store::{PersistedState, RuntimeSettings, Store, StoreError, StoredDownload, StoredState};

/// Serializes unit tests that read or change the process environment (paths, config
/// defaults).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
