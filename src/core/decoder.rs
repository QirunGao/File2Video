use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use crate::core::bitstream::{bits_to_bytes_msb, hard_llr_to_bits};
use crate::core::mapper::{write_slot_llr_sequential, StreamBuffers};
use crate::core::phy_channel::unpack_bytes_to_slots;
use crate::core::session::{extract_verified_payload, recover_meta_from_prefix};
use crate::params::cfg;

pub fn decode(
    input: PathBuf,
    out: PathBuf,
    window_chunks: usize,
    heavy_geom: bool,
    best_effort: bool,
) -> Result<()> {
    let bytes = fs::read(&input).with_context(|| format!("read input {:?}", input))?;

    let mut selected_profile = None;
    for profile_id in 0u8..=2 {
        let profile = cfg(profile_id)?;
        let slots = unpack_bytes_to_slots(&bytes, profile);
        let mut buffers = StreamBuffers::default();

        for (t, slot) in slots.iter().enumerate() {
            write_slot_llr_sequential(&mut buffers, slot, t, profile, 0.35);
        }

        let sys_bits = hard_llr_to_bits(&buffers.sys_llr);
        let sys_bytes = bits_to_bytes_msb(&sys_bits);
        if let Some(meta) = recover_meta_from_prefix(&sys_bytes, profile.r_meta) {
            if meta.profile == profile.id {
                selected_profile = Some((profile, sys_bytes));
                break;
            }
        }
    }

    let (profile, sys_bytes) = selected_profile.context("unable to lock profile from metadata prefix")?;
    let payload = extract_verified_payload(&sys_bytes, profile.r_meta, best_effort)?;
    fs::write(&out, payload).with_context(|| format!("write output {:?}", out))?;

    eprintln!(
        "decode(vnext7): profile={}, split=({},{}) bits, window_chunks={}, heavy_geom={}, best_effort={}",
        profile.id,
        profile.b_sys,
        profile.b_par,
        window_chunks,
        heavy_geom,
        best_effort,
    );

    Ok(())
}
