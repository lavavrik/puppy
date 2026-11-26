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
    /// The files' relative paths (`/`-separated) in the torrent's own order.
    torrent_order: Vec<String>,
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

        let torrent_order = match &created.as_info().info.data.files {
            // A multi-file torrent's `files`, in the order the info dict lists them.
            Some(files) if !single => files
                .iter()
                .map(|f| {
                    let parts: Vec<_> = f
                        .path
                        .iter()
                        .map(|c| String::from_utf8_lossy(c.as_ref()))
                        .collect();
                    parts.join("/")
                })
                .collect(),
            _ => vec![name.clone()],
        };

        Seeder {
            torrent_bytes: created.as_bytes().unwrap(),
            magnet: created.as_magnet().to_string(),
            info_hash_hex: created.info_hash().as_string(),
            name,
            addr,
            dir,
            session,
            files: files.iter().map(|(n, l)| (n.to_string(), *l)).collect(),
            torrent_order,
        }
    }

    /// The files' relative paths (as given to [`Seeder::start`]) in the torrent's order.
    /// `create_torrent` lists a folder in readdir order, which differs between file
    /// systems (APFS, NTFS, ext4), so tests must not assume it is the order given.
    pub fn torrent_order(&self) -> &[String] {
        &self.torrent_order
    }

    /// 0-based index of file `rel` in the torrent.
    pub fn file_index(&self, rel: &str) -> usize {
        self.torrent_order
            .iter()
            .position(|p| p == rel)
            .unwrap_or_else(|| panic!("{rel} is not in the torrent: {:?}", self.torrent_order))
    }

    /// 1-based number of file `rel` in the torrent (as `puppy files` lists it).
    pub fn file_number(&self, rel: &str) -> usize {
        self.file_index(rel) + 1
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

/// Hand-made `.torrent` bytes (no checks; for torrents `create_torrent` won't make): a
/// single file `name` of `len` bytes, or (`files` given) a multi-file torrent `name`
/// with `(path components, len)` files. Piece hashes are zeros.
pub fn craft_torrent(name: &str, len: u64, files: Option<&[(&[&str], u64)]>) -> Vec<u8> {
    fn bstr(out: &mut Vec<u8>, s: &[u8]) {
        out.extend_from_slice(format!("{}:", s.len()).as_bytes());
        out.extend_from_slice(s);
    }
    let total: u64 = files.map_or(len, |f| f.iter().map(|x| x.1).sum());
    let pieces = total.div_ceil(PIECE_LENGTH as u64).max(1) as usize;
    let mut out = b"d4:infod".to_vec();
    match files {
        Some(files) => {
            bstr(&mut out, b"files");
            out.push(b'l');
            for (path, len) in files {
                out.extend_from_slice(format!("d6:lengthi{len}e4:pathl").as_bytes());
                for c in *path {
                    bstr(&mut out, c.as_bytes());
                }
                out.extend_from_slice(b"ee");
            }
            out.push(b'e');
        }
        None => out.extend_from_slice(format!("6:lengthi{len}e").as_bytes()),
    }
    bstr(&mut out, b"name");
    bstr(&mut out, name.as_bytes());
    out.extend_from_slice(format!("12:piece lengthi{PIECE_LENGTH}e").as_bytes());
    bstr(&mut out, b"pieces");
    bstr(&mut out, &vec![0u8; 20 * pieces]);
    out.extend_from_slice(b"ee");
    out
}

/// A raw librqbit leecher (no puppy code) of a torrent whose only peer is `peer`
/// (e.g. puppy's listen address): makes puppy upload, network-free. Its session stops
/// when it is dropped.
pub struct Leecher {
    pub session: Arc<Session>,
    pub handle: Arc<librqbit::ManagedTorrent>,
    /// Holds the downloaded files.
    pub dir: TempDir,
}

impl Leecher {
    /// Starts downloading `torrent_bytes` from `peer`, at most `download_bps` if given.
    pub async fn start(torrent_bytes: &Bytes, peer: SocketAddr, download_bps: Option<u32>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = local_session_options(false, None);
        opts.ratelimits.download_bps = download_bps.and_then(NonZeroU32::new);
        let session = Session::new_with_opts(dir.path().to_owned(), opts)
            .await
            .expect("leecher session starts");
        let handle = session
            .add_torrent(
                AddTorrent::TorrentFileBytes(torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    overwrite: true,
                    disable_trackers: true,
                    initial_peers: Some(vec![peer]),
                    ..Default::default()
                }),
            )
            .await
            .expect("torrent added")
            .into_handle()
            .expect("a managed torrent");
        Leecher {
            session,
            handle,
            dir,
        }
    }

    /// Bytes downloaded and verified so far.
    pub fn progress(&self) -> u64 {
        self.handle.stats().progress_bytes
    }

    pub fn finished(&self) -> bool {
        self.handle.stats().finished
    }
}

impl Drop for Leecher {
    fn drop(&mut self) {
        self.session.cancellation_token().cancel();
    }
}

/// A second, complete copy of the seeder's torrent in its own session that connects to
/// `peer` (e.g. puppy, which then downloads from it: an incoming peer, for a torrent
/// added without peers).
pub async fn serving_copy(seeder: &Seeder, peer: SocketAddr) -> Leecher {
    let dir = tempfile::tempdir().unwrap();
    for (rel, bytes) in seeder.expected_files() {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let session = Session::new_with_opts(dir.path().to_owned(), local_session_options(false, None))
        .await
        .expect("serving session starts");
    let handle = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(seeder.torrent_bytes.clone()),
            Some(AddTorrentOptions {
                overwrite: true,
                disable_trackers: true,
                initial_peers: Some(vec![peer]),
                ..Default::default()
            }),
        )
        .await
        .expect("torrent added")
        .into_handle()
        .expect("a managed torrent");
    handle
        .wait_until_initialized()
        .await
        .expect("copy verified");
    assert!(handle.stats().finished, "the copy is complete");
    Leecher {
        session,
        handle,
        dir,
    }
}
