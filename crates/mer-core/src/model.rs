use crate::{limits, CoreError, Digest, Hasher, SnapshotId};
use serde::{Deserialize, Serialize};

/// What an entry is for. Artifacts are served byte-exact; records and evidence
/// are metadata kept so indices can be regenerated and upstream trust re-checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EntryRole {
    Artifact,
    Record,
    Evidence,
}

impl EntryRole {
    pub fn as_u8(self) -> u8 {
        match self {
            EntryRole::Artifact => 0,
            EntryRole::Record => 1,
            EntryRole::Evidence => 2,
        }
    }
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(EntryRole::Artifact),
            1 => Some(EntryRole::Record),
            2 => Some(EntryRole::Evidence),
            _ => None,
        }
    }
}

/// One member of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRec {
    pub path: String,
    pub role: EntryRole,
    pub digest: Digest,
    pub size: u64,
}

/// One change between two snapshots of a repo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryOp {
    Put(EntryRec),
    Del { path: String },
}

impl EntryOp {
    pub fn path(&self) -> &str {
        match self {
            EntryOp::Put(e) => &e.path,
            EntryOp::Del { path } => path,
        }
    }
}

/// Position of a bundle on a channel. Ordered lexicographically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    pub epoch: u32,
    pub seq: u64,
}

/// Repo-relative paths are attacker-controlled on the vault. Reject anything
/// that could escape a directory or confuse a client.
pub fn validate_path(p: &str) -> Result<(), CoreError> {
    if p.is_empty() {
        return Err(CoreError::BadPath("empty"));
    }
    if p.len() > limits::MAX_PATH_BYTES {
        return Err(CoreError::BadPath("too long"));
    }
    if p.starts_with('/') || p.ends_with('/') {
        return Err(CoreError::BadPath("leading or trailing slash"));
    }
    if p.bytes().any(|b| b == b'\\' || b < 0x20 || b == 0x7f) {
        return Err(CoreError::BadPath("control character or backslash"));
    }
    for seg in p.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(CoreError::BadPath("empty, '.' or '..' segment"));
        }
    }
    Ok(())
}

/// Hash of a complete entry set. `entries` must be sorted by path bytes.
pub fn content_root<'a>(entries: impl IntoIterator<Item = &'a EntryRec>) -> Digest {
    let mut h = Hasher::new();
    h.update(b"mer/content-root/v1\0");
    for e in entries {
        h.update(&[e.role.as_u8()]);
        h.update(&(e.path.len() as u32).to_be_bytes());
        h.update(e.path.as_bytes());
        h.update(&e.digest.0);
        h.update(&e.size.to_be_bytes());
    }
    h.finish()
}

impl SnapshotId {
    /// Root of a snapshot described by its full content.
    pub fn full(content_root: &Digest) -> Self {
        let mut h = Hasher::new();
        h.update(b"mer/snap-full/v1\0");
        h.update(&content_root.0);
        SnapshotId(h.finish().0)
    }
    /// Root of the empty repo: where every chain starts.
    pub fn genesis() -> Self {
        Self::full(&content_root(std::iter::empty()))
    }
    /// Root after applying one ops object on top of `prev`.
    pub fn step(prev: &SnapshotId, ops: &Digest) -> Self {
        let mut h = Hasher::new();
        h.update(b"mer/snap-step/v1\0");
        h.update(&prev.0);
        h.update(&ops.0);
        SnapshotId(h.finish().0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str, body: &[u8]) -> EntryRec {
        EntryRec {
            path: path.into(),
            role: EntryRole::Artifact,
            digest: Digest::of(body),
            size: body.len() as u64,
        }
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            Digest::of(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut h = Hasher::new();
        h.update(b"a");
        h.update(b"bc");
        assert_eq!(h.finish(), Digest::of(b"abc"));
    }

    #[test]
    fn hex_round_trip_and_errors() {
        let d = Digest::of(b"x");
        assert_eq!(Digest::from_hex(&d.to_hex()).unwrap(), d);
        assert_eq!(Digest::from_hex("zz"), Err(CoreError::BadHex));
        assert_eq!(Digest::from_hex("abcd"), Err(CoreError::BadLength { expected: 32, got: 2 }));
    }

    #[test]
    fn content_root_depends_on_every_field() {
        let a = rec("a", b"1");
        let b = rec("b", b"2");
        let base = content_root([&a, &b]);
        assert_eq!(base, content_root([&a, &b]));
        assert_ne!(base, content_root([&a]));
        assert_ne!(base, content_root([&a, &rec("b", b"3")]));
        assert_ne!(base, content_root([&a, &rec("c", b"2")]));
        let mut b2 = b.clone();
        b2.role = EntryRole::Record;
        assert_ne!(base, content_root([&a, &b2]));
        assert_ne!(content_root([&rec("ab", b"1")]), content_root([&rec("a", b"1")]));
    }

    #[test]
    fn snapshot_chain_is_deterministic_and_order_sensitive() {
        let g = SnapshotId::genesis();
        let o1 = Digest::of(b"ops1");
        let o2 = Digest::of(b"ops2");
        let a = SnapshotId::step(&SnapshotId::step(&g, &o1), &o2);
        let b = SnapshotId::step(&SnapshotId::step(&g, &o1), &o2);
        let c = SnapshotId::step(&SnapshotId::step(&g, &o2), &o1);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(SnapshotId::full(&o1).0, o1.0);
    }

    #[test]
    fn path_validation() {
        for ok in ["a", "pool/main/c/curl/curl_8.deb", "@scope/pkg/-/pkg-1.0.0.tgz", "a..b/c"] {
            assert!(validate_path(ok).is_ok(), "{ok}");
        }
        for bad in ["", "/a", "a/", "a//b", "../a", "a/../b", "a/./b", "a\\b", "a\0b", "a\nb"] {
            assert!(validate_path(bad).is_err(), "{bad:?}");
        }
        assert!(validate_path(&"x".repeat(1025)).is_err());
    }
}
