//! The `h1:` hashes Go records in `go.sum`, computed over module zips and
//! `go.mod` files. The zip comes from upstream and is untrusted, so it is
//! read with strict limits.
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::Read;

/// Go refuses module zips larger than this (`modzip.MaxZipFile`), and the
/// unpacked tree has the same limit.
pub const MAX_ZIP: u64 = 500 << 20;
const MAX_FILES: usize = 100_000;

/// `h1:` of a set of (name, content hash) pairs: Go's `dirhash.Hash1`.
fn hash1(mut files: Vec<(String, [u8; 32])>) -> String {
    files.sort();
    let mut summary = Sha256::new();
    for (name, h) in &files {
        summary.update(format!("{}  {name}\n", hex(h)));
    }
    format!("h1:{}", base64::engine::general_purpose::STANDARD.encode(summary.finalize()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `h1:` of a `go.mod` file, as in the `/go.mod` lines of `go.sum`.
pub fn gomod_hash(body: &[u8]) -> String {
    hash1(vec![("go.mod".into(), Sha256::digest(body).into())])
}

fn u16_at(b: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize)
}

fn u32_at(b: &[u8], i: usize) -> Option<u64> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as u64)
}

/// `h1:` of a module zip. Every file must sit under `<module>@<version>/`.
pub fn zip_hash(zip: &[u8], prefix: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("module zip: {why}"));
    if zip.len() as u64 > MAX_ZIP {
        return bad("too large");
    }
    // End of central directory: the last 22 bytes plus a comment of at most 64 KiB.
    let from = zip.len().saturating_sub(22 + 65_535);
    let Some(eocd) = (from..zip.len().saturating_sub(21)).rev().find(|&i| zip[i..i + 4] == [0x50, 0x4b, 0x05, 0x06]) else {
        return bad("no central directory");
    };
    let count = u16_at(zip, eocd + 10).ok_or("truncated")?;
    let mut at = u32_at(zip, eocd + 16).ok_or("truncated")? as usize;
    if count == 0xffff || at == 0xffff_ffff {
        return bad("zip64 is not used for module zips");
    }
    if count > MAX_FILES {
        return bad("too many files");
    }
    let mut files = Vec::with_capacity(count);
    let mut seen = std::collections::BTreeSet::new();
    let mut total: u64 = 0;
    for _ in 0..count {
        if zip.get(at..at + 4) != Some(&[0x50, 0x4b, 0x01, 0x02][..]) {
            return bad("broken central directory");
        }
        let method = u16_at(zip, at + 10).ok_or("truncated")?;
        let csize = u32_at(zip, at + 20).ok_or("truncated")?;
        let usize_ = u32_at(zip, at + 24).ok_or("truncated")?;
        let (nlen, elen, clen) = (u16_at(zip, at + 28).ok_or("truncated")?, u16_at(zip, at + 30).ok_or("truncated")?, u16_at(zip, at + 32).ok_or("truncated")?);
        let local = u32_at(zip, at + 42).ok_or("truncated")? as usize;
        let name = std::str::from_utf8(zip.get(at + 46..at + 46 + nlen).ok_or("truncated")?).map_err(|_| "file name is not UTF-8")?.to_string();
        at += 46 + nlen + elen + clen;
        let rest = name.strip_prefix(prefix).and_then(|r| r.strip_prefix('/'));
        if rest.is_none_or(|r| r.is_empty() || r.ends_with('/') || r.split('/').any(|e| e.is_empty() || e == "." || e == "..")) || name.contains(['\\', '\n']) {
            return bad(&format!("file {name:?} is outside {prefix}/"));
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            return bad(&format!("file {name:?} appears twice"));
        }
        total += usize_;
        if total > MAX_ZIP {
            return bad("unpacks to more than 500 MiB");
        }
        // Local header: its name and extra field lengths may differ from the central copy.
        if zip.get(local..local + 4) != Some(&[0x50, 0x4b, 0x03, 0x04][..]) {
            return bad("broken local header");
        }
        let start = local + 30 + u16_at(zip, local + 26).ok_or("truncated")? + u16_at(zip, local + 28).ok_or("truncated")?;
        let data = zip.get(start..start + csize as usize).ok_or("truncated file data")?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 64 << 10];
        let mut got: u64 = 0;
        let mut reader: Box<dyn Read> = match method {
            0 => Box::new(data),
            8 => Box::new(flate2::read::DeflateDecoder::new(data)),
            _ => return bad(&format!("file {name:?} uses compression method {method}")),
        };
        loop {
            let n = reader.read(&mut buf).map_err(|e| format!("module zip: {name}: {e}"))?;
            if n == 0 {
                break;
            }
            got += n as u64;
            if got > usize_ {
                return bad(&format!("file {name:?} is larger than its header says"));
            }
            h.update(&buf[..n]);
        }
        if got != usize_ {
            return bad(&format!("file {name:?} is shorter than its header says"));
        }
        files.push((name, h.finalize().into()));
    }
    Ok(hash1(files))
}
