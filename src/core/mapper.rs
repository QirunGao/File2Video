use crate::core::phy_channel::{SlotLlr, SlotPayloadBits};
use crate::params::ProfileCfg;

#[derive(Default)]
pub struct StreamBuffers {
    pub sys_llr: Vec<f32>,
    pub par_llr: Vec<f32>,
}

pub fn build_slot_payloads(
    sys_stream: &[u8],
    par_stream: &[u8],
    profile: ProfileCfg,
) -> Vec<SlotPayloadBits> {
    let slot_count = sys_stream
        .len()
        .div_ceil(profile.b_sys)
        .max(par_stream.len().div_ceil(profile.b_par));
    let mut slots = Vec::with_capacity(slot_count);
    for t in 0..slot_count {
        let sys_start = t * profile.b_sys;
        let par_start = t * profile.b_par;
        let mut sys_bits = vec![0u8; profile.b_sys];
        let mut par_bits = vec![0u8; profile.b_par];
        let sys_n = profile
            .b_sys
            .min(sys_stream.len().saturating_sub(sys_start));
        let par_n = profile
            .b_par
            .min(par_stream.len().saturating_sub(par_start));
        if sys_n > 0 {
            sys_bits[..sys_n].copy_from_slice(&sys_stream[sys_start..sys_start + sys_n]);
        }
        if par_n > 0 {
            par_bits[..par_n].copy_from_slice(&par_stream[par_start..par_start + par_n]);
        }
        slots.push(SlotPayloadBits { sys_bits, par_bits });
    }
    slots
}

pub fn write_slot_llr_sequential(
    buffers: &mut StreamBuffers,
    slot: &SlotLlr,
    t: usize,
    profile: ProfileCfg,
    t_drop: f32,
) {
    let sys_start = t * profile.b_sys;
    let par_start = t * profile.b_par;

    if buffers.sys_llr.len() < sys_start + profile.b_sys {
        buffers.sys_llr.resize(sys_start + profile.b_sys, 0.0);
    }
    if buffers.par_llr.len() < par_start + profile.b_par {
        buffers.par_llr.resize(par_start + profile.b_par, 0.0);
    }

    if slot.q_pilot < t_drop {
        for v in &mut buffers.sys_llr[sys_start..sys_start + profile.b_sys] {
            *v = 0.0;
        }
        for v in &mut buffers.par_llr[par_start..par_start + profile.b_par] {
            *v = 0.0;
        }
    } else {
        buffers.sys_llr[sys_start..sys_start + profile.b_sys].copy_from_slice(&slot.sys_llr);
        buffers.par_llr[par_start..par_start + profile.b_par].copy_from_slice(&slot.par_llr);
    }
}
