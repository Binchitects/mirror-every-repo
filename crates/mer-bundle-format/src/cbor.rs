use crate::FormatError;
use mer_core::{limits, validate_path, EntryOp, Manifest, SnapshotId, FORMAT_VERSION};
use serde::{de::DeserializeOwned, Serialize};

pub fn to_cbor<T: Serialize>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(value, &mut out).expect("writing CBOR to a Vec cannot fail");
    out
}

/// Decode exactly one CBOR item occupying all of `bytes`.
pub fn from_cbor<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FormatError> {
    let mut rest = bytes;
    let value = ciborium::de::from_reader_with_recursion_limit(&mut rest, 16)
        .map_err(|e| FormatError::Cbor(e.to_string()))?;
    if !rest.is_empty() {
        return Err(FormatError::Cbor("trailing bytes".into()));
    }
    Ok(value)
}

/// An ops object: the changes of one step, sorted by path, one op per path.
pub fn encode_ops(ops: &[EntryOp]) -> Vec<u8> {
    debug_assert!(ops.windows(2).all(|w| w[0].path() < w[1].path()));
    to_cbor(&ops)
}

pub fn decode_ops(bytes: &[u8]) -> Result<Vec<EntryOp>, FormatError> {
    let ops: Vec<EntryOp> = from_cbor(bytes)?;
    for op in &ops {
        validate_path(op.path()).map_err(|e| FormatError::Integrity(format!("ops: {e}")))?;
    }
    if !ops.windows(2).all(|w| w[0].path() < w[1].path()) {
        return Err(FormatError::Integrity("ops are not sorted by unique path".into()));
    }
    Ok(ops)
}

fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Decode a manifest and check everything that can be checked without other data.
pub fn decode_manifest(bytes: &[u8]) -> Result<Manifest, FormatError> {
    if bytes.len() as u64 > limits::MAX_MANIFEST_BYTES {
        return Err(FormatError::Limit("manifest size".into()));
    }
    let m: Manifest = from_cbor(bytes)?;
    if m.v != FORMAT_VERSION {
        return Err(FormatError::Unsupported(format!("manifest version {}", m.v)));
    }
    if m.repos.len() > limits::MAX_REPOS {
        return Err(FormatError::Limit("repo count".into()));
    }
    let mut names = std::collections::BTreeSet::new();
    for repo in &m.repos {
        if !valid_repo_name(&repo.repo) || !names.insert(repo.repo.as_str()) {
            return Err(FormatError::Integrity(format!("repo name {:?}", repo.repo)));
        }
        if repo.steps.is_empty() || repo.steps.len() > limits::MAX_STEPS_PER_REPO {
            return Err(FormatError::Limit(format!("step count of {}", repo.repo)));
        }
        let mut prev: Option<&mer_core::Step> = None;
        for step in &repo.steps {
            if step.seq == 0 || step.seq > m.seq || step.prev_seq >= step.seq {
                return Err(FormatError::Integrity(format!("step seq of {}", repo.repo)));
            }
            if step.root != SnapshotId::step(&step.prev_root, &step.ops) {
                return Err(FormatError::Integrity(format!("step root of {}", repo.repo)));
            }
            if let Some(p) = prev {
                if step.prev_seq != p.seq || step.prev_root != p.root {
                    return Err(FormatError::Integrity(format!("step chain of {} is broken", repo.repo)));
                }
            }
            prev = Some(step);
        }
    }
    Ok(m)
}
