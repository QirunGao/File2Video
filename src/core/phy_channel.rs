use crate::params::ProfileCfg;

#[derive(Clone, Debug)]
pub struct SlotPayloadBits {
    pub sys_bits: Vec<u8>,
    pub par_bits: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct SlotLlr {
    pub sys_llr: Vec<f32>,
    pub par_llr: Vec<f32>,
    pub q_pilot: f32,
}

pub fn pack_slots_to_bytes(slots: &[SlotPayloadBits], profile: ProfileCfg) -> Vec<u8> {
    let mut bits = Vec::<u8>::with_capacity(slots.len() * profile.coded_bits_per_slot);
    for slot in slots {
        bits.extend(slot.sys_bits.iter().copied());
        bits.extend(slot.par_bits.iter().copied());
    }
    crate::core::bitstream::bits_to_bytes_msb(&bits)
}

pub fn unpack_bytes_to_slots(bytes: &[u8], profile: ProfileCfg) -> Vec<SlotLlr> {
    let bits = crate::core::bitstream::bytes_to_bits_msb(bytes);
    let slot_count = bits.len() / profile.coded_bits_per_slot;
    let mut out = Vec::with_capacity(slot_count);
    for t in 0..slot_count {
        let base = t * profile.coded_bits_per_slot;
        let sys = &bits[base..base + profile.b_sys];
        let par = &bits[base + profile.b_sys..base + profile.coded_bits_per_slot];
        let sys_llr = sys
            .iter()
            .map(|&b| if b == 0 { 8.0f32 } else { -8.0f32 })
            .collect::<Vec<_>>();
        let par_llr = par
            .iter()
            .map(|&b| if b == 0 { 8.0f32 } else { -8.0f32 })
            .collect::<Vec<_>>();
        out.push(SlotLlr {
            sys_llr,
            par_llr,
            q_pilot: 1.0,
        });
    }
    out
}
