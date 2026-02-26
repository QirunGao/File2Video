use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;

use crate::core::fec::encode_systematic_ra;
use crate::core::mapper::build_slot_payloads;
use crate::core::phy_channel::pack_slots_to_yuv420;
use crate::core::session::{
    FileIntegrity, META_LEN_CURRENT, StreamMeta, build_meta_bytes, prefix_strengthen, sha256_bytes,
};
use crate::params::{Y_BLOCK, cfg};

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
        file_integrity: FileIntegrity::Sha256(sha256_bytes(&data)),
        meta_len: META_LEN_CURRENT,
    };

    let meta_bytes = build_meta_bytes(&meta);
    let src = prefix_strengthen(&meta_bytes, &data, profile.r_meta);

    let fec = encode_systematic_ra(&src, profile);
    let mut slots = build_slot_payloads(&fec.sys_stream, &fec.par_stream, profile);

    if max_overhead > 0.0 {
        let extra_slots = ((slots.len() as f32) * max_overhead) as usize;
        if let Some(last) = slots.last().cloned() {
            slots.extend(std::iter::repeat_n(last, extra_slots));
        }
    }

    let out_bytes = pack_slots_to_yuv420(&slots, width, height, profile)?;
    fs::write(&out, out_bytes).with_context(|| format!("write output {:?}", out))?;

    let frames = slots.len().div_ceil(profile.slots_per_frame).max(1);

    eprintln!(
        "encode(vnext7): profile={}, frames={}, slots={}, slots_per_frame={}, split=({},{}) bits, pam={}, fec(z={},m={},q={}), chunk_bytes={}, suggest_window={}",
        profile.id,
        frames,
        slots.len(),
        profile.slots_per_frame,
        profile.b_sys,
        profile.b_par,
        profile.pam_order,
        profile.qc_z,
        profile.sc_memory,
        profile.ra_repeat,
        profile.chunk_bytes,
        profile.window_chunks_suggest,
    );

    Ok(())
}
