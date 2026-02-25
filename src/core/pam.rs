use super::*;

#[derive(Clone)]
pub(crate) struct Means {
    pub(crate) y: Vec<f64>,
    pub(crate) u: Vec<f64>,
    pub(crate) v: Vec<f64>,
    pub(crate) sigma_y: f64,
    pub(crate) sigma_u: f64,
    pub(crate) sigma_v: f64,
    pub(crate) sigma_y_levels: Vec<f64>,
    pub(crate) sigma_u_levels: Vec<f64>,
    pub(crate) sigma_v_levels: Vec<f64>,
}

const PAYLOAD_SAMPLE_SEARCH_PX: i32 = 1;

pub(crate) fn encode_bits_pam_yuv(
    frame: &mut Yuv420Frame,
    width: usize,
    height: usize,
    pcfg: &ProfileCfg,
    pos: &[(usize, usize)],
    bits: &[u8],
) {
    let n = pcfg.bits_per_plane;
    let y_levels = pcfg.y_slice();
    let mut bit_idx = 0usize;

    for &(bx, by) in pos {
        let mut take_bits = |cnt: usize| -> u8 {
            let mut v = 0u8;
            for _ in 0..cnt {
                v <<= 1;
                v |= bits.get(bit_idx).copied().unwrap_or(0) & 1;
                bit_idx += 1;
            }
            v
        };

        let gy = take_bits(n);
        let sy = (gray_to_binary_u8(gy) as usize).min(pcfg.pam_m - 1);

        fill_y_block(&mut frame.y, width, Y_BLOCK, bx, by, y_levels[sy]);
        // Payload is carried primarily on Y for compression and 4:2:0 robustness; keep chroma neutral.
        fill_uv_block_420(&mut frame.u, width, height, bx, by, Y_BLOCK, 128);
        fill_uv_block_420(&mut frame.v, width, height, bx, by, Y_BLOCK, 128);
    }
}

fn append_symbol_gray_bit_llrs_maxlog(
    out: &mut Vec<f32>,
    x: f64,
    means: &[f64],
    sigmas: &[f64],
    sigma_fallback: f64,
    nbits: usize,
) {
    if means.is_empty() || nbits == 0 {
        return;
    }
    let mut metrics = [f64::INFINITY; 8];
    let m = means.len().min(metrics.len());
    for s in 0..m {
        let mu = means[s];
        let sigma = sigmas.get(s).copied().unwrap_or(sigma_fallback).max(1e-3);
        let sigma2 = sigma * sigma;
        let d = x - mu;
        // Negative log-likelihood (up to additive constants) for max-log LLR.
        metrics[s] = (d * d) / (2.0 * sigma2) + sigma.ln();
    }
    for bit_i in 0..nbits {
        let shift = nbits - 1 - bit_i;
        let mut min0 = f64::INFINITY;
        let mut min1 = f64::INFINITY;
        for (s, &metric) in metrics[..m].iter().enumerate() {
            let g = binary_to_gray_u8(s as u8);
            if ((g >> shift) & 1) == 0 {
                if metric < min0 {
                    min0 = metric;
                }
            } else if metric < min1 {
                min1 = metric;
            }
        }
        // Sign convention matches previous implementation: positive => bit 0 is more likely.
        out.push((min1 - min0) as f32);
    }
}

fn symbol_confidence_score(x: f64, means: &[f64], sigmas: &[f64], sigma_fallback: f64) -> f64 {
    if means.is_empty() {
        return 0.0;
    }
    let mut best = f64::INFINITY;
    let mut second = f64::INFINITY;
    for (i, &mu) in means.iter().enumerate() {
        let sigma = sigmas.get(i).copied().unwrap_or(sigma_fallback).max(1e-3);
        let sigma2 = sigma * sigma;
        let d = (x - mu) * (x - mu) / (2.0 * sigma2) + sigma.ln();
        if d < best {
            second = best;
            best = d;
        } else if d < second {
            second = d;
        }
    }
    if second.is_finite() { second - best } else { 0.0 }
}

fn sample_symbol_triplet(frame: &Yuv420Frame, bx: usize, by: usize, geom: Geom) -> (f64, f64, f64) {
    let yv = avg_y_block_geom(&frame.y, frame.width, frame.height, Y_BLOCK, bx, by, geom);
    let uv = avg_uv_block_420_geom(&frame.u, frame.width, frame.height, Y_BLOCK, bx, by, geom);
    let vv = avg_uv_block_420_geom(&frame.v, frame.width, frame.height, Y_BLOCK, bx, by, geom);
    (yv, uv, vv)
}

fn sample_symbol_triplet_best(frame: &Yuv420Frame, bx: usize, by: usize, means: &Means, geom: Geom) -> (f64, f64, f64) {
    if PAYLOAD_SAMPLE_SEARCH_PX <= 0 {
        return sample_symbol_triplet(frame, bx, by, geom);
    }
    let mut best_xyz = sample_symbol_triplet(frame, bx, by, geom);
    let mut best_score =
        symbol_confidence_score(best_xyz.0, &means.y, &means.sigma_y_levels, means.sigma_y) +
        0.6 * symbol_confidence_score(best_xyz.1, &means.u, &means.sigma_u_levels, means.sigma_u) +
        0.6 * symbol_confidence_score(best_xyz.2, &means.v, &means.sigma_v_levels, means.sigma_v);
    for ddy in -PAYLOAD_SAMPLE_SEARCH_PX..=PAYLOAD_SAMPLE_SEARCH_PX {
        for ddx in -PAYLOAD_SAMPLE_SEARCH_PX..=PAYLOAD_SAMPLE_SEARCH_PX {
            if ddx == 0 && ddy == 0 {
                continue;
            }
            let g = Geom { dx: geom.dx + ddx as f64, dy: geom.dy + ddy as f64, ..geom };
            let xyz = sample_symbol_triplet(frame, bx, by, g);
            let score =
                symbol_confidence_score(xyz.0, &means.y, &means.sigma_y_levels, means.sigma_y) +
                0.6 * symbol_confidence_score(xyz.1, &means.u, &means.sigma_u_levels, means.sigma_u) +
                0.6 * symbol_confidence_score(xyz.2, &means.v, &means.sigma_v_levels, means.sigma_v);
            if score > best_score {
                best_score = score;
                best_xyz = xyz;
            }
        }
    }
    best_xyz
}

fn decode_bits_pam_yuv_llr_from_sampler<F>(
    pcfg: &ProfileCfg,
    means: &Means,
    pos: &[(usize, usize)],
    need_bits: usize,
    mut sample: F,
) -> Option<Vec<f32>>
where
    F: FnMut(usize, usize) -> (f64, f64, f64),
{
    let n = pcfg.bits_per_plane;
    let mut llr = Vec::<f32>::with_capacity(need_bits);
    for &(bx, by) in pos {
        let (yv, uv, vv) = sample(bx, by);
        append_symbol_gray_bit_llrs_maxlog(&mut llr, yv, &means.y, &means.sigma_y_levels, means.sigma_y, n);
        let _ = (uv, vv);
        if llr.len() >= need_bits {
            llr.truncate(need_bits);
            return Some(llr);
        }
    }
    None
}

pub(crate) fn decode_bits_pam_yuv_llr(
    frame: &Yuv420Frame,
    pcfg: &ProfileCfg,
    means: &Means,
    pos: &[(usize, usize)],
    need_bits: usize,
    geom: Geom,
) -> Option<Vec<f32>> {
    decode_bits_pam_yuv_llr_from_sampler(pcfg, means, pos, need_bits, |bx, by| {
        sample_symbol_triplet_best(frame, bx, by, means, geom)
    })
}

pub(crate) fn decode_bits_pam_yuv_llr_cached(
    cache: &AlignedFrameCache,
    pcfg: &ProfileCfg,
    means: &Means,
    pos: &[(usize, usize)],
    need_bits: usize,
) -> Option<Vec<f32>> {
    decode_bits_pam_yuv_llr_from_sampler(pcfg, means, pos, need_bits, |bx, by| cache.avg_symbol_triplet(bx, by))
}

pub(crate) fn draw_cal_strip(frame: &mut Yuv420Frame, width: usize, height: usize, pcfg: &ProfileCfg, layout: &ProfileLayout) {
    let y_levels = pcfg.y_slice();
    let u_levels = pcfg.u_slice();
    let v_levels = pcfg.v_slice();

    for &(bx, by, lvl) in &layout.cal_pos {
        let i = lvl as usize;
        fill_y_block(&mut frame.y, width, Y_BLOCK, bx, by, y_levels[i]);
        fill_uv_block_420(&mut frame.u, width, height, bx, by, Y_BLOCK, u_levels[i]);
        fill_uv_block_420(&mut frame.v, width, height, bx, by, Y_BLOCK, v_levels[i]);
    }
}

// Read CAL means using medians to improve robustness.
fn estimate_sigma_from_levels(samples: &[Vec<f64>], means: &[f64]) -> f64 {
    let mut ss = 0.0f64;
    let mut n = 0usize;
    for (i, arr) in samples.iter().enumerate() {
        let mu = means.get(i).copied().unwrap_or(0.0);
        for &x in arr {
            let d = x - mu;
            ss += d * d;
            n += 1;
        }
    }
    let v = if n > 0 { ss / (n as f64) } else { 0.0 };
    v.sqrt().clamp(1.0, 64.0)
}

fn estimate_sigma_per_level(samples: &[Vec<f64>], means: &[f64], sigma_fallback: f64) -> Vec<f64> {
    let mut out = vec![sigma_fallback; means.len()];
    for (i, arr) in samples.iter().enumerate() {
        if i >= means.len() || arr.is_empty() {
            continue;
        }
        let mu = means[i];
        let mut ss = 0.0f64;
        for &x in arr {
            let d = x - mu;
            ss += d * d;
        }
        let n = arr.len() as f64;
        let s = (ss / n.max(1.0)).sqrt();
        // With low CAL repeats, blend toward the global estimate.
        let w = (arr.len() as f64 / 4.0).clamp(0.0, 1.0);
        out[i] = (w * s + (1.0 - w) * sigma_fallback).clamp(0.8, 80.0);
    }
    out
}

fn isotonic_fit_non_decreasing(xs: &[f64]) -> Vec<f64> {
    if xs.is_empty() {
        return Vec::new();
    }
    let mut vals: Vec<f64> = Vec::new();
    let mut cnts: Vec<usize> = Vec::new();
    for &x in xs {
        vals.push(x);
        cnts.push(1);
        while vals.len() >= 2 {
            let n = vals.len();
            if vals[n - 2] <= vals[n - 1] {
                break;
            }
            let c0 = cnts[n - 2];
            let c1 = cnts[n - 1];
            let merged = (vals[n - 2] * c0 as f64 + vals[n - 1] * c1 as f64) / (c0 + c1) as f64;
            vals[n - 2] = merged;
            cnts[n - 2] = c0 + c1;
            vals.pop();
            cnts.pop();
        }
    }
    let mut out = Vec::with_capacity(xs.len());
    for (v, c) in vals.into_iter().zip(cnts.into_iter()) {
        out.extend(std::iter::repeat_n(v, c));
    }
    out
}

fn read_cal_means_from_sampler<F>(pcfg: &ProfileCfg, layout: &ProfileLayout, mut sample: F) -> Option<Means>
where
    F: FnMut(usize, usize) -> (f64, f64, f64),
{
    let m = pcfg.pam_m;
    let mut y_samples: Vec<Vec<f64>> = vec![Vec::new(); m];
    let mut u_samples: Vec<Vec<f64>> = vec![Vec::new(); m];
    let mut v_samples: Vec<Vec<f64>> = vec![Vec::new(); m];

    for &(bx, by, lvl) in &layout.cal_pos {
        let i = lvl as usize;
        let (y, u, v) = sample(bx, by);
        y_samples[i].push(y);
        u_samples[i].push(u);
        v_samples[i].push(v);
    }

    let mut ym = vec![0.0f64; m];
    let mut um = vec![0.0f64; m];
    let mut vm = vec![0.0f64; m];

    for i in 0..m {
        if y_samples[i].is_empty() {
            return None;
        }
        ym[i] = median_f64(y_samples[i].clone());
        um[i] = median_f64(u_samples[i].clone());
        vm[i] = median_f64(v_samples[i].clone());
        if !ym[i].is_finite() || !um[i].is_finite() || !vm[i].is_finite() {
            return None;
        }
    }

    ym = isotonic_fit_non_decreasing(&ym);
    um = isotonic_fit_non_decreasing(&um);
    vm = isotonic_fit_non_decreasing(&vm);

    let y_span = ym[m - 1] - ym[0];
    let u_span = um[m - 1] - um[0];
    let v_span = vm[m - 1] - vm[0];
    let min_y_span = pcfg.min_gap_y * (m.saturating_sub(1) as f64) * 0.6;
    let min_uv_span = pcfg.min_gap_uv * (m.saturating_sub(1) as f64) * 0.6;
    if y_span < min_y_span || u_span < min_uv_span || v_span < min_uv_span {
        return None;
    }

    let sigma_y = estimate_sigma_from_levels(&y_samples, &ym);
    let sigma_u = estimate_sigma_from_levels(&u_samples, &um);
    let sigma_v = estimate_sigma_from_levels(&v_samples, &vm);
    let sigma_y_levels = estimate_sigma_per_level(&y_samples, &ym, sigma_y);
    let sigma_u_levels = estimate_sigma_per_level(&u_samples, &um, sigma_u);
    let sigma_v_levels = estimate_sigma_per_level(&v_samples, &vm, sigma_v);

    Some(Means {
        y: ym,
        u: um,
        v: vm,
        sigma_y,
        sigma_u,
        sigma_v,
        sigma_y_levels,
        sigma_u_levels,
        sigma_v_levels,
    })
}

pub(crate) fn read_cal_means(frame: &Yuv420Frame, pcfg: &ProfileCfg, layout: &ProfileLayout, geom: Geom) -> Option<Means> {
    read_cal_means_from_sampler(pcfg, layout, |bx, by| {
        (
            avg_y_block_geom(&frame.y, frame.width, frame.height, Y_BLOCK, bx, by, geom),
            avg_uv_block_420_geom(&frame.u, frame.width, frame.height, Y_BLOCK, bx, by, geom),
            avg_uv_block_420_geom(&frame.v, frame.width, frame.height, Y_BLOCK, bx, by, geom),
        )
    })
}

pub(crate) fn read_cal_means_cached(cache: &AlignedFrameCache, pcfg: &ProfileCfg, layout: &ProfileLayout) -> Option<Means> {
    read_cal_means_from_sampler(pcfg, layout, |bx, by| cache.avg_symbol_triplet(bx, by))
}

pub(crate) fn write_payload_interleaved_bits(
    frame: &mut Yuv420Frame,
    width: usize,
    height: usize,
    layout: &ProfileLayout,
    pcfg: &ProfileCfg,
    bits: &[u8],
    frame_idx: u32,
) {
    let n = pcfg.bits_per_plane;
    let y_levels = pcfg.y_slice();
    let frame_cap_bits = payload_capacity_bits_for_frame(layout, pcfg, frame_idx);
    let (a, b) = affine_prp_params(frame_cap_bits, frame_bit_permute_seed(pcfg.profile, frame_idx));
    let mut out_bit_idx = 0usize;

    for &(bx, by) in &layout.payload_pos {
        if !payload_block_is_active(layout, bx, by, frame_idx) {
            continue;
        }
        let mut gy = 0u8;
        for _ in 0..n {
            gy <<= 1;
            let src_idx = affine_prp_map(out_bit_idx, frame_cap_bits, a, b);
            gy |= bits.get(src_idx).copied().unwrap_or(0) & 1;
            out_bit_idx += 1;
        }
        let sy = (gray_to_binary_u8(gy) as usize).min(pcfg.pam_m - 1);
        fill_y_block(&mut frame.y, width, Y_BLOCK, bx, by, y_levels[sy]);
        fill_uv_block_420(&mut frame.u, width, height, bx, by, Y_BLOCK, 128);
        fill_uv_block_420(&mut frame.v, width, height, bx, by, Y_BLOCK, 128);
    }
}

fn read_payload_interleaved_llrs_from_sampler<F>(
    means: &Means,
    layout: &ProfileLayout,
    pcfg: &ProfileCfg,
    need_bits: usize,
    frame_idx: u32,
    mut sample_y: F,
) -> Option<Vec<f32>>
where
    F: FnMut(usize, usize) -> f64,
{
    let mut out = Vec::new();
    let mut raw = Vec::new();
    read_payload_interleaved_llrs_from_sampler_into(
        means,
        layout,
        pcfg,
        need_bits,
        frame_idx,
        &mut out,
        &mut raw,
        &mut sample_y,
    )?;
    Some(out)
}

fn read_payload_interleaved_llrs_from_sampler_into<F>(
    means: &Means,
    layout: &ProfileLayout,
    pcfg: &ProfileCfg,
    need_bits: usize,
    frame_idx: u32,
    out: &mut Vec<f32>,
    raw: &mut Vec<f32>,
    sample_y: &mut F,
) -> Option<()>
where
    F: FnMut(usize, usize) -> f64,
{
    let n = pcfg.bits_per_plane;
    let frame_cap_bits = payload_capacity_bits_for_frame(layout, pcfg, frame_idx);
    if need_bits > frame_cap_bits {
        return None;
    }
    let (a, b) = affine_prp_params(frame_cap_bits, frame_bit_permute_seed(pcfg.profile, frame_idx));
    raw.clear();
    if raw.capacity() < frame_cap_bits {
        raw.reserve(frame_cap_bits - raw.capacity());
    }
    for &(bx, by) in &layout.payload_pos {
        if !payload_block_is_active(layout, bx, by, frame_idx) {
            continue;
        }
        let yv = sample_y(bx, by);
        append_symbol_gray_bit_llrs_maxlog(raw, yv, &means.y, &means.sigma_y_levels, means.sigma_y, n);
    }
    if raw.len() < frame_cap_bits {
        return None;
    }
    // Fix: use frame_cap_bits as the PRP modulus (must match encoder).
    // The encoder writes: frame_pos[i] = bits[P(i)] where P(i) = affine_prp_map(i, frame_cap_bits, a, b).
    // So raw[i] is the LLR for bits[P(i)]; we place it at out[P(i)].
    out.clear();
    out.resize(need_bits, 0.0);
    for i in 0..frame_cap_bits {
        let src_idx = affine_prp_map(i, frame_cap_bits, a, b);
        if src_idx < need_bits {
            out[src_idx] = raw[i];
        }
    }
    Some(())
}

pub(crate) fn read_payload_interleaved_llrs(
    frame: &Yuv420Frame,
    means: &Means,
    layout: &ProfileLayout,
    pcfg: &ProfileCfg,
    need_bits: usize,
    frame_idx: u32,
    geom: Geom,
) -> Option<Vec<f32>> {
    read_payload_interleaved_llrs_from_sampler(means, layout, pcfg, need_bits, frame_idx, |bx, by| {
        let (yv, _, _) = sample_symbol_triplet_best(frame, bx, by, means, geom);
        yv
    })
}

pub(crate) fn read_payload_interleaved_llrs_cached(
    cache: &AlignedFrameCache,
    means: &Means,
    layout: &ProfileLayout,
    pcfg: &ProfileCfg,
    need_bits: usize,
    frame_idx: u32,
) -> Option<Vec<f32>> {
    read_payload_interleaved_llrs_from_sampler(means, layout, pcfg, need_bits, frame_idx, |bx, by| {
        let (yv, _, _) = cache.avg_symbol_triplet(bx, by);
        yv
    })
}

pub(crate) fn read_payload_interleaved_llrs_cached_into(
    cache: &AlignedFrameCache,
    means: &Means,
    layout: &ProfileLayout,
    pcfg: &ProfileCfg,
    need_bits: usize,
    frame_idx: u32,
    out: &mut Vec<f32>,
    raw: &mut Vec<f32>,
) -> Option<()> {
    let mut sample = |bx, by| {
        let (yv, _, _) = cache.avg_symbol_triplet(bx, by);
        yv
    };
    read_payload_interleaved_llrs_from_sampler_into(means, layout, pcfg, need_bits, frame_idx, out, raw, &mut sample)
}

pub(crate) fn read_payload_interleaved_llrs_luma_cached_into(
    cache: &AlignedLumaCache,
    means: &Means,
    layout: &ProfileLayout,
    pcfg: &ProfileCfg,
    need_bits: usize,
    frame_idx: u32,
    out: &mut Vec<f32>,
    raw: &mut Vec<f32>,
) -> Option<()> {
    let mut sample = |bx, by| cache.avg_y_block(bx, by);
    read_payload_interleaved_llrs_from_sampler_into(means, layout, pcfg, need_bits, frame_idx, out, raw, &mut sample)
}
