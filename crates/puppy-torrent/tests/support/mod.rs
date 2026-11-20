//! Local, network-free BitTorrent test harness.
//!
//! [`Seeder`] writes deterministic files into a temp dir, makes a torrent of them with
//! `librqbit::create_torrent` (small pieces, no trackers) and seeds it from a librqbit
//! session listening on `127.0.0.1:0` with DHT, trackers, LSD and UPnP off. Leechers reach
//! it only through `initial_peers = [seeder.addr]`.

#![allow(dead_code)] // each test binary uses a different part of the harness

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
};
use tempfile::TempDir;

/// Piece length of the generated torrents: small, so tiny files still have several pieces.
pub const PIECE_LENGTH: u32 = 16 * 1024;

/// Name of the top folder of a multi-file torrent.
pub const MULTI_NAME: &str = "bundle";

/// Deterministic content for file `name` of `len` bytes (differs per name and offset).
pub fn content(name: &str, len: usize) -> Vec<u8> {
    // FNV-1a of the name seeds an xorshift64 stream.
    let mut x: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        x = (x ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    x |= 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Session options for a test session: no DHT, trackers, LSD, UPnP or persistence.
/// `listen`: accept incoming peers on `127.0.0.1:0`.
pub fn local_session_options(listen: bool, upload_bps: Option<u32>) -> SessionOptions {
    SessionOptions {
        dht: None,
        disable_trackers: true,
        disable_local_service_discovery: true,
        persistence: None,
        fastresume: false,
        listen: listen.then(|| ListenerOptions {
            listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
            enable_upnp_port_forwarding: false,
            ..Default::default()
        }),
        ratelimits: LimitsConfig {
            upload_bps: upload_bps.and_then(NonZeroU32::new),
            download_bps: None,
        },
        ..Default::default()
    }
}

pub struct Seeder {
    /// The `.torrent` file.
    pub torrent_bytes: Bytes,
    /// `magnet:?xt=urn:btih:<hash>` (no trackers, no `dn`).
    pub magnet: String,
    /// Lowercase hex info hash.
    pub info_hash_hex: String,
    /// Torrent name: the single file's name, or [`MULTI_NAME`] for several files.
    pub name: String,
    /// Where the seeding session accepts peers (127.0.0.1).
    pub addr: SocketAddr,
    /// Holds the seeded files (under `seed/`).
    pub dir: TempDir,
    pub session: Arc<Session>,
    files: Vec<(String, usize)>,
}

impl Seeder {
    /// Seeds `files` (`(relative path, length)`). One file without `/` makes a
    /// single-file torrent named after it; otherwise a multi-file torrent named
    /// [`MULTI_NAME`] containing the files at their relative paths.
    pub async fn start(files: &[(&str, usize)]) -> Seeder {
        Self::start_inner(files, None).await
    }

    /// Like [`Seeder::start`], with the seeder's upload throttled to `upload_bps`.
    pub async fn start_with_limit(files: &[(&str, usize)], upload_bps: u32) -> Seeder {
        Self::start_inner(files, Some(upload_bps)).await
    }

    async fn start_inner(files: &[(&str, usize)], upload_bps: Option<u32>) -> Seeder {
        assert!(!files.is_empty(), "a torrent needs at least one file");
        let dir = tempfile::tempdir().unwrap();
        let seed_root = dir.path().join("seed");
        let single = files.len() == 1 && !files[0].0.contains('/');
        let (name, torrent_path) = if single {
            (files[0].0.to_owned(), seed_root.join(files[0].0))
        } else {
            (MULTI_NAME.to_owned(), seed_root.join(MULTI_NAME))
        };
        for (rel, len) in files {
            let path = if single {
                torrent_path.clone()
            } else {
                torrent_path.join(rel)
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content(rel, *len)).unwrap();
        }

        let session =
            Session::new_with_opts(seed_root.clone(), local_session_options(true, upload_bps))
                .await
                .expect("seeding session starts");
        let addr = session.listen_addr().expect("seeder listens");
        let (created, handle) = session
            .create_and_serve_torrent(
                &torrent_path,
                CreateTorrentOptions {
                    name: None,
                    trackers: Vec::new(),
                    piece_length: Some(PIECE_LENGTH),
                },
            )
            .await
            .expect("torrent created and seeded");
        handle
            .wait_until_initialized()
            .await
            .expect("seeder verified its files");
        assert!(handle.stats().finished, "seeder has the whole torrent");

        Seeder {
            torrent_bytes: created.as_bytes().unwrap(),
            magnet: created.as_magnet().to_string(),
            info_hash_hex: created.info_hash().as_string(),
            name,
            addr,
            dir,
            session,
            files: files.iter().map(|(n, l)| (n.to_string(), *l)).collect(),
        }
    }

    /// `(relative path, expected bytes)` of each file, relative to the folder a leecher
    /// downloads the torrent into (`<name>` or `<MULTI_NAME>/<path>`).
    pub fn expected_files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let single = self.files.len() == 1 && !self.files[0].0.contains('/');
        self.files
            .iter()
            .map(|(rel, len)| {
                let path = if single {
                    PathBuf::from(rel)
                } else {
                    Path::new(MULTI_NAME).join(rel)
                };
                (path, content(rel, *len))
            })
            .collect()
    }

    /// Asserts every file of the torrent is in `output_dir` with the right bytes.
    pub fn assert_downloaded_into(&self, output_dir: &Path) {
        for (rel, expected) in self.expected_files() {
            let got = std::fs::read(output_dir.join(&rel))
                .unwrap_or_else(|e| panic!("reading {}: {e}", rel.display()));
            assert!(
                got == expected,
                "{} differs from the seeded file",
                rel.display()
            );
        }
    }
}

impl Drop for Seeder {
    fn drop(&mut self) {
        // Stops the session's tasks (listener, peers) even if something still holds it.
        self.session.cancellation_token().cancel();
    }
}

/// Downloads the seeder's torrent with a raw librqbit session (no puppy code) into
/// `output_dir`, peers only from `initial_peers = [seeder.addr]`.
pub async fn leech_raw(seeder: &Seeder, output_dir: &Path) {
    let session = Session::new_with_opts(output_dir.to_owned(), local_session_options(false, None))
        .await
        .expect("leecher session starts");
    let handle = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(seeder.torrent_bytes.clone()),
            Some(AddTorrentOptions {
                // No explicit output_folder: librqbit then puts a multi-file torrent in
                // `<session folder>/<name>/` (an explicit one gets the files directly).
                overwrite: true,
                disable_trackers: true,
                initial_peers: Some(vec![seeder.addr]),
                ..Default::default()
            }),
        )
        .await
        .expect("torrent added")
        .into_handle()
        .expect("a managed torrent");
    handle
        .wait_until_completed()
        .await
        .expect("download completes");
    session.cancellation_token().cancel();
}
