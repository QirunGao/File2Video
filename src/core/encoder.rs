use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;

use crate::core::fec::encode_systematic_ra;
use crate::core::mapper::build_slot_payloads;
use crate::core::phy_channel::pack_slots_to_bytes;
use crate::core::session::{build_meta_bytes, crc32_bytes, prefix_strengthen, StreamMeta};
use crate::params::{cfg, Y_BLOCK};

pub fn encode(
    input: PathBuf,
    out: PathBuf,
    width: usize,
    height: usize,
    profile_id: u8,
    max_overhead: f32,
) -> Result<()> {
    if width % Y_BLOCK != 0 || height % Y_BLOCK != 0 {
        bail!("width/height must be multiples of {}", Y_BLOCK);
    }
    if !(0.0..=2.0).contains(&max_overhead) {
        bail!("max_overhead must be in [0.0, 2.0]");
    }

    let profile = cfg(profile_id)?;
    let data = fs::read(&input).with_context(|| format!("read input {:?}", input))?;
    if data.is_empty() {
        bail!("input is empty");
    }

    let meta = StreamMeta {
        profile: profile.id,
        file_len: data.len() as u64,
        file_crc32: crc32_bytes(&data),
    };

    let meta_bytes = build_meta_bytes(&meta);
    let src = prefix_strengthen(&meta_bytes, &data, profile.r_meta);

    let fec = encode_systematic_ra(&src);
    let mut slots = build_slot_payloads(&fec.sys_stream, &fec.par_stream, profile);

    if max_overhead > 0.0 {
        let extra_slots = ((slots.len() as f32) * max_overhead) as usize;
        if let Some(last) = slots.last().cloned() {
            slots.extend(std::iter::repeat_n(last, extra_slots));
        }
    }

    let out_bytes = pack_slots_to_bytes(&slots, profile);
    fs::write(&out, out_bytes).with_context(|| format!("write output {:?}", out))?;

    eprintln!(
        "encode(vnext7): profile={}, slots={}, split=({},{}) bits, pam={}, chunk_bytes={}, suggest_window={}",
        profile.id,
        slots.len(),
        profile.b_sys,
        profile.b_par,
        profile.pam_order,
        profile.chunk_bytes,
        profile.window_chunks_suggest,
    );

    Ok(())
}
