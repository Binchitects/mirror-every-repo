//! Removable media transport: a bundle is written as `.mab` volume files sized
//! to the medium (USB stick, disk, DVD) and read back on the other side.
//!
//! ```text
//! volume  = header (4096) | signed descriptor, zero padded to 4096 | slots | trailer (4096)
//! ```
//! Every volume carries the signed descriptor, so import can start from any
//! volume, in any order, and a damaged sector costs only the slots it touches.
#[cfg(test)]
mod tests;

use mer_bundle_format::{unpad_slot, pad_slot, FormatError, SegmentSink, SegmentSource, SlotOutcome};
use mer_core::{BundleId, Hasher};
use std::fs::File;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

const HEADER_LEN: usize = 4096;
const VOLUME_MAGIC: &[u8; 8] = b"MERVOL01";
const TRAILER_MAGIC: &[u8; 8] = b"MERVEND1";
const MAX_DESCRIPTOR_ENVELOPE: u32 = 32 * 1024;
pub const EXTENSION: &str = "mab";

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("volume size {volume_size} is too small: one slot of this bundle needs {needed} bytes")]
    VolumeTooSmall { volume_size: u64, needed: u64 },
}

/// Fixed-layout volume header (big-endian), 4096 bytes with a crc32c at the end.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VolumeHeader {
    set_id: [u8; 16],
    bundle_id: BundleId,
    vol_index: u32,
    vol_count: u32,
    first_segment: u32,
    slots_in_volume: u32,
    slot_size: u32,
    segment_size: u32,
    descriptor_len: u32,
}

impl VolumeHeader {
    fn encode(&self) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        h[0..8].copy_from_slice(VOLUME_MAGIC);
        h[8..10].copy_from_slice(&1u16.to_be_bytes());
        h[12..28].copy_from_slice(&self.set_id);
        h[28..60].copy_from_slice(&self.bundle_id.0);
        for (at, v) in [
            (60, self.vol_index),
            (64, self.vol_count),
            (68, self.first_segment),
            (72, self.slots_in_volume),
            (76, self.slot_size),
            (80, self.segment_size),
            (84, self.descriptor_len),
        ] {
            h[at..at + 4].copy_from_slice(&v.to_be_bytes());
        }
        let crc = crc32c::crc32c(&h[..HEADER_LEN - 4]);
        h[HEADER_LEN - 4..].copy_from_slice(&crc.to_be_bytes());
        h
    }

    /// Everything here is attacker-controlled until the descriptor verifies.
    fn decode(h: &[u8]) -> Result<Self, String> {
        if h.len() != HEADER_LEN || &h[0..8] != VOLUME_MAGIC {
            return Err("not a volume file".into());
        }
        let crc = u32::from_be_bytes(h[HEADER_LEN - 4..].try_into().unwrap());
        if crc32c::crc32c(&h[..HEADER_LEN - 4]) != crc {
            return Err("volume header is damaged".into());
        }
        if u16::from_be_bytes([h[8], h[9]]) != 1 || h[10] != 0 {
            return Err("unsupported volume version or kind".into());
        }
        let u = |at: usize| u32::from_be_bytes(h[at..at + 4].try_into().unwrap());
        let header = VolumeHeader {
            set_id: h[12..28].try_into().unwrap(),
            bundle_id: BundleId(h[28..60].try_into().unwrap()),
            vol_index: u(60),
            vol_count: u(64),
            first_segment: u(68),
            slots_in_volume: u(72),
            slot_size: u(76),
            segment_size: u(80),
            descriptor_len: u(84),
        };
        if header.descriptor_len == 0 || header.descriptor_len > MAX_DESCRIPTOR_ENVELOPE {
            return Err("descriptor length out of range".into());
        }
        Ok(header)
    }

    fn slots_offset(&self) -> u64 {
        HEADER_LEN as u64 + (self.descriptor_len as u64).div_ceil(4096) * 4096
    }
}

fn volume_name(src: &dyn SegmentSource, index: u32) -> String {
    let d = src.descriptor();
    format!("{}-e{}-s{:010}-d{:04}.{EXTENSION}", d.channel.to_hex(), d.epoch, d.seq, index)
}

/// Write a sealed bundle as volume files of at most `volume_size` bytes each.
/// Returns the files written, in order.
pub fn write_volumes(src: &dyn SegmentSource, out_dir: &Path, volume_size: u64) -> Result<Vec<PathBuf>, MediaError> {
    let params = src.params();
    let envelope = src.descriptor_envelope();
    let fixed = HEADER_LEN as u64 + (envelope.len() as u64).div_ceil(4096) * 4096 + HEADER_LEN as u64;
    let needed = fixed + params.slot_size as u64;
    if volume_size < needed {
        return Err(MediaError::VolumeTooSmall { volume_size, needed });
    }
    let per_volume = ((volume_size - fixed) / params.slot_size as u64).min(u32::MAX as u64) as u32;
    let vol_count = params.segment_count.div_ceil(per_volume);
    let mut set_id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut set_id);

    std::fs::create_dir_all(out_dir)?;
    let mut written = Vec::new();
    let mut listing = String::from("UNAUTHENTICATED, INFORMATIONAL ONLY\n");
    for vol in 0..vol_count {
        let first = vol * per_volume;
        let slots = per_volume.min(params.segment_count - first);
        let header = VolumeHeader {
            set_id,
            bundle_id: src.descriptor().bundle_id,
            vol_index: vol,
            vol_count,
            first_segment: first,
            slots_in_volume: slots,
            slot_size: params.slot_size,
            segment_size: params.segment_size,
            descriptor_len: envelope.len() as u32,
        };
        let name = volume_name(src, vol);
        let path = out_dir.join(&name);
        let part = out_dir.join(format!("{name}.part"));
        let mut out = HashingWriter { inner: File::create(&part)?, hasher: Hasher::new() };
        out.write_all(&header.encode())?;
        out.write_all(envelope)?;
        let pad = header.slots_offset() as usize - HEADER_LEN - envelope.len();
        out.write_all(&vec![0u8; pad])?;
        for j in 0..slots {
            let block = src.read_block(first + j)?;
            out.write_all(&pad_slot(&block, params.slot_size))?;
        }
        let HashingWriter { inner: mut file, hasher } = out;
        let mut trailer = vec![0u8; HEADER_LEN];
        trailer[0..8].copy_from_slice(TRAILER_MAGIC);
        trailer[8..12].copy_from_slice(&slots.to_be_bytes());
        trailer[12..44].copy_from_slice(&hasher.finish().0);
        let crc = crc32c::crc32c(&trailer[..HEADER_LEN - 4]);
        trailer[HEADER_LEN - 4..].copy_from_slice(&crc.to_be_bytes());
        file.write_all(&trailer)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&part, &path)?;
        listing.push_str(&format!(
            "{name}  volume {} of {vol_count}  segments {first}..{}\n",
            vol + 1,
            first + slots
        ));
        written.push(path);
    }
    std::fs::write(out_dir.join("CONTENTS.txt"), listing)?;
    #[cfg(unix)]
    File::open(out_dir)?.sync_all()?;
    Ok(written)
}

struct HashingWriter {
    inner: File,
    hasher: Hasher,
}

impl Write for HashingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaReport {
    pub volumes_read: u32,
    /// Files that were not usable, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
    pub slots_accepted: u64,
    pub slots_already_had: u64,
    /// Slots that failed their checksum or proof (damaged media).
    pub slots_damaged: u64,
    /// For each bundle seen: the segment ranges still missing afterwards.
    pub bundles: Vec<(BundleId, Vec<Range<u32>>)>,
}

/// Read every `.mab` file directly inside `dir` into `sink`. File names are
/// ignored, symlinks are not followed, and the medium is never written to.
pub fn import_dir(dir: &Path, sink: &dyn SegmentSink) -> Result<MediaReport, MediaError> {
    let mut report = MediaReport::default();
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        // file_type() does not follow symlinks.
        if !entry.file_type()?.is_file() {
            report.skipped.push((path, "not a regular file".into()));
            continue;
        }
        files.push(path);
    }
    files.sort();
    let mut seen: Vec<BundleId> = Vec::new();
    for path in files {
        match import_volume(&path, sink, &mut report) {
            Ok(id) => {
                report.volumes_read += 1;
                if !seen.contains(&id) {
                    seen.push(id);
                }
            }
            Err(reason) => report.skipped.push((path, reason)),
        }
    }
    report.bundles = seen.into_iter().map(|id| (id, sink.missing(&id))).collect();
    Ok(report)
}

fn import_volume(path: &Path, sink: &dyn SegmentSink, report: &mut MediaReport) -> Result<BundleId, String> {
    let io = |e: std::io::Error| e.to_string();
    let mut file = File::open(path).map_err(io)?;
    let mut raw = vec![0u8; HEADER_LEN];
    file.read_exact(&mut raw).map_err(|_| "shorter than a volume header".to_string())?;
    let header = VolumeHeader::decode(&raw)?;

    // The descriptor is verified by the sink (signature first) before any
    // other header field is used.
    let mut envelope = vec![0u8; header.descriptor_len as usize];
    file.read_exact(&mut envelope).map_err(|_| "truncated descriptor".to_string())?;
    let id = sink.offer_descriptor(&envelope).map_err(|e| e.to_string())?;
    let params = sink.params(&id).ok_or("bundle is not being received")?;
    if header.bundle_id != id || header.slot_size != params.slot_size || header.segment_size != params.segment_size {
        return Err("volume header disagrees with its signed descriptor".into());
    }
    let end = header.first_segment as u64 + header.slots_in_volume as u64;
    if end > params.segment_count as u64 {
        return Err("volume claims segments beyond the bundle".into());
    }

    let skip = header.slots_offset() - HEADER_LEN as u64 - header.descriptor_len as u64;
    std::io::copy(&mut (&mut file).take(skip), &mut std::io::sink()).map_err(io)?;
    let mut slot = vec![0u8; params.slot_size as usize];
    for j in 0..header.slots_in_volume {
        let index = header.first_segment + j;
        // Always read: positions are fixed, so one bad slot never shifts the rest.
        if file.read_exact(&mut slot).is_err() {
            break; // truncated volume: keep what was readable
        }
        if sink.has(&id, index) {
            report.slots_already_had += 1;
            continue;
        }
        match unpad_slot(&slot, &params, index) {
            Ok(block) => match sink.offer_block(&id, index, block) {
                SlotOutcome::Accepted => report.slots_accepted += 1,
                SlotOutcome::Duplicate => report.slots_already_had += 1,
                SlotOutcome::Rejected(_) => report.slots_damaged += 1,
            },
            Err(_) => report.slots_damaged += 1,
        }
    }
    Ok(id)
}
