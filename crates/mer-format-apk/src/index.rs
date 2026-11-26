//! Alpine repository indexes and packages, checked from the signature down.
//!
//! `APKINDEX.tar.gz` is two gzip members: a tar holding one signature file
//! (`.SIGN.RSA.<key name>`, or `RSA256`/`RSA512`), then a tar holding
//! `DESCRIPTION` and `APKINDEX`. The signature is RSA (PKCS #1 v1.5) over the
//! raw bytes of the second member. Each index entry states, in `C:`, the
//! SHA-1 of a package's control member (`Q1` + base64), and the control
//! member's `.PKGINFO` states, as `datahash`, the SHA-256 of the data member
//! that follows it. So every byte of a package is tied to the signed index.
//! Nothing is parsed before the bytes it comes from are checked.
use base64::Engine;
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::io::Read;

const MAX_INDEX_TEXT: u64 = 256 << 20;
const MAX_CONTROL: u64 = 16 << 20;
const MAX_SIGNATURE: u64 = 1 << 20;

/// One gzip member: where its raw bytes are, and what they unpack to.
struct Member {
    start: usize,
    end: usize,
    plain: Vec<u8>,
}

/// Split off the gzip member at `start`, unpacking at most `limit` bytes.
fn member(data: &[u8], start: usize, limit: u64) -> Result<Member, String> {
    if data.get(start..start + 2) != Some(&[0x1f, 0x8b]) {
        return Err("not gzip where a gzip member should start".into());
    }
    // One member only: bufread::GzDecoder stops at its end, and the slice
    // it reads from is left just after it.
    let mut dec = flate2::bufread::GzDecoder::new(&data[start..]);
    let mut plain = Vec::new();
    (&mut dec).take(limit + 1).read_to_end(&mut plain).map_err(|e| format!("bad gzip data: {e}"))?;
    if plain.len() as u64 > limit {
        return Err("unpacks to more than is allowed".into());
    }
    let rest = dec.into_inner();
    Ok(Member { start, end: data.len() - rest.len(), plain })
}

/// The files of a tar archive (regular files only), by name. Archives cut
/// without their end blocks, as Alpine's are, are read too.
pub(crate) fn tar_files(tar: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut long_name: Option<String> = None;
    while pos + 512 <= tar.len() {
        let h = &tar[pos..pos + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let field = |r: std::ops::Range<usize>| {
            let f = &h[r];
            String::from_utf8_lossy(&f[..f.iter().position(|&b| b == 0).unwrap_or(f.len())]).into_owned()
        };
        let size_text = field(124..136);
        let size = usize::from_str_radix(size_text.trim(), 8).map_err(|_| format!("tar: bad size {size_text:?}"))?;
        let body_start = pos + 512;
        let body_end = body_start.checked_add(size).filter(|&e| e <= tar.len()).ok_or("tar: entry runs past the end")?;
        let body = &tar[body_start..body_end];
        let mut name = field(0..100);
        if &h[257..262] == b"ustar" {
            let prefix = field(345..500);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        match h[156] {
            // A pax header: its `path` names the entry that follows.
            b'x' => {
                for rec in String::from_utf8_lossy(body).lines() {
                    if let Some((_, kv)) = rec.split_once(' ') {
                        if let Some(p) = kv.strip_prefix("path=") {
                            long_name = Some(p.to_string());
                        }
                    }
                }
            }
            b'g' => {}
            b'0' | 0 => out.push((long_name.take().unwrap_or(name), body.to_vec())),
            _ => {
                long_name = None;
            }
        }
        pos = body_start + size.div_ceil(512) * 512;
    }
    Ok(out)
}

/// The signature algorithms apk uses, by the word after `.SIGN.`.
fn algorithm(kind: &str) -> Option<&'static dyn ring::signature::VerificationAlgorithm> {
    Some(match kind {
        "RSA" => &ring::signature::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
        "RSA256" => &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        "RSA512" => &ring::signature::RSA_PKCS1_2048_8192_SHA512,
        _ => return None,
    })
}

/// A key name as apk uses it: a plain file name in the keys directory.
pub fn valid_key_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 200 && !n.starts_with('.') && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'+'))
}

/// Read one DER length; returns (length, bytes it took).
fn der_len(b: &[u8]) -> Option<(usize, usize)> {
    let first = *b.first()?;
    if first < 0x80 {
        return Some((first as usize, 1));
    }
    let n = (first & 0x7f) as usize;
    if n == 0 || n > 4 || b.len() < 1 + n {
        return None;
    }
    Some((b[1..=n].iter().fold(0usize, |acc, &x| (acc << 8) | x as usize), 1 + n))
}

/// The contents of the DER element of type `tag` at the start of `b`, and what follows it.
fn der(b: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    if *b.first()? != tag {
        return None;
    }
    let (len, took) = der_len(&b[1..])?;
    let start = 1 + took;
    let end = start.checked_add(len)?;
    Some((b.get(start..end)?, b.get(end..)?))
}

/// An RSA public key as `ring` takes it (PKCS #1 `RSAPublicKey`), from the
/// PEM `PUBLIC KEY` (SubjectPublicKeyInfo) files apk keeps in `/etc/apk/keys`.
pub fn rsa_key_from_pem(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN PUBLIC KEY-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    let der_bytes = base64::engine::general_purpose::STANDARD.decode(body.trim()).map_err(|_| "not a PEM public key".to_string())?;
    let (spki, _) = der(&der_bytes, 0x30).ok_or("not a SubjectPublicKeyInfo")?;
    let (alg, rest) = der(spki, 0x30).ok_or("not a SubjectPublicKeyInfo")?;
    // rsaEncryption, 1.2.840.113549.1.1.1
    const RSA_OID: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    if !alg.starts_with(RSA_OID) {
        return Err("not an RSA key".into());
    }
    let (bits, _) = der(rest, 0x03).ok_or("not a SubjectPublicKeyInfo")?;
    match bits.split_first() {
        Some((0, key)) => Ok(key.to_vec()),
        _ => Err("not a SubjectPublicKeyInfo".into()),
    }
}

/// Trusted keys by name, as in `/etc/apk/keys`.
pub type Keys = HashMap<String, Vec<u8>>;

/// One package as an index describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub version: String,
    pub arch: String,
    /// SHA-1 of the package's control member.
    pub control_sha1: [u8; 20],
    /// The size of the `.apk` file.
    pub size: u64,
    /// `D:`: names and capabilities, with version constraints, and `!conflicts`.
    pub depends: Vec<String>,
    /// `p:`: what else it provides (`so:`, `cmd:`, `pc:`, names), with versions.
    pub provides: Vec<String>,
    /// `i:`: installed by itself when all of these are installed.
    pub install_if: Vec<String>,
    /// `k:`: preferred among packages providing the same thing.
    pub priority: i64,
}

impl Entry {
    /// The file name of the package in its repository.
    pub fn file_name(&self) -> String {
        format!("{}-{}.apk", self.name, self.version)
    }
}

/// The name in a dependency, provide or install-if word, without its
/// version constraint: `so:libc.musl-x86_64.so.1`, `libcurl=8.23.0-r0` → `libcurl`.
pub fn bare(word: &str) -> &str {
    let w = word.strip_prefix('!').unwrap_or(word);
    let end = w.find(['=', '<', '>', '~']).unwrap_or(w.len());
    &w[..end]
}

/// A package or capability name that is safe as part of a file name.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 200 && !n.starts_with(['.', '-']) && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

fn parse_checksum(c: &str) -> Option<[u8; 20]> {
    let raw = base64::engine::general_purpose::STANDARD.decode(c.strip_prefix("Q1")?).ok()?;
    raw.try_into().ok()
}

/// Parse the text of `APKINDEX`. Entries that cannot be packages (a name or
/// version that is not safe, no SHA-1 checksum) are left out and counted.
pub fn parse_index(text: &str) -> (Vec<Entry>, usize) {
    let (mut out, mut skipped) = (Vec::new(), 0);
    for block in text.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut e = Entry::default();
        let mut sum = None;
        for line in block.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let words = || v.split_whitespace().map(str::to_string).collect::<Vec<_>>();
            match k {
                "P" => e.name = v.to_string(),
                "V" => e.version = v.to_string(),
                "A" => e.arch = v.to_string(),
                "C" => sum = parse_checksum(v),
                "S" => e.size = v.parse().unwrap_or(0),
                "D" => e.depends = words(),
                "p" => e.provides = words(),
                "i" => e.install_if = words(),
                "k" => e.priority = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        match sum {
            Some(s) if valid_name(&e.name) && crate::version::valid(&e.version) && valid_name(&e.version) && e.size > 0 => {
                e.control_sha1 = s;
                out.push(e);
            }
            _ => skipped += 1,
        }
    }
    (out, skipped)
}

/// A checked index: its entries, and the key that signed it.
#[derive(Debug)]
pub struct Index {
    pub entries: Vec<Entry>,
    pub skipped: usize,
    pub signed_by: Option<String>,
}

/// Check `APKINDEX.tar.gz` against the trusted keys and read it. With no
/// keys (`None`), the signature is not checked; the caller says so.
pub fn read_index(data: &[u8], keys: Option<&Keys>) -> Result<Index, String> {
    let sig = member(data, 0, MAX_SIGNATURE)?;
    let signatures = tar_files(&sig.plain)?;
    if signatures.is_empty() || !signatures.iter().all(|(n, _)| n.starts_with(".SIGN.")) {
        return Err("the index does not start with its signature".into());
    }
    let signed = &data[sig.end..];
    let mut signed_by = None;
    if let Some(keys) = keys {
        // Any one signature from a trusted key will do, as for apk.
        let mut why = Vec::new();
        for (name, bytes) in &signatures {
            let rest = &name[".SIGN.".len()..];
            let Some((kind, key_name)) = rest.split_once('.') else {
                why.push(format!("{name:?} is not named as apk names a signature"));
                continue;
            };
            if !valid_key_name(key_name) {
                why.push(format!("it names a key {key_name:?} that is not a plain file name"));
                continue;
            }
            let Some(alg) = algorithm(kind) else {
                why.push(format!("it is signed with {kind}, which is not supported"));
                continue;
            };
            let Some(key) = keys.get(key_name) else {
                why.push(format!("it is signed by {key_name}, which is not among the trusted keys"));
                continue;
            };
            match ring::signature::UnparsedPublicKey::new(alg, key).verify(signed, bytes) {
                Ok(()) => {
                    signed_by = Some(key_name.to_string());
                    break;
                }
                Err(_) => why.push(format!("the signature by {key_name} does not verify")),
            }
        }
        if signed_by.is_none() {
            return Err(format!("the index is not signed by a trusted key: {}", why.join("; ")));
        }
    }
    let control = member(data, sig.end, MAX_INDEX_TEXT)?;
    if control.end != data.len() {
        return Err("the index has more after its contents".into());
    }
    let files = tar_files(&control.plain)?;
    let text = files.iter().find(|(n, _)| n == "APKINDEX").map(|(_, b)| b).ok_or("the index holds no APKINDEX")?;
    let text = std::str::from_utf8(text).map_err(|_| "APKINDEX is not UTF-8")?;
    let (entries, skipped) = parse_index(text);
    Ok(Index { entries, skipped, signed_by })
}

/// Check a downloaded `.apk` against its index entry: its size, the SHA-1
/// of its control member, and the SHA-256 of its data member that the
/// control member states.
pub fn check_package(data: &[u8], e: &Entry) -> Result<(), String> {
    if data.len() as u64 != e.size {
        return Err(format!("is {} bytes, the index says {}", data.len(), e.size));
    }
    let sig = member(data, 0, MAX_SIGNATURE)?;
    if !tar_files(&sig.plain)?.iter().all(|(n, _)| n.starts_with(".SIGN.")) {
        return Err("does not start with its signature".into());
    }
    let control = member(data, sig.end, MAX_CONTROL)?;
    let raw = &data[control.start..control.end];
    if Sha1::digest(raw).as_slice() != e.control_sha1 {
        return Err("its control section does not match the checksum in the index".into());
    }
    let files = tar_files(&control.plain)?;
    let info = files.iter().find(|(n, _)| n == ".PKGINFO").map(|(_, b)| String::from_utf8_lossy(b).into_owned()).ok_or("has no .PKGINFO")?;
    let field = |k: &str| info.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.trim_start().strip_prefix('=')).map(|v| v.trim().to_string()));
    if field("pkgname").as_deref() != Some(e.name.as_str()) || field("pkgver").as_deref() != Some(e.version.as_str()) {
        return Err("its .PKGINFO names another package".into());
    }
    let datahash = field("datahash").ok_or("its .PKGINFO states no datahash")?;
    let data_part = &data[control.end..];
    if !data_part.starts_with(&[0x1f, 0x8b]) {
        return Err("has no data section after its control section".into());
    }
    if hex(&Sha256::digest(data_part)) != datahash.to_ascii_lowercase() {
        return Err("its contents do not match the datahash in its control section".into());
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
