use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;

use crate::core::bitstream::{bits_to_bytes_msb, hard_llr_to_bits};
use crate::core::fec::decode_systematic_ra_llr;
use crate::core::mapper::{StreamBuffers, write_slot_llr_sequential};
use crate::core::phy_channel::unpack_yuv420_to_slots;
use crate::core::session::{commit_chunks_from_decoded, extract_verified_payload, recover_meta_from_prefix};
use crate::params::{PROFILE_CFGS, cfg};

pub fn decode(
    input: PathBuf,
    out: PathBuf,
    profile_hint: Option<u8>,
    width: usize,
    height: usize,
    window_chunks: usize,
    heavy_geom: bool,
    best_effort: bool,
) -> Result<()> {
    if window_chunks == 0 {
        bail!("window_chunks must be > 0");
    }

    let bytes = fs::read(&input).with_context(|| format!("read input {:?}", input))?;

    let mut selected_profile = None;
    let probe_profiles: Vec<u8> = if let Some(p) = profile_hint {
        vec![p]
    } else {
        PROFILE_CFGS.iter().map(|p| p.id).collect()
    };

    for profile_id in probe_profiles {
        let profile = cfg(profile_id)?;
        let phy = unpack_yuv420_to_slots(&bytes, width, height, profile, heavy_geom)?;
        let mut buffers = StreamBuffers::default();

        for (t, slot) in phy.slots.iter().enumerate() {
            write_slot_llr_sequential(&mut buffers, slot, t, profile, profile.q_drop_threshold);
        }

        let post_sys_llr = decode_systematic_ra_llr(
            &buffers.sys_llr,
            &buffers.par_llr,
            profile,
            window_chunks,
        );
        let sys_bits = hard_llr_to_bits(&post_sys_llr);
        let sys_bytes = bits_to_bytes_msb(&sys_bits);
        if let Some(meta) = recover_meta_from_prefix(&sys_bytes, profile.r_meta) {
            if meta.profile == profile.id {
                selected_profile = Some((profile, sys_bytes, phy.frames_total, phy.frames_synced));
                break;
            }
        }
    }

    let (profile, sys_bytes, frames_total, frames_synced) =
        selected_profile.context("unable to lock profile from metadata prefix")?;

    // Block-level convergence tracking (vNext-7 §2, §11)
    let tracker = commit_chunks_from_decoded(&sys_bytes, profile.chunk_bytes, profile.r_meta);

    let payload = extract_verified_payload(&sys_bytes, profile.r_meta, best_effort)?;
    fs::write(&out, payload).with_context(|| format!("write output {:?}", out))?;

    eprintln!(
        "decode(vnext7): profile={}, frames={}/{}, split=({},{}) bits, fec(z={},m={},q={}), window_chunks={}, heavy_geom={}, best_effort={}, profile_hint={}, chunks_converged={}/{}",
        profile.id,
        frames_synced,
        frames_total,
        profile.b_sys,
        profile.b_par,
        profile.qc_z,
        profile.sc_memory,
        profile.ra_repeat,
        window_chunks,
        heavy_geom,
        best_effort,
        profile_hint
            .map(|v| v.to_string())
            .unwrap_or_else(|| "auto".to_string()),
        tracker.converged_count(),
        tracker.total_chunks(),
    );

    Ok(())
}
