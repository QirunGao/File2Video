use crate::core::bitstream::bytes_to_bits_msb;

pub struct FecEncoded {
    pub sys_stream: Vec<u8>,
    pub par_stream: Vec<u8>,
}

pub fn encode_systematic_ra(src_bytes: &[u8]) -> FecEncoded {
    let sys_stream = bytes_to_bits_msb(src_bytes);

    let mut par_stream = Vec::<u8>::with_capacity(sys_stream.len() * 2);
    let mut acc = 0u8;
    for &bit in &sys_stream {
        // Simple systematic RA-like parity: accumulator output duplicated per source bit.
        // This keeps parity length = 2 * N while preserving accumulator memory across bits.
        acc ^= bit & 1;
        let p = acc & 1;
        par_stream.push(p);
        par_stream.push(p);
    }

    FecEncoded {
        sys_stream,
        par_stream,
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
    chunk_bytes: usize,
    window_chunks: usize,
) -> Vec<f32> {
    let mut out = sys_llr.to_vec();
    if sys_llr.is_empty() {
        return out;
    }
    if chunk_bytes == 0 || window_chunks == 0 {
        return out;
    }

    let coded_n = sys_llr.len().min(par_llr.len() / 2);
    if coded_n == 0 {
        return out;
    }

    let window_bits = chunk_bytes
        .saturating_mul(8)
        .saturating_mul(window_chunks)
        .max(1);

    let mut start = 0usize;
    let mut alpha_prior = [0.0f32, NEG_INF]; // accumulator starts from zero state

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
            let yp0 = par_llr[2 * i];
            let yp1 = par_llr[2 * i + 1];
            let mut next = [NEG_INF; 2];

            for s_prev in 0..=1u8 {
                let a = alpha[k][s_prev as usize];
                if !a.is_finite() {
                    continue;
                }
                for s_cur in 0..=1u8 {
                    let u = s_prev ^ s_cur;
                    let branch = a
                        + bit_metric(ys, u)
                        + bit_metric(yp0, s_cur)
                        + bit_metric(yp1, s_cur);
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
            let yp0 = par_llr[2 * i];
            let yp1 = par_llr[2 * i + 1];
            let mut cur = [NEG_INF; 2];

            for s_prev in 0..=1u8 {
                let mut best = NEG_INF;
                for s_cur in 0..=1u8 {
                    let u = s_prev ^ s_cur;
                    let branch = bit_metric(ys, u)
                        + bit_metric(yp0, s_cur)
                        + bit_metric(yp1, s_cur)
                        + beta[k + 1][s_cur as usize];
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
            let yp0 = par_llr[2 * i];
            let yp1 = par_llr[2 * i + 1];
            let mut score0 = NEG_INF;
            let mut score1 = NEG_INF;

            for s_prev in 0..=1u8 {
                let a = alpha[k][s_prev as usize];
                if !a.is_finite() {
                    continue;
                }
                for s_cur in 0..=1u8 {
                    let u = s_prev ^ s_cur;
                    let score = a
                        + bit_metric(ys, u)
                        + bit_metric(yp0, s_cur)
                        + bit_metric(yp1, s_cur)
                        + beta[k + 1][s_cur as usize];
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

    fn bits_to_llr(bits: &[u8], mag: f32) -> Vec<f32> {
        bits.iter()
            .map(|&b| if b == 0 { mag } else { -mag })
            .collect()
    }

    #[test]
    fn ra_soft_decode_roundtrip_clean() {
        let src = b"hello ra window decoder";
        let enc = encode_systematic_ra(src);
        let sys_llr = bits_to_llr(&enc.sys_stream, 4.0);
        let par_llr = bits_to_llr(&enc.par_stream, 4.0);
        let post = decode_systematic_ra_llr(&sys_llr, &par_llr, 16, 2);
        let bits = hard_llr_to_bits(&post);
        assert_eq!(bits, enc.sys_stream);
    }

    #[test]
    fn ra_soft_decode_works_when_systematic_erased() {
        let src = b"fec";
        let enc = encode_systematic_ra(src);
        let sys_llr = vec![0.0; enc.sys_stream.len()];
        let par_llr = bits_to_llr(&enc.par_stream, 6.0);
        let post = decode_systematic_ra_llr(&sys_llr, &par_llr, 4, 1);
        let bits = hard_llr_to_bits(&post);
        assert_eq!(bits, enc.sys_stream);
    }
}
