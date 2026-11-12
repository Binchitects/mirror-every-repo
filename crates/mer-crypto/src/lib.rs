//! Ed25519 signed envelopes, the vault's trust store, and the diode link MAC.
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use mer_core::{limits, Digest, PubKey};
use sha2::Sha256;
use std::path::Path;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("envelope malformed: {0}")]
    Malformed(&'static str),
    #[error("signature check failed: {valid} valid trusted signatures, {needed} needed")]
    Untrusted { valid: usize, needed: usize },
    #[error("key file: {0}")]
    KeyFile(String),
}

/// A private signing key.
pub struct KeyPair(SigningKey);

impl KeyPair {
    pub fn generate() -> Self {
        KeyPair(SigningKey::generate(&mut rand::rngs::OsRng))
    }
    pub fn from_seed(seed: [u8; 32]) -> Self {
        KeyPair(SigningKey::from_bytes(&seed))
    }
    pub fn public(&self) -> PubKey {
        PubKey(self.0.verifying_key().to_bytes())
    }
    fn sign(&self, domain: &[u8], body: &[u8]) -> [u8; 64] {
        let mut msg = Vec::with_capacity(domain.len() + body.len());
        msg.extend_from_slice(domain);
        msg.extend_from_slice(body);
        self.0.sign(&msg).to_bytes()
    }
    /// Write the private key (hex seed). The file is created private to its owner.
    pub fn save(&self, path: &Path) -> Result<(), CryptoError> {
        write_secret(path, &hex::encode(self.0.to_bytes()))
    }
    pub fn load(path: &Path) -> Result<Self, CryptoError> {
        Ok(KeyPair::from_seed(read_hex32(path)?))
    }
}

fn write_secret(path: &Path, hex: &str) -> Result<(), CryptoError> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| CryptoError::KeyFile(format!("{}: {e}", path.display())))?;
    writeln!(f, "{hex}").map_err(|e| CryptoError::KeyFile(e.to_string()))
}

fn read_hex32(path: &Path) -> Result<[u8; 32], CryptoError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CryptoError::KeyFile(format!("{}: {e}", path.display())))?;
    let bytes = hex::decode(text.trim())
        .map_err(|_| CryptoError::KeyFile(format!("{}: not hex", path.display())))?;
    bytes.try_into().map_err(|_| CryptoError::KeyFile(format!("{}: need 32 bytes", path.display())))
}

/// SHA-256 of a public key. Envelopes name signers by this.
pub fn key_hash(pk: &PubKey) -> Digest {
    Digest::of(&pk.0)
}

/// The public keys a vault accepts bundle signatures from, and how many
/// distinct ones must have signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustStore {
    keys: Vec<PubKey>,
    threshold: usize,
}

impl TrustStore {
    pub fn new(keys: Vec<PubKey>, threshold: usize) -> Self {
        TrustStore { keys, threshold: threshold.max(1) }
    }
    pub fn keys(&self) -> &[PubKey] {
        &self.keys
    }
    /// Text file: one hex public key per line, optional `threshold=N`, `#` comments.
    pub fn load(path: &Path) -> Result<Self, CryptoError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CryptoError::KeyFile(format!("{}: {e}", path.display())))?;
        let mut keys = Vec::new();
        let mut threshold = 1;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if let Some(n) = line.strip_prefix("threshold=") {
                threshold = n.parse().map_err(|_| CryptoError::KeyFile("bad threshold".into()))?;
            } else {
                keys.push(
                    PubKey::from_hex(line)
                        .map_err(|_| CryptoError::KeyFile(format!("bad public key line {line:?}")))?,
                );
            }
        }
        if keys.is_empty() {
            return Err(CryptoError::KeyFile(format!("{}: no keys", path.display())));
        }
        Ok(TrustStore::new(keys, threshold))
    }
    pub fn save(&self, path: &Path) -> Result<(), CryptoError> {
        let mut text = format!("# mirror-every-repo trust store\nthreshold={}\n", self.threshold);
        for k in &self.keys {
            text.push_str(&k.to_hex());
            text.push('\n');
        }
        std::fs::write(path, text).map_err(|e| CryptoError::KeyFile(e.to_string()))
    }
}

const SIG_ENTRY: usize = 32 + 64;

/// Fixed binary envelope:
/// `magic[4] | body_len u32 BE | body | sig_count u8 | sig_count x (key_hash[32] | sig[64])`
/// with `sig = Ed25519(domain || body)`.
pub fn seal_envelope(magic: &[u8; 4], domain: &[u8], body: &[u8], signers: &[&KeyPair]) -> Vec<u8> {
    assert!(!signers.is_empty() && signers.len() <= limits::MAX_SIGNATURES);
    let mut out = Vec::with_capacity(9 + body.len() + signers.len() * SIG_ENTRY);
    out.extend_from_slice(magic);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out.push(signers.len() as u8);
    for k in signers {
        out.extend_from_slice(&key_hash(&k.public()).0);
        out.extend_from_slice(&k.sign(domain, body));
    }
    out
}

/// Total length of the envelope starting at `data[0]`, from its own header.
/// Nothing is trusted yet; lengths are capped before use.
pub fn envelope_len(data: &[u8], max_body: usize) -> Result<usize, CryptoError> {
    if data.len() < 9 {
        return Err(CryptoError::Malformed("too short"));
    }
    let body_len = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
    if body_len > max_body {
        return Err(CryptoError::Malformed("body too large"));
    }
    let count_at = 8 + body_len;
    let count = *data.get(count_at).ok_or(CryptoError::Malformed("truncated"))? as usize;
    if count == 0 || count > limits::MAX_SIGNATURES {
        return Err(CryptoError::Malformed("signature count"));
    }
    let total = count_at + 1 + count * SIG_ENTRY;
    if data.len() < total {
        return Err(CryptoError::Malformed("truncated"));
    }
    Ok(total)
}

/// Verify an envelope and return its body. The body must not be parsed, or
/// even looked at, before this returns Ok.
pub fn open_envelope<'a>(
    magic: &[u8; 4],
    domain: &[u8],
    max_body: usize,
    env: &'a [u8],
    trust: &TrustStore,
) -> Result<&'a [u8], CryptoError> {
    let total = envelope_len(env, max_body)?;
    if total != env.len() {
        return Err(CryptoError::Malformed("trailing bytes"));
    }
    if &env[0..4] != magic {
        return Err(CryptoError::Malformed("wrong magic"));
    }
    let body_len = u32::from_be_bytes(env[4..8].try_into().unwrap()) as usize;
    let body = &env[8..8 + body_len];
    let count = env[8 + body_len] as usize;
    let mut msg = Vec::with_capacity(domain.len() + body.len());
    msg.extend_from_slice(domain);
    msg.extend_from_slice(body);

    let mut signed_by: Vec<PubKey> = Vec::new();
    for i in 0..count {
        let at = 8 + body_len + 1 + i * SIG_ENTRY;
        let hash = &env[at..at + 32];
        let sig = Signature::from_bytes(env[at + 32..at + SIG_ENTRY].try_into().unwrap());
        let Some(key) = trust.keys.iter().find(|k| key_hash(k).0 == hash) else { continue };
        if signed_by.contains(key) {
            continue; // the same key twice counts once
        }
        let Ok(vk) = VerifyingKey::from_bytes(&key.0) else { continue };
        if vk.verify_strict(&msg, &sig).is_ok() {
            signed_by.push(*key);
        }
    }
    if signed_by.len() < trust.threshold {
        return Err(CryptoError::Untrusted { valid: signed_by.len(), needed: trust.threshold });
    }
    Ok(body)
}

/// Shared secret that authenticates every packet on the diode link, so the
/// receiver does no work for packets that did not come from the collector.
#[derive(Clone)]
pub struct LinkKey([u8; 32]);

impl LinkKey {
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        LinkKey(k)
    }
    pub fn from_bytes(k: [u8; 32]) -> Self {
        LinkKey(k)
    }
    pub fn save(&self, path: &Path) -> Result<(), CryptoError> {
        write_secret(path, &hex::encode(self.0))
    }
    pub fn load(path: &Path) -> Result<Self, CryptoError> {
        Ok(LinkKey(read_hex32(path)?))
    }
    fn mac_state(&self, data: &[u8]) -> Hmac<Sha256> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.0).expect("hmac accepts any key length");
        m.update(data);
        m
    }
    /// First 16 bytes of HMAC-SHA256.
    pub fn tag(&self, data: &[u8]) -> [u8; 16] {
        let full = self.mac_state(data).finalize().into_bytes();
        full[..16].try_into().unwrap()
    }
    /// Constant-time check.
    pub fn verify(&self, data: &[u8], tag: &[u8]) -> bool {
        tag.len() == 16 && self.mac_state(data).verify_truncated_left(tag).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGIC: &[u8; 4] = b"MSD1";
    const DOMAIN: &[u8] = b"mer/descriptor/v1\0";

    fn key(n: u8) -> KeyPair {
        KeyPair::from_seed([n; 32])
    }
    fn trust(keys: &[&KeyPair], threshold: usize) -> TrustStore {
        TrustStore::new(keys.iter().map(|k| k.public()).collect(), threshold)
    }

    #[test]
    fn signed_body_opens_with_the_right_key_only() {
        let (a, b) = (key(1), key(2));
        let env = seal_envelope(MAGIC, DOMAIN, b"hello", &[&a]);
        assert_eq!(open_envelope(MAGIC, DOMAIN, 1024, &env, &trust(&[&a], 1)).unwrap(), b"hello");
        assert_eq!(
            open_envelope(MAGIC, DOMAIN, 1024, &env, &trust(&[&b], 1)),
            Err(CryptoError::Untrusted { valid: 0, needed: 1 })
        );
    }

    #[test]
    fn any_flipped_bit_is_rejected() {
        let a = key(1);
        let t = trust(&[&a], 1);
        let env = seal_envelope(MAGIC, DOMAIN, b"important body", &[&a]);
        for i in 0..env.len() {
            let mut bad = env.clone();
            bad[i] ^= 0x01;
            assert!(open_envelope(MAGIC, DOMAIN, 1024, &bad, &t).is_err(), "bit flip at byte {i}");
        }
    }

    #[test]
    fn signature_is_bound_to_its_domain_and_magic() {
        let a = key(1);
        let t = trust(&[&a], 1);
        let env = seal_envelope(MAGIC, DOMAIN, b"x", &[&a]);
        assert!(open_envelope(MAGIC, b"mer/report/v1\0", 1024, &env, &t).is_err());
        assert!(open_envelope(b"MSR1", DOMAIN, 1024, &env, &t).is_err());
    }

    #[test]
    fn threshold_counts_distinct_trusted_keys() {
        let (a, b, c) = (key(1), key(2), key(3));
        let two_of_three = trust(&[&a, &b, &c], 2);
        let one = seal_envelope(MAGIC, DOMAIN, b"x", &[&a]);
        let same_twice = seal_envelope(MAGIC, DOMAIN, b"x", &[&a, &a]);
        let two = seal_envelope(MAGIC, DOMAIN, b"x", &[&a, &c]);
        let with_stranger = seal_envelope(MAGIC, DOMAIN, b"x", &[&a, &key(9)]);
        assert!(open_envelope(MAGIC, DOMAIN, 1024, &one, &two_of_three).is_err());
        assert!(open_envelope(MAGIC, DOMAIN, 1024, &same_twice, &two_of_three).is_err());
        assert!(open_envelope(MAGIC, DOMAIN, 1024, &with_stranger, &two_of_three).is_err());
        assert!(open_envelope(MAGIC, DOMAIN, 1024, &two, &two_of_three).is_ok());
    }

    #[test]
    fn size_caps_and_truncation() {
        let a = key(1);
        let t = trust(&[&a], 1);
        let env = seal_envelope(MAGIC, DOMAIN, &[0u8; 100], &[&a]);
        assert_eq!(envelope_len(&env, 1024), Ok(env.len()));
        assert_eq!(envelope_len(&env, 99), Err(CryptoError::Malformed("body too large")));
        for cut in [0, 5, 50, env.len() - 1] {
            assert!(open_envelope(MAGIC, DOMAIN, 1024, &env[..cut], &t).is_err());
        }
        let mut longer = env.clone();
        longer.push(0);
        assert_eq!(
            open_envelope(MAGIC, DOMAIN, 1024, &longer, &t),
            Err(CryptoError::Malformed("trailing bytes"))
        );
        // A huge claimed length must not cause a huge allocation or a panic.
        let mut huge = env.clone();
        huge[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(open_envelope(MAGIC, DOMAIN, 1024, &huge, &t).is_err());
    }

    #[test]
    fn keys_and_trust_store_round_trip_through_files() {
        let dir = tempfile::tempdir().unwrap();
        let k = KeyPair::generate();
        let kp = dir.path().join("bundle.key");
        k.save(&kp).unwrap();
        assert!(k.save(&kp).is_err(), "never overwrite a key file");
        assert_eq!(KeyPair::load(&kp).unwrap().public(), k.public());

        let t = TrustStore::new(vec![k.public(), key(2).public()], 2);
        let tp = dir.path().join("trust.txt");
        t.save(&tp).unwrap();
        assert_eq!(TrustStore::load(&tp).unwrap(), t);
    }

    #[test]
    fn link_mac_accepts_only_the_exact_packet() {
        let k = LinkKey::from_bytes([7; 32]);
        let tag = k.tag(b"packet");
        assert!(k.verify(b"packet", &tag));
        assert!(!k.verify(b"packet!", &tag));
        assert!(!k.verify(b"packet", &tag[..15]));
        assert!(!LinkKey::from_bytes([8; 32]).verify(b"packet", &tag));
        let mut bad = tag;
        bad[15] ^= 1;
        assert!(!k.verify(b"packet", &bad));
    }
}
