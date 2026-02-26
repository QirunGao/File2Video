use anyhow::{Context, Result, bail};
use crc32fast::Hasher;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::params::{META_MAGIC, META_VERSION, META_VERSION_LEGACY_CRC32};

// ---------------------------------------------------------------------------
// Block-level writeback: ChunkTracker (vNext-7 §2, §11)
// ---------------------------------------------------------------------------

/// Computes CRC32 of a single decoded chunk for early-stop detection
/// (architecture §11, §15-item 4: internal-only, does not enter the protocol).
pub fn chunk_crc32(data: &[u8]) -> u32 {
    crc32_bytes(data)
}

/// Tracks which source chunks have converged during decoding.
pub struct ChunkTracker {
    chunk_bytes: usize,
    total_chunks: usize,
    converged: Vec<bool>,
    output: Vec<u8>,
}

impl ChunkTracker {
    /// Create a new tracker. `total_source_len` is the full systematic byte length.
    pub fn new(chunk_bytes: usize, total_source_len: usize) -> Self {
        let total_chunks = if total_source_len == 0 {
            0
        } else {
            total_source_len.div_ceil(chunk_bytes)
        };
        Self {
            chunk_bytes,
            total_chunks,
            converged: vec![false; total_chunks],
            output: vec![0u8; total_source_len],
        }
    }

    /// Mark chunk `chunk_idx` as converged and write its data into the output buffer.
    /// Returns `true` if this was a *new* convergence (first time for this chunk).
    pub fn try_commit_chunk(&mut self, chunk_idx: usize, chunk_data: &[u8]) -> bool {
        if chunk_idx >= self.total_chunks {
            return false;
        }
        let was_new = !self.converged[chunk_idx];
        self.converged[chunk_idx] = true;

        let start = chunk_idx * self.chunk_bytes;
        let end = (start + self.chunk_bytes).min(self.output.len());
        let copy_len = (end - start).min(chunk_data.len());
        self.output[start..start + copy_len].copy_from_slice(&chunk_data[..copy_len]);

        was_new
    }

    /// Returns `true` when every chunk has converged.
    pub fn all_converged(&self) -> bool {
        self.converged.iter().all(|&c| c)
    }

    /// Index of the first chunk that has not yet converged (for window sliding / reclaim).
    pub fn first_unconverged(&self) -> Option<usize> {
        self.converged.iter().position(|&c| !c)
    }

    /// Read-only access to the accumulated output bytes.
    pub fn output_bytes(&self) -> &[u8] {
        &self.output
    }

    /// Number of chunks that have converged so far.
    pub fn converged_count(&self) -> usize {
        self.converged.iter().filter(|&&c| c).count()
    }

    /// Total number of chunks being tracked.
    pub fn total_chunks(&self) -> usize {
        self.total_chunks
    }
}

/// Commit all chunks from decoded systematic bytes into a [`ChunkTracker`].
///
/// In single-pass decoding every chunk is treated as converged; future
/// multi-pass decoders can refine convergence per-chunk.
pub fn commit_chunks_from_decoded(
    sys_bytes: &[u8],
    chunk_bytes: usize,
    r_meta: usize,
) -> ChunkTracker {
    let meta_region = recover_meta_from_prefix(sys_bytes, r_meta)
        .map(|m| m.meta_len * r_meta)
        .unwrap_or(0);
    let data_bytes = &sys_bytes[meta_region.min(sys_bytes.len())..];
    let mut tracker = ChunkTracker::new(chunk_bytes, data_bytes.len());

    // Single-pass: mark every data chunk as converged
    for idx in 0..tracker.total_chunks {
        let start = idx * chunk_bytes;
        let end = (start + chunk_bytes).min(data_bytes.len());
        tracker.try_commit_chunk(idx, &data_bytes[start..end]);
    }

    tracker
}

const META_LEN_SHA256: usize = 46;
const META_LEN_LEGACY_CRC32: usize = 18;
pub const META_LEN_CURRENT: usize = META_LEN_SHA256;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileIntegrity {
    Sha256([u8; 32]),
    LegacyCrc32(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamMeta {
    pub profile: u8,
    pub file_len: u64,
    pub file_integrity: FileIntegrity,
    pub meta_len: usize,
}

pub fn build_meta_bytes(meta: &StreamMeta) -> Vec<u8> {
    match meta.file_integrity {
        FileIntegrity::Sha256(hash) => {
            let mut out = Vec::<u8>::with_capacity(META_LEN_SHA256);
            out.extend_from_slice(META_MAGIC);
            out.push(META_VERSION);
            out.push(meta.profile);
            out.extend_from_slice(&meta.file_len.to_le_bytes());
            out.extend_from_slice(&hash);
            out
        }
        FileIntegrity::LegacyCrc32(_) => {
            panic!("build_meta_bytes only supports current SHA-256 metadata")
        }
    }
}

pub fn parse_meta_bytes(data: &[u8]) -> Option<StreamMeta> {
    if data.len() < META_LEN_LEGACY_CRC32 {
        return None;
    }
    if &data[0..4] != META_MAGIC {
        return None;
    }
    let version = data[4];

    let profile = data[5];
    let file_len = u64::from_le_bytes(data[6..14].try_into().ok()?);
    match version {
        META_VERSION => {
            if data.len() < META_LEN_SHA256 {
                return None;
            }
            let file_sha256 = <[u8; 32]>::try_from(&data[14..46]).ok()?;
            Some(StreamMeta {
                profile,
                file_len,
                file_integrity: FileIntegrity::Sha256(file_sha256),
                meta_len: META_LEN_SHA256,
            })
        }
        META_VERSION_LEGACY_CRC32 => {
            let file_crc32 = u32::from_le_bytes(data[14..18].try_into().ok()?);
            Some(StreamMeta {
                profile,
                file_len,
                file_integrity: FileIntegrity::LegacyCrc32(file_crc32),
                meta_len: META_LEN_LEGACY_CRC32,
            })
        }
        _ => None,
    }
}

pub fn prefix_strengthen(meta: &[u8], data: &[u8], r_meta: usize) -> Vec<u8> {
    let mut out = Vec::<u8>::with_capacity(meta.len() * r_meta + data.len());
    for _ in 0..r_meta {
        out.extend_from_slice(meta);
    }
    out.extend_from_slice(data);
    out
}

pub fn recover_meta_from_prefix(sys_bytes: &[u8], r_meta: usize) -> Option<StreamMeta> {
    recover_meta_from_prefix_with_len(sys_bytes, r_meta, META_LEN_SHA256)
        .or_else(|| recover_meta_from_prefix_with_len(sys_bytes, r_meta, META_LEN_LEGACY_CRC32))
}

fn recover_meta_from_prefix_with_len(
    sys_bytes: &[u8],
    r_meta: usize,
    meta_len: usize,
) -> Option<StreamMeta> {
    let mut votes: HashMap<StreamMeta, usize> = HashMap::new();
    let mut best: Option<(StreamMeta, usize)> = None;

    for copy_idx in 0..r_meta {
        let s = copy_idx * meta_len;
        let e = s + meta_len;
        if e > sys_bytes.len() {
            break;
        }
        if let Some(meta) = parse_meta_bytes(&sys_bytes[s..e])
            .filter(|m| m.file_len > 0 && m.meta_len == meta_len)
        {
            let count = votes
                .entry(meta.clone())
                .and_modify(|c| *c += 1)
                .or_insert(1usize);
            if best.as_ref().is_none_or(|(_, best_count)| *count > *best_count) {
                best = Some((meta, *count));
            }
        }
    }
    best.map(|(meta, _)| meta)
}

pub fn crc32_bytes(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn hash_to_hex(hash: &FileIntegrity) -> String {
    match hash {
        FileIntegrity::Sha256(bytes) => bytes.iter().map(|b| format!("{:02x}", b)).collect(),
        FileIntegrity::LegacyCrc32(v) => format!("{:08x}", v),
    }
}

pub fn extract_verified_payload(
    sys_bytes: &[u8],
    r_meta: usize,
    best_effort: bool,
) -> Result<Vec<u8>> {
    let meta = recover_meta_from_prefix(sys_bytes, r_meta)
        .context("meta not recovered from strengthened prefix")?;
    let data_off = meta.meta_len * r_meta;
    let needed_end = data_off
        .checked_add(meta.file_len as usize)
        .context("decoded payload length overflow")?;

    if needed_end > sys_bytes.len() {
        bail!(
            "decoded stream too short: need {} bytes, got {} bytes",
            needed_end,
            sys_bytes.len()
        );
    }

    let payload = sys_bytes[data_off..needed_end].to_vec();
    match meta.file_integrity.clone() {
        FileIntegrity::Sha256(expected) => {
            let got = sha256_bytes(&payload);
            if got != expected {
                if best_effort {
                    return Ok(payload);
                }
                bail!(
                    "final hash mismatch (sha256): expected {}, got {}",
                    hash_to_hex(&FileIntegrity::Sha256(expected)),
                    hash_to_hex(&FileIntegrity::Sha256(got))
                );
            }
        }
        FileIntegrity::LegacyCrc32(expected) => {
            let got = crc32_bytes(&payload);
            if got != expected {
                if best_effort {
                    return Ok(payload);
                }
                bail!(
                    "final hash mismatch (legacy crc32): expected {}, got {}",
                    hash_to_hex(&FileIntegrity::LegacyCrc32(expected)),
                    hash_to_hex(&FileIntegrity::LegacyCrc32(got))
                );
            }
        }
    }

    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_meta_prefers_majority_copy() {
        let meta = StreamMeta {
            profile: 1,
            file_len: 12345,
            file_integrity: FileIntegrity::Sha256([0x11; 32]),
            meta_len: META_LEN_SHA256,
        };
        let good = build_meta_bytes(&meta);

        let mut bad_meta = meta.clone();
        bad_meta.file_len += 7;
        let bad = build_meta_bytes(&bad_meta);

        let mut sys = Vec::new();
        sys.extend_from_slice(&bad);
        sys.extend_from_slice(&good);
        sys.extend_from_slice(&good);
        sys.extend_from_slice(&good);
        sys.extend_from_slice(b"payload");

        let got = recover_meta_from_prefix(&sys, 4).expect("meta");
        assert_eq!(got, meta);
    }

    #[test]
    fn parse_legacy_crc32_meta_is_supported() {
        let mut raw = Vec::new();
        raw.extend_from_slice(META_MAGIC);
        raw.push(META_VERSION_LEGACY_CRC32);
        raw.push(2);
        raw.extend_from_slice(&123u64.to_le_bytes());
        raw.extend_from_slice(&0xAABBCCDDu32.to_le_bytes());

        let meta = parse_meta_bytes(&raw).expect("legacy meta");
        assert_eq!(meta.profile, 2);
        assert_eq!(meta.file_len, 123);
        assert_eq!(meta.meta_len, META_LEN_LEGACY_CRC32);
        assert_eq!(meta.file_integrity, FileIntegrity::LegacyCrc32(0xAABBCCDD));
    }

    #[test]
    fn chunk_tracker_basic_functionality() {
        let chunk_bytes = 8;
        let total_len = 20; // 3 chunks: 8 + 8 + 4

        let mut tracker = ChunkTracker::new(chunk_bytes, total_len);
        assert_eq!(tracker.total_chunks(), 3);
        assert!(!tracker.all_converged());
        assert_eq!(tracker.first_unconverged(), Some(0));

        // Commit chunk 0
        assert!(tracker.try_commit_chunk(0, &[1u8; 8]));
        assert!(!tracker.all_converged());
        assert_eq!(tracker.first_unconverged(), Some(1));
        assert_eq!(tracker.converged_count(), 1);

        // Duplicate commit returns false
        assert!(!tracker.try_commit_chunk(0, &[1u8; 8]));

        // Commit chunk 2 (partial, last chunk)
        assert!(tracker.try_commit_chunk(2, &[3u8; 4]));
        assert_eq!(tracker.first_unconverged(), Some(1));

        // Commit chunk 1
        assert!(tracker.try_commit_chunk(1, &[2u8; 8]));
        assert!(tracker.all_converged());
        assert_eq!(tracker.first_unconverged(), None);

        // Verify output
        let out = tracker.output_bytes();
        assert_eq!(&out[0..8], &[1u8; 8]);
        assert_eq!(&out[8..16], &[2u8; 8]);
        assert_eq!(&out[16..20], &[3u8; 4]);
    }

    #[test]
    fn chunk_crc32_is_consistent() {
        let data = b"hello world";
        let c1 = super::chunk_crc32(data);
        let c2 = super::chunk_crc32(data);
        assert_eq!(c1, c2);
        assert_ne!(c1, 0);
    }

    #[test]
    fn commit_chunks_from_decoded_marks_all_converged() {
        // No valid metadata prefix → meta_region=0, tracks all bytes as data
        let data = vec![0xABu8; 100];
        let tracker = commit_chunks_from_decoded(&data, 32, 1);
        assert!(tracker.all_converged());
        assert_eq!(tracker.output_bytes(), &data[..]);
    }
}
