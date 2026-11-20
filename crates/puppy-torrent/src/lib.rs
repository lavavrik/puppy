//! BitTorrent engine for puppy: librqbit inside, puppy's `Engine` trait outside. No
//! librqbit type appears in this crate's public API.

mod engine;

pub use engine::{TorrentEngine, TorrentEngineOptions};
