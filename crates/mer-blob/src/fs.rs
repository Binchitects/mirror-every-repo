use crate::{BlobReader, BlobStat, BlobStore, Expect, PutOutcome, StoreError};
use mer_core::{Digest, Hasher};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const FORMAT: &str = "mer-blobstore 1\nalgo=sha256\nfanout=2/2\n";

/// Filesystem backend.
///
/// ```text
/// <root>/FORMAT
/// <root>/blobs/sha256/ab/cd/<64 hex>     read-only once placed
/// <root>/tmp/<pid>-<n>.part              same filesystem, renamed into place
/// ```
pub struct FsStore {
    root: PathBuf,
    counter: AtomicU64,
}

impl FsStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("blobs/sha256"))?;
        fs::create_dir_all(root.join("tmp"))?;
        let marker = root.join("FORMAT");
        match fs::read_to_string(&marker) {
            Ok(s) if s == FORMAT => {}
            Ok(s) => {
                return Err(StoreError::Format(format!(
                    "unsupported store format {:?}",
                    s.lines().next().unwrap_or("")
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::write(&marker, FORMAT)?;
                sync_dir(&root)?;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(FsStore { root, counter: AtomicU64::new(0) })
    }

    fn path_of(&self, d: &Digest) -> PathBuf {
        let hex = d.to_hex();
        self.root.join("blobs/sha256").join(&hex[0..2]).join(&hex[2..4]).join(&hex)
    }

    fn tmp_path(&self) -> PathBuf {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        self.root.join("tmp").join(format!("{}-{}.part", std::process::id(), n))
    }
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

impl BlobStore for FsStore {
    fn put(&self, src: &mut dyn Read, expect: Expect) -> Result<PutOutcome, StoreError> {
        let tmp = self.tmp_path();
        let result = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            let mut hasher = Hasher::new();
            let mut size = 0u64;
            let mut buf = vec![0u8; 128 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                f.write_all(&buf[..n])?;
                size += n as u64;
            }
            let digest = hasher.finish();
            if let Some(want) = expect.size {
                if want != size {
                    return Err(StoreError::SizeMismatch { expected: want, got: size });
                }
            }
            if let Some(want) = expect.digest {
                if want != digest {
                    return Err(StoreError::DigestMismatch { expected: want, got: digest });
                }
            }
            let dest = self.path_of(&digest);
            // An existing file of the right size was placed by an earlier verified
            // write (placement is an atomic rename of verified bytes).
            if let Ok(meta) = fs::metadata(&dest) {
                if meta.len() == size {
                    return Ok(PutOutcome { digest, size, existed: true });
                }
            }
            f.sync_all()?;
            drop(f);
            let mut perm = fs::metadata(&tmp)?.permissions();
            perm.set_readonly(true);
            fs::set_permissions(&tmp, perm)?;
            let shard = dest.parent().expect("blob path has a parent");
            fs::create_dir_all(shard)?;
            fs::rename(&tmp, &dest)?;
            sync_dir(shard)?;
            Ok(PutOutcome { digest, size, existed: false })
        })();
        if tmp.exists() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn stat(&self, d: &Digest) -> Result<Option<BlobStat>, StoreError> {
        match fs::metadata(self.path_of(d)) {
            Ok(m) => Ok(Some(BlobStat { digest: *d, size: m.len() })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn open(&self, d: &Digest, range: Option<(u64, u64)>) -> Result<BlobReader, StoreError> {
        let mut f = match File::open(self.path_of(d)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(*d))
            }
            Err(e) => return Err(e.into()),
        };
        let size = f.metadata()?.len();
        let (off, len) = range.unwrap_or((0, size));
        if off.checked_add(len).is_none_or(|end| end > size) {
            return Err(StoreError::BadRange);
        }
        if off > 0 {
            f.seek(SeekFrom::Start(off))?;
        }
        Ok(Box::new(f.take(len)))
    }

    fn delete(&self, d: &Digest) -> Result<(), StoreError> {
        let p = self.path_of(d);
        match fs::remove_file(&p) {
            Ok(()) => {
                if let Some(shard) = p.parent() {
                    sync_dir(shard)?;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn list(&self) -> Result<Vec<BlobStat>, StoreError> {
        let mut out = Vec::new();
        for a in fs::read_dir(self.root.join("blobs/sha256"))? {
            let a = a?;
            if !a.file_type()?.is_dir() {
                continue;
            }
            for b in fs::read_dir(a.path())? {
                let b = b?;
                if !b.file_type()?.is_dir() {
                    continue;
                }
                for f in fs::read_dir(b.path())? {
                    let f = f?;
                    let name = f.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if let Ok(digest) = Digest::from_hex(name) {
                        out.push(BlobStat { digest, size: f.metadata()?.len() });
                    }
                }
            }
        }
        out.sort_by_key(|s| s.digest);
        Ok(out)
    }

    fn sweep_tmp(&self) -> Result<u64, StoreError> {
        let mut n = 0;
        for e in fs::read_dir(self.root.join("tmp"))? {
            let e = e?;
            if e.file_type()?.is_file() {
                fs::remove_file(e.path())?;
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FsStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FsStore::open(dir.path().join("store")).unwrap();
        (dir, s)
    }

    fn tmp_count(dir: &tempfile::TempDir) -> usize {
        fs::read_dir(dir.path().join("store/tmp")).unwrap().count()
    }

    #[test]
    fn put_then_read_back() {
        let (dir, s) = store();
        let data = vec![7u8; 300_000];
        let out = s.put_bytes(&data).unwrap();
        assert_eq!(out.digest, Digest::of(&data));
        assert_eq!(out.size, 300_000);
        assert!(!out.existed);
        assert_eq!(s.read_all(&out.digest).unwrap(), data);
        assert_eq!(s.stat(&out.digest).unwrap(), Some(BlobStat { digest: out.digest, size: 300_000 }));
        let hex = out.digest.to_hex();
        let p = dir.path().join("store/blobs/sha256").join(&hex[0..2]).join(&hex[2..4]).join(&hex);
        assert!(p.is_file(), "fan-out layout");
        assert_eq!(tmp_count(&dir), 0);
    }

    #[test]
    fn identical_content_is_stored_once() {
        let (_dir, s) = store();
        let a = s.put_bytes(b"same bytes").unwrap();
        let b = s.put_bytes(b"same bytes").unwrap();
        assert!(!a.existed);
        assert!(b.existed);
        assert_eq!(a.digest, b.digest);
        assert_eq!(s.list().unwrap().len(), 1);
    }

    #[test]
    fn wrong_digest_or_size_stores_nothing() {
        let (dir, s) = store();
        let mut r: &[u8] = b"evil";
        let err = s.put(&mut r, Expect::exact(Digest::of(b"good"), 4)).unwrap_err();
        assert!(matches!(err, StoreError::DigestMismatch { .. }));
        let mut r: &[u8] = b"evil!";
        let err = s.put(&mut r, Expect::exact(Digest::of(b"evil!"), 4)).unwrap_err();
        assert!(matches!(err, StoreError::SizeMismatch { expected: 4, got: 5 }));
        assert!(s.list().unwrap().is_empty());
        assert_eq!(s.stat(&Digest::of(b"evil")).unwrap(), None);
        assert_eq!(tmp_count(&dir), 0, "failed writes leave no temp file");
    }

    #[test]
    fn range_reads() {
        let (_dir, s) = store();
        let d = s.put_bytes(b"0123456789").unwrap().digest;
        let mut out = String::new();
        s.open(&d, Some((3, 4))).unwrap().read_to_string(&mut out).unwrap();
        assert_eq!(out, "3456");
        let mut out = String::new();
        s.open(&d, Some((10, 0))).unwrap().read_to_string(&mut out).unwrap();
        assert_eq!(out, "");
        assert!(matches!(s.open(&d, Some((8, 3))), Err(StoreError::BadRange)));
        assert!(matches!(s.open(&d, Some((u64::MAX, 2))), Err(StoreError::BadRange)));
    }

    #[test]
    fn delete_is_idempotent_and_missing_is_reported() {
        let (_dir, s) = store();
        let d = s.put_bytes(b"bye").unwrap().digest;
        s.delete(&d).unwrap();
        s.delete(&d).unwrap();
        assert_eq!(s.stat(&d).unwrap(), None);
        assert!(matches!(s.open(&d, None), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn list_is_sorted_and_complete() {
        let (_dir, s) = store();
        let mut want: Vec<Digest> =
            (0..50u32).map(|i| s.put_bytes(&i.to_be_bytes()).unwrap().digest).collect();
        want.sort();
        let got: Vec<Digest> = s.list().unwrap().into_iter().map(|b| b.digest).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn sweep_removes_interrupted_writes_and_reopen_keeps_data() {
        let (dir, s) = store();
        let d = s.put_bytes(b"keep me").unwrap().digest;
        fs::write(dir.path().join("store/tmp/999-0.part"), b"torn write").unwrap();
        assert_eq!(s.sweep_tmp().unwrap(), 1);
        drop(s);
        let s = FsStore::open(dir.path().join("store")).unwrap();
        assert_eq!(s.read_all(&d).unwrap(), b"keep me");
    }

    #[test]
    fn refuses_unknown_store_format() {
        let (dir, s) = store();
        drop(s);
        fs::write(dir.path().join("store/FORMAT"), "mer-blobstore 99\n").unwrap();
        assert!(matches!(FsStore::open(dir.path().join("store")), Err(StoreError::Format(_))));
    }
}
