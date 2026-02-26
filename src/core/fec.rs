use crate::core::bitstream::bytes_to_bits_msb;
use crate::params::ProfileCfg;

/// Hard decision: map LLR to bit (positive→0, negative→1).
#[inline]
fn llr_to_hard_bit(llr: f32) -> u8 {
    if llr >= 0.0 { 0 } else { 1 }
}

pub struct FecEncoded {
    pub sys_stream: Vec<u8>,
    pub par_stream: Vec<u8>,
}

/// QC interleaving: cyclic-shift permutation within groups of size `z`.
/// For partial trailing groups (len < z), the shift is still `gi % z` but
/// the modular arithmetic uses `len` to keep indices in bounds; the inverse
/// in `qc_deinterleave_llr` uses the same convention so the pair is consistent.
fn qc_interleave(raw: &[u8], z: usize) -> Vec<u8> {
    let mut out = vec![0u8; raw.len()];
    for (gi, chunk) in raw.chunks(z).enumerate() {
        let shift = gi % z;
        let base = gi * z;
        let len = chunk.len();
        for (j, &v) in chunk.iter().enumerate() {
            out[base + (j + shift) % len] = v;
        }
    }
    out
}

/// Inverse QC interleaving for f32 LLR streams.
fn qc_deinterleave_llr(data: &[f32], z: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; data.len()];
    for (gi, chunk) in data.chunks(z).enumerate() {
        let shift = gi % z;
        let base = gi * z;
        let len = chunk.len();
        for (new_j, &v) in chunk.iter().enumerate() {
            out[base + (new_j + len - shift) % len] = v;
        }
    }
    out
}

/// SC coupling: XOR of source bits at the same intra-block position
/// in up to `m` previous blocks (block size = qc_z * 8).
#[inline]
fn sc_coupling(sys: &[u8], i: usize, block_size: usize, m: usize) -> u8 {
    let blk = i / block_size;
    let mut c = 0u8;
    for d in 1..=m.min(blk) {
        c ^= sys[i - d * block_size] & 1;
    }
    c
}

pub fn encode_systematic_ra(src_bytes: &[u8], profile: ProfileCfg) -> FecEncoded {
    let sys_stream = bytes_to_bits_msb(src_bytes);
    let q = profile.ra_repeat as usize;
    let block_size = profile.qc_z * 8;

    let mut raw_par = Vec::<u8>::with_capacity(sys_stream.len() * q);
    let mut acc = 0u8;
    for (i, &bit) in sys_stream.iter().enumerate() {
        let coupled = (bit & 1) ^ sc_coupling(&sys_stream, i, block_size, profile.sc_memory);
        acc ^= coupled;
        let p = acc & 1;
        for _ in 0..q {
            raw_par.push(p);
        }
    }

    FecEncoded {
        sys_stream,
        par_stream: qc_interleave(&raw_par, profile.qc_z),
    }
}

const NEG_INF: f32 = -1.0e30;

#[inline]
fn bit_metric(llr: f32, bit: u8) -> f32 {
    if bit & 1 == 0 { llr } else { -llr }
}

#[inline]
fn norm_pair(v: &mut [f32; 2]) {
    let m = v[0].max(v[1]);
    if m.is_finite() {
        v[0] -= m;
        v[1] -= m;
    }
}

pub fn decode_systematic_ra_llr(
    sys_llr: &[f32],
    par_llr: &[f32],
    profile: ProfileCfg,
    window_chunks: usize,
) -> Vec<f32> {
    let q = profile.ra_repeat as usize;
    let block_size = profile.qc_z * 8;
    let m = profile.sc_memory;

    let mut out = sys_llr.to_vec();
    if sys_llr.is_empty() {
        return out;
    }
    if profile.chunk_bytes == 0 || window_chunks == 0 {
        return out;
    }

    let deint_par = qc_deinterleave_llr(par_llr, profile.qc_z);
    let coded_n = sys_llr.len().min(deint_par.len() / q);
    if coded_n == 0 {
        return out;
    }

    // Hard decisions from systematic LLRs for SC coupling
    let sys_hard: Vec<u8> = sys_llr.iter().map(|&l| llr_to_hard_bit(l)).collect();

    let window_bits = profile
        .chunk_bytes
        .saturating_mul(8)
        .saturating_mul(window_chunks)
        .max(1);

    let mut start = 0usize;
    let mut alpha_prior = [0.0f32, NEG_INF];

    while start < coded_n {
        let end = (start + window_bits).min(coded_n);
        let n = end - start;
        let mut alpha = vec![[NEG_INF; 2]; n + 1];
        let mut beta = vec![[0.0f32; 2]; n + 1];
        alpha[0] = alpha_prior;
        norm_pair(&mut alpha[0]);

        for k in 0..n {
            let i = start + k;
            let ys = sys_llr[i];
            let c = sc_coupling(&sys_hard, i, block_size, m);
            let mut next = [NEG_INF; 2];

            for s_prev in 0..=1u8 {
                let a = alpha[k][s_prev as usize];
                if !a.is_finite() {
                    continue;
                }
                for s_cur in 0..=1u8 {
                    let u = s_prev ^ s_cur ^ c;
                    let mut branch = a + bit_metric(ys, u);
                    for r in 0..q {
                        branch += bit_metric(deint_par[q * i + r], s_cur);
                    }
                    let dst = &mut next[s_cur as usize];
                    if branch > *dst {
                        *dst = branch;
                    }
                }
            }
            norm_pair(&mut next);
            alpha[k + 1] = next;
        }

        beta[n] = [0.0, 0.0];
        for k in (0..n).rev() {
            let i = start + k;
            let ys = sys_llr[i];
            let c = sc_coupling(&sys_hard, i, block_size, m);
            let mut cur = [NEG_INF; 2];

            for s_prev in 0..=1u8 {
                let mut best = NEG_INF;
                for s_cur in 0..=1u8 {
                    let u = s_prev ^ s_cur ^ c;
                    let mut branch = bit_metric(ys, u);
                    for r in 0..q {
                        branch += bit_metric(deint_par[q * i + r], s_cur);
                    }
                    branch += beta[k + 1][s_cur as usize];
                    if branch > best {
                        best = branch;
                    }
                }
                cur[s_prev as usize] = best;
            }
            norm_pair(&mut cur);
            beta[k] = cur;
        }

        for k in 0..n {
            let i = start + k;
            let ys = sys_llr[i];
            let c = sc_coupling(&sys_hard, i, block_size, m);
            let mut score0 = NEG_INF;
            let mut score1 = NEG_INF;

            for s_prev in 0..=1u8 {
                let a = alpha[k][s_prev as usize];
                if !a.is_finite() {
                    continue;
                }
                for s_cur in 0..=1u8 {
                    let u = s_prev ^ s_cur ^ c;
                    let mut score = a + bit_metric(ys, u);
                    for r in 0..q {
                        score += bit_metric(deint_par[q * i + r], s_cur);
                    }
                    score += beta[k + 1][s_cur as usize];
                    if u == 0 {
                        if score > score0 {
                            score0 = score;
                        }
                    } else if score > score1 {
                        score1 = score;
                    }
                }
            }
            out[i] = score0 - score1;
        }

        alpha_prior = alpha[n];
        start = end;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bitstream::hard_llr_to_bits;
    use crate::params::cfg;

    fn bits_to_llr(bits: &[u8], mag: f32) -> Vec<f32> {
        bits.iter()
            .map(|&b| if b == 0 { mag } else { -mag })
            .collect()
    }

    #[test]
    fn ra_soft_decode_roundtrip_clean() {
        let profile = cfg(0).unwrap();
        let src = b"hello ra window decoder";
        let enc = encode_systematic_ra(src, profile);
        let sys_llr = bits_to_llr(&enc.sys_stream, 4.0);
        let par_llr = bits_to_llr(&enc.par_stream, 4.0);
        let post = decode_systematic_ra_llr(&sys_llr, &par_llr, profile, 2);
        let bits = hard_llr_to_bits(&post);
        assert_eq!(bits, enc.sys_stream);
    }

    #[test]
    fn ra_soft_decode_works_when_systematic_erased() {
        let profile = cfg(0).unwrap();
        let src = b"fec";
        let enc = encode_systematic_ra(src, profile);
        let sys_llr = vec![0.0; enc.sys_stream.len()];
        let par_llr = bits_to_llr(&enc.par_stream, 6.0);
        let post = decode_systematic_ra_llr(&sys_llr, &par_llr, profile, 1);
        let bits = hard_llr_to_bits(&post);
        assert_eq!(bits, enc.sys_stream);
    }
}
