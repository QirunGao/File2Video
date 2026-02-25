use anyhow::{bail, Context, Result};
use crc32fast::Hasher;

use crate::params::{META_MAGIC, META_VERSION};

#[derive(Clone, Debug)]
pub struct StreamMeta {
    pub profile: u8,
    pub file_len: u64,
    pub file_crc32: u32,
}

pub fn build_meta_bytes(meta: &StreamMeta) -> Vec<u8> {
    let mut out = Vec::<u8>::with_capacity(4 + 1 + 1 + 8 + 4);
    out.extend_from_slice(META_MAGIC);
    out.push(META_VERSION);
    out.push(meta.profile);
    out.extend_from_slice(&meta.file_len.to_le_bytes());
    out.extend_from_slice(&meta.file_crc32.to_le_bytes());
    out
}

pub fn parse_meta_bytes(data: &[u8]) -> Option<StreamMeta> {
    if data.len() < 18 {
        return None;
    }
    if &data[0..4] != META_MAGIC {
        return None;
    }
    if data[4] != META_VERSION {
        return None;
    }

    let profile = data[5];
    let file_len = u64::from_le_bytes(data[6..14].try_into().ok()?);
    let file_crc32 = u32::from_le_bytes(data[14..18].try_into().ok()?);
    Some(StreamMeta {
        profile,
        file_len,
        file_crc32,
    })
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
    let meta_len = 18usize;
    for copy_idx in 0..r_meta {
        let s = copy_idx * meta_len;
        let e = s + meta_len;
        if e > sys_bytes.len() {
            break;
        }
        if let Some(meta) = parse_meta_bytes(&sys_bytes[s..e]) {
            return Some(meta);
        }
    }
    None
}

pub fn crc32_bytes(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

pub fn extract_verified_payload(sys_bytes: &[u8], r_meta: usize, best_effort: bool) -> Result<Vec<u8>> {
    let meta = recover_meta_from_prefix(sys_bytes, r_meta).context("meta not recovered from strengthened prefix")?;
    let meta_len = 18usize;
    let data_off = meta_len * r_meta;
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
    let crc = crc32_bytes(&payload);
    if crc != meta.file_crc32 {
        if best_effort {
            return Ok(payload);
        }
        bail!(
            "final hash mismatch: expected {:08x}, got {:08x}",
            meta.file_crc32,
            crc
        );
    }

    Ok(payload)
}
