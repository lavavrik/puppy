//! Whole-file checksums: the `ALGO=HEX` spec (`sha-256=…`, as aria2 and Metalink write
//! it) and hashing a finished file.
//!
//! A download carries at most one [`Checksum`] (`AddRequest::checksum`), checked once the
//! file is complete. Sources that know several hashes for a file (a Metalink) pick the
//! strongest ([`HashAlgo::strength`]).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A hash algorithm. On the wire and in the state file its name is the IANA / Metalink
/// one (`sha-256`, `md5`); [`HashAlgo::parse`] also takes `sha256`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HashAlgo {
    #[serde(rename = "md5")]
    Md5,
    #[serde(rename = "sha-1")]
    Sha1,
    #[serde(rename = "sha-224")]
    Sha224,
    #[serde(rename = "sha-256")]
    Sha256,
    #[serde(rename = "sha-384")]
    Sha384,
    #[serde(rename = "sha-512")]
    Sha512,
}

impl HashAlgo {
    pub const ALL: [HashAlgo; 6] = [
        HashAlgo::Md5,
        HashAlgo::Sha1,
        HashAlgo::Sha224,
        HashAlgo::Sha256,
        HashAlgo::Sha384,
        HashAlgo::Sha512,
    ];

    /// The canonical name (`sha-256`).
    pub fn name(self) -> &'static str {
        match self {
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha-1",
            HashAlgo::Sha224 => "sha-224",
            HashAlgo::Sha256 => "sha-256",
            HashAlgo::Sha384 => "sha-384",
            HashAlgo::Sha512 => "sha-512",
        }
    }

    /// Parses a name, ignoring case and the dash: `sha-256`, `SHA256`, `md5`, `md-5`.
    pub fn parse(name: &str) -> Option<HashAlgo> {
        let n: String = name
            .chars()
            .filter(|c| *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        HashAlgo::ALL
            .into_iter()
            .find(|a| a.name().replace('-', "") == n)
    }

    /// Digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            HashAlgo::Md5 => 16,
            HashAlgo::Sha1 => 20,
            HashAlgo::Sha224 => 28,
            HashAlgo::Sha256 => 32,
            HashAlgo::Sha384 => 48,
            HashAlgo::Sha512 => 64,
        }
    }

    /// Orders algorithms by strength, for picking one of several hashes (higher is
    /// stronger).
    pub fn strength(self) -> u8 {
        match self {
            HashAlgo::Md5 => 0,
            HashAlgo::Sha1 => 1,
            HashAlgo::Sha224 => 2,
            HashAlgo::Sha256 => 3,
            HashAlgo::Sha384 => 4,
            HashAlgo::Sha512 => 5,
        }
    }

    fn hasher(self) -> Box<dyn sha2::digest::DynDigest + Send> {
        match self {
            HashAlgo::Md5 => Box::new(md5::Md5::default()),
            HashAlgo::Sha1 => Box::new(sha1::Sha1::default()),
            HashAlgo::Sha224 => Box::new(sha2::Sha224::default()),
            HashAlgo::Sha256 => Box::new(sha2::Sha256::default()),
            HashAlgo::Sha384 => Box::new(sha2::Sha384::default()),
            HashAlgo::Sha512 => Box::new(sha2::Sha512::default()),
        }
    }
}

impl fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The expected digest of a download's file. `hex` is lowercase and as long as `algo`'s
/// digest (checked on construction and when deserialized).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawChecksum")]
pub struct Checksum {
    pub algo: HashAlgo,
    pub hex: String,
}

#[derive(Deserialize)]
struct RawChecksum {
    algo: HashAlgo,
    hex: String,
}

impl TryFrom<RawChecksum> for Checksum {
    type Error = ChecksumError;
    fn try_from(r: RawChecksum) -> Result<Self, Self::Error> {
        Checksum::new(r.algo, &r.hex)
    }
}

/// Why a checksum spec was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChecksumError {
    #[error("expected ALGO=HEX, e.g. sha-256=<64 hex digits>")]
    Syntax,
    #[error("unknown algorithm {0:?} (supported: md5, sha-1, sha-224, sha-256, sha-384, sha-512)")]
    UnknownAlgo(String),
    #[error("a {algo} digest is {expected} hex digits, got {got}")]
    Length {
        algo: HashAlgo,
        expected: usize,
        got: usize,
    },
    #[error("{0:?} is not a hex digit")]
    NotHex(char),
}

impl Checksum {
    /// Validates `hex` (any case) for `algo`.
    pub fn new(algo: HashAlgo, hex: &str) -> Result<Checksum, ChecksumError> {
        if let Some(c) = hex.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(ChecksumError::NotHex(c));
        }
        let expected = algo.digest_len() * 2;
        if hex.len() != expected {
            return Err(ChecksumError::Length {
                algo,
                expected,
                got: hex.len(),
            });
        }
        Ok(Checksum {
            algo,
            hex: hex.to_ascii_lowercase(),
        })
    }

    /// The reason a download fails with when its file hashes to `actual`.
    pub fn mismatch(&self, actual: &str) -> String {
        format!(
            "{CHECKSUM_MISMATCH} ({}): expected {}, got {actual}",
            self.algo, self.hex
        )
    }
}

/// How a `Failed` reason for a checksum mismatch starts (see [`Checksum::mismatch`]).
pub const CHECKSUM_MISMATCH: &str = "checksum mismatch";

/// Whether a `Failed` reason is a checksum mismatch.
pub fn is_mismatch(reason: &str) -> bool {
    reason.starts_with(CHECKSUM_MISMATCH)
}

impl std::str::FromStr for Checksum {
    type Err = ChecksumError;

    /// `ALGO=HEX`, e.g. `sha-256=9f86…` or `SHA256=9F86…`; spaces around either part are
    /// ignored.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (algo, hex) = s.split_once('=').ok_or(ChecksumError::Syntax)?;
        let (algo, hex) = (algo.trim(), hex.trim());
        if algo.is_empty() || hex.is_empty() {
            return Err(ChecksumError::Syntax);
        }
        let algo = HashAlgo::parse(algo).ok_or_else(|| ChecksumError::UnknownAlgo(algo.into()))?;
        Checksum::new(algo, hex)
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.algo, self.hex)
    }
}

const CHUNK: usize = 1 << 20;

/// Hashes the file at `path` (blocking: run it off the async runtime), adding the bytes
/// read so far to `progress`. Stops early with `Ok(None)` once `stop` is set. Returns
/// the lowercase hex digest.
pub fn hash_file(
    path: &Path,
    algo: HashAlgo,
    progress: &AtomicU64,
    stop: &AtomicBool,
) -> io::Result<Option<String>> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = algo.hasher();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(Some(to_hex(&hasher.finalize())))
}

fn to_hex(bytes: &[u8]) -> String {
    use fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn parses_specs() {
        let c: Checksum = format!("sha-256={ABC_SHA256}").parse().unwrap();
        assert_eq!(c.algo, HashAlgo::Sha256);
        assert_eq!(c.hex, ABC_SHA256);
        let upper: Checksum = format!("SHA256={}", ABC_SHA256.to_uppercase())
            .parse()
            .unwrap();
        assert_eq!(upper, c, "case-insensitive algorithm and hex");
        assert_eq!(c.to_string(), format!("sha-256={ABC_SHA256}"));
        for (name, algo) in [
            ("md5", HashAlgo::Md5),
            ("MD-5", HashAlgo::Md5),
            ("sha1", HashAlgo::Sha1),
            ("sha-1", HashAlgo::Sha1),
            ("sha-224", HashAlgo::Sha224),
            ("sha384", HashAlgo::Sha384),
            ("Sha-512", HashAlgo::Sha512),
        ] {
            assert_eq!(HashAlgo::parse(name), Some(algo), "{name}");
        }
    }

    #[test]
    fn refuses_bad_specs() {
        let e = |s: &str| s.parse::<Checksum>().unwrap_err();
        assert_eq!(e(ABC_SHA256), ChecksumError::Syntax);
        assert_eq!(e("sha-256="), ChecksumError::Syntax);
        assert_eq!(e(&format!("={ABC_SHA256}")), ChecksumError::Syntax);
        assert_eq!(e("crc32=abcd"), ChecksumError::UnknownAlgo("crc32".into()));
        assert_eq!(
            e("sha-256=abcd"),
            ChecksumError::Length {
                algo: HashAlgo::Sha256,
                expected: 64,
                got: 4
            }
        );
        assert_eq!(
            e(&format!("sha-256={}", "g".repeat(64))),
            ChecksumError::NotHex('g')
        );
        assert_eq!(
            e("md5=abcd").to_string(),
            "a md5 digest is 32 hex digits, got 4"
        );
    }

    #[test]
    fn serde_form_and_validation() {
        let c = Checksum::new(HashAlgo::Sha1, &"A".repeat(40)).unwrap();
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"algo": "sha-1", "hex": "a".repeat(40)})
        );
        assert_eq!(serde_json::from_value::<Checksum>(v).unwrap(), c);
        let bad = serde_json::json!({"algo": "sha-1", "hex": "abc"});
        assert!(serde_json::from_value::<Checksum>(bad).is_err());
        let unknown = serde_json::json!({"algo": "sha3-256", "hex": "a".repeat(64)});
        assert!(serde_json::from_value::<Checksum>(unknown).is_err());
    }

    #[test]
    fn hashes_files_with_every_algorithm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc");
        std::fs::write(&path, b"abc").unwrap();
        let expected = [
            (HashAlgo::Md5, "900150983cd24fb0d6963f7d28e17f72"),
            (HashAlgo::Sha1, "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                HashAlgo::Sha224,
                "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
            ),
            (HashAlgo::Sha256, ABC_SHA256),
            (
                HashAlgo::Sha384,
                "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
                 8086072ba1e7cc2358baeca134c825a7",
            ),
            (
                HashAlgo::Sha512,
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                 2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            ),
        ];
        for (algo, hex) in expected {
            let progress = AtomicU64::new(0);
            let got = hash_file(&path, algo, &progress, &AtomicBool::new(false)).unwrap();
            assert_eq!(got.as_deref(), Some(hex), "{algo}");
            assert_eq!(progress.load(Ordering::Relaxed), 3);
            assert!(Checksum::new(algo, hex).is_ok(), "{algo} length");
        }
        let stopped = hash_file(
            &path,
            HashAlgo::Sha256,
            &AtomicU64::new(0),
            &AtomicBool::new(true),
        )
        .unwrap();
        assert_eq!(stopped, None);
    }

    #[test]
    fn mismatch_reason_names_everything() {
        let c = Checksum::new(HashAlgo::Sha256, ABC_SHA256).unwrap();
        let r = c.mismatch("00");
        assert_eq!(
            r,
            format!("checksum mismatch (sha-256): expected {ABC_SHA256}, got 00")
        );
        assert!(is_mismatch(&r));
        assert!(!is_mismatch("HTTP 404"));
    }
}
