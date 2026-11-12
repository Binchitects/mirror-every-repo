//! OpenPGP through GnuPG's own tools: `gpgv` to verify (the same verifier apt
//! uses) and `gpg` to sign with the site key. Arguments are passed directly,
//! never through a shell, and data goes through private temporary files.
use crate::AptError;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(cmd: &mut Command) -> Result<std::process::Output, AptError> {
    let program = cmd.get_program().to_string_lossy().to_string();
    cmd.output().map_err(|e| AptError::Gpg(format!("cannot run {program}: {e} (is GnuPG installed?)")))
}

fn last_lines(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join("; ")
}

/// Verify a clearsigned `InRelease` against a keyring and return the signed
/// text. The text is produced by gpgv itself, so only bytes that were covered
/// by a valid signature ever reach the parser.
pub fn verify_inrelease(keyring: &Path, inrelease: &[u8]) -> Result<Vec<u8>, AptError> {
    if !keyring.is_file() {
        return Err(AptError::Signature(format!("keyring {} does not exist", keyring.display())));
    }
    let tmp = tempfile::tempdir()?;
    let input = tmp.path().join("InRelease");
    let output = tmp.path().join("Release");
    std::fs::write(&input, inrelease)?;
    let out = run(Command::new("gpgv")
        .arg("--homedir")
        .arg(tmp.path())
        .arg("--keyring")
        .arg(keyring)
        .arg("--output")
        .arg(&output)
        .arg(&input))?;
    if !out.status.success() {
        return Err(AptError::Signature(last_lines(&out.stderr)));
    }
    Ok(std::fs::read(&output)?)
}

/// The key this site signs its own repository metadata with. It lives in a
/// GnuPG home directory on the collector.
pub struct SiteKey {
    home: PathBuf,
}

impl SiteKey {
    pub fn open(home: impl Into<PathBuf>) -> Result<Self, AptError> {
        let key = SiteKey { home: home.into() };
        if key.export(true)?.is_empty() {
            return Err(AptError::Gpg(format!("no key in {}; create one with `mer apt keygen`", key.home.display())));
        }
        Ok(key)
    }

    /// Create a new signing key (Ed25519, no passphrase, no expiry).
    pub fn generate(home: impl Into<PathBuf>, name: &str) -> Result<Self, AptError> {
        let home = home.into();
        std::fs::create_dir_all(&home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        }
        let key = SiteKey { home };
        if !key.export(true)?.is_empty() {
            return Err(AptError::Gpg(format!("{} already holds a key", key.home.display())));
        }
        let out = run(key
            .gpg()
            .args(["--passphrase", "", "--quick-generate-key", name, "ed25519", "sign", "never"]))?;
        if !out.status.success() {
            return Err(AptError::Gpg(last_lines(&out.stderr)));
        }
        Ok(key)
    }

    fn gpg(&self) -> Command {
        let mut c = Command::new("gpg");
        c.arg("--homedir").arg(&self.home).args(["--batch", "--yes", "--no-tty", "--pinentry-mode", "loopback"]);
        c
    }

    fn export(&self, armor: bool) -> Result<Vec<u8>, AptError> {
        if !self.home.is_dir() {
            return Ok(Vec::new());
        }
        let mut c = self.gpg();
        if armor {
            c.arg("--armor");
        }
        let out = run(c.arg("--export"))?;
        if !out.status.success() {
            return Err(AptError::Gpg(last_lines(&out.stderr)));
        }
        Ok(out.stdout)
    }

    /// ASCII-armored public key, for `signed-by=` on clients.
    pub fn public_armored(&self) -> Result<Vec<u8>, AptError> {
        self.export(true)
    }

    /// Binary public key, usable as a `gpgv --keyring`.
    pub fn public_binary(&self) -> Result<Vec<u8>, AptError> {
        self.export(false)
    }

    fn sign(&self, data: &[u8], mode: &[&str]) -> Result<Vec<u8>, AptError> {
        let tmp = tempfile::tempdir()?;
        let input = tmp.path().join("in");
        let output = tmp.path().join("out");
        std::fs::write(&input, data)?;
        let out = run(self
            .gpg()
            .args(["--passphrase", "", "--digest-algo", "SHA512"])
            .args(mode)
            .arg("--output")
            .arg(&output)
            .arg(&input))?;
        if !out.status.success() {
            return Err(AptError::Gpg(last_lines(&out.stderr)));
        }
        Ok(std::fs::read(&output)?)
    }

    /// `InRelease`: the text with an inline signature.
    pub fn clearsign(&self, data: &[u8]) -> Result<Vec<u8>, AptError> {
        self.sign(data, &["--clearsign"])
    }

    /// `Release.gpg`: a detached, armored signature.
    pub fn detach_sign(&self, data: &[u8]) -> Result<Vec<u8>, AptError> {
        self.sign(data, &["--armor", "--detach-sign"])
    }
}

impl Drop for SiteKey {
    fn drop(&mut self) {
        // gpg starts an agent per home directory; do not leave it running.
        let _ = Command::new("gpgconf").arg("--homedir").arg(&self.home).args(["--kill", "gpg-agent"]).output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring_of(key: &SiteKey, dir: &Path) -> PathBuf {
        let p = dir.join("keyring.gpg");
        std::fs::write(&p, key.public_binary().unwrap()).unwrap();
        p
    }

    #[test]
    fn sign_then_verify_returns_exactly_the_signed_text() {
        let dir = tempfile::tempdir().unwrap();
        let key = SiteKey::generate(dir.path().join("home"), "Test Site Key").unwrap();
        let ring = keyring_of(&key, dir.path());
        let text = b"Suite: stable\nSHA256:\n abc 1 x\n";
        let signed = key.clearsign(text).unwrap();
        assert!(signed.starts_with(b"-----BEGIN PGP SIGNED MESSAGE-----"));
        assert_eq!(verify_inrelease(&ring, &signed).unwrap(), text);
        assert!(key.detach_sign(text).unwrap().starts_with(b"-----BEGIN PGP SIGNATURE-----"));
        assert!(key.public_armored().unwrap().starts_with(b"-----BEGIN PGP PUBLIC KEY BLOCK-----"));
    }

    #[test]
    fn tampering_and_foreign_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let key = SiteKey::generate(dir.path().join("a"), "Key A").unwrap();
        let other = SiteKey::generate(dir.path().join("b"), "Key B").unwrap();
        let ring = keyring_of(&key, dir.path());
        let signed = key.clearsign(b"Suite: stable\n").unwrap();

        let tampered = String::from_utf8(signed.clone()).unwrap().replace("stable", "sid");
        assert!(matches!(verify_inrelease(&ring, tampered.as_bytes()), Err(AptError::Signature(_))));

        let foreign = other.clearsign(b"Suite: stable\n").unwrap();
        assert!(matches!(verify_inrelease(&ring, &foreign), Err(AptError::Signature(_))));

        assert!(matches!(verify_inrelease(&ring, b"Suite: stable\n"), Err(AptError::Signature(_))), "unsigned");
        assert!(matches!(verify_inrelease(&dir.path().join("nope.gpg"), &signed), Err(AptError::Signature(_))));
    }

    #[test]
    fn text_outside_the_signed_block_is_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let key = SiteKey::generate(dir.path().join("home"), "Test").unwrap();
        let ring = keyring_of(&key, dir.path());
        let signed = String::from_utf8(key.clearsign(b"Suite: stable\n").unwrap()).unwrap();
        // An attacker wraps the genuine signed block with unsigned text.
        let wrapped = format!("Suite: evil\n\n{signed}\nSuite: evil-too\n");
        match verify_inrelease(&ring, wrapped.as_bytes()) {
            Ok(text) => assert_eq!(text, b"Suite: stable\n", "only signed bytes may come back"),
            Err(AptError::Signature(_)) => {} // refusing outright is fine too
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_home_holds_one_key_and_open_needs_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SiteKey::open(dir.path().join("missing")).is_err());
        let _k = SiteKey::generate(dir.path().join("home"), "One").unwrap();
        assert!(SiteKey::generate(dir.path().join("home"), "Two").is_err());
        assert!(SiteKey::open(dir.path().join("home")).is_ok());
    }
}
