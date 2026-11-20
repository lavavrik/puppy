//! BitTorrent engine for puppy: librqbit inside, puppy's `Engine` trait outside. No
//! librqbit type appears in this crate's public API.

mod engine;
mod map;
mod poll;
mod selection;
mod source;

pub use engine::{TorrentEngine, TorrentEngineOptions};
pub use selection::resolve;
