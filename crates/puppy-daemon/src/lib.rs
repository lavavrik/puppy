//! Puppy daemon library: platform paths, configuration, state store and the download manager.

pub mod budget;
pub mod config;
pub mod manager;
pub mod paths;
pub mod store;

pub use config::{Config, ConfigError};
pub use manager::{Manager, ManagerError};
pub use paths::{Paths, SocketName};
pub use store::{PersistedState, RuntimeSettings, Store, StoreError, StoredDownload, StoredState};
