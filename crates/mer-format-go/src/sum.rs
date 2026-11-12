//! The `h1:` checksums of go.sum and the Go checksum database.
//!
//! An `h1` hash is the SHA-256 of a listing: one line per file, giving the
//! SHA-256 of the file and its name, sorted by name.
use base64::Engine;
use sha2::{Digest as _, Sha256};
use std::io::Read;

/// Total uncompressed size accepted from one archive.
const MAX_UNPACKED: u64 = 4 << 30;

fn h1(mut files: Vec<(String, [u8; 32])>) -> String {
    files.sort();
    let mut listing = Sha256::new();
    for (name, hash) in &files {
        listing.update(format!("{}  {name}\n", hex::encode(hash)));
    }
    format!("h1:{}", base64::engine::general_purpose::STANDARD.encode(listing.finalize()))
}

/// Checksum of a go.mod file (the `/go.mod` line of go.sum).
pub fn h1_gomod(body: &[u8]) -> String {
    h1(vec![("go.mod".to_string(), Sha256::digest(body).into())])
}

/// Checksum of a module archive: over the files inside the zip.
pub fn h1_zip(body: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(body)).map_err(|e| format!("not a zip archive: {e}"))?;
    let mut files = Vec::with_capacity(archive.len());
    let mut total = 0u64;
    let mut buf = vec![0u8; 64 << 10];
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let name = file.name().to_string();
        if name.contains('\n') {
            return Err("a file name in the archive contains a newline".into());
        }
        let mut hash = Sha256::new();
        loop {
            let n = file.read(&mut buf).map_err(|e| format!("{name}: {e}"))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_UNPACKED {
                return Err("the archive unpacks to more than is accepted".into());
            }
            hash.update(&buf[..n]);
        }
        files.push((name, hash.finalize().into()));
    }
    Ok(h1(files))
}
