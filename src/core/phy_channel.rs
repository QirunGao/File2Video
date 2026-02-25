use anyhow::{Result, bail};

use crate::params::{MARGIN_BLOCKS, ProfileCfg, Y_BLOCK};

const LOCATOR_BLOCKS: usize = 4;
const PILOT_BLOCKS_TARGET: usize = 12;
const LUMA_MID: u8 = 128;
const LUMA_ZERO: u8 = 224;
const LUMA_ONE: u8 = 32;

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

#[derive(Clone, Debug, Default)]
pub struct PhyDecodeResult {
    pub slots: Vec<SlotLlr>,
    pub frames_total: usize,
    pub frames_synced: usize,
}

#[derive(Clone, Debug)]
struct FrameLayout {
    width: usize,
    height: usize,
    bx_n: usize,
    by_n: usize,
    frame_bytes: usize,
    pilot_blocks: Vec<(usize, usize)>,
    slot_blocks: Vec<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, Default)]
struct BlockOffset {
    dx: isize,
    dy: isize,
}

fn apply_block_offset(
    bx: usize,
    by: usize,
    bx_n: usize,
    by_n: usize,
    offset: BlockOffset,
) -> Option<(usize, usize)> {
    let x = bx as isize + offset.dx;
    let y = by as isize + offset.dy;
    if x < 0 || y < 0 || x >= bx_n as isize || y >= by_n as isize {
        return None;
    }
    Some((x as usize, y as usize))
}

fn frame_layout(width: usize, height: usize, profile: ProfileCfg) -> Result<FrameLayout> {
    if width % Y_BLOCK != 0 || height % Y_BLOCK != 0 {
        bail!("width/height must be multiples of {}", Y_BLOCK);
    }
    let bx_n = width / Y_BLOCK;
    let by_n = height / Y_BLOCK;
    if bx_n <= 2 * MARGIN_BLOCKS + 2 || by_n <= 2 * MARGIN_BLOCKS + 2 {
        bail!(
            "frame too small for locator/pilot/slot layout: {}x{}",
            width,
            height
        );
    }
    if profile.coded_bits_per_slot > Y_BLOCK * Y_BLOCK {
        bail!(
            "coded_bits_per_slot {} exceeds 32x32 luma capacity {}",
            profile.coded_bits_per_slot,
            Y_BLOCK * Y_BLOCK
        );
    }

    let mut used = vec![false; bx_n * by_n];

    let locator_blocks = locator_coords(bx_n, by_n);
    for &(bx, by) in &locator_blocks {
        used[by * bx_n + bx] = true;
    }

    let mut pilot_blocks = Vec::with_capacity(PILOT_BLOCKS_TARGET);
    for (bx, by) in perimeter_scan_blocks(bx_n, by_n) {
        if pilot_blocks.len() >= PILOT_BLOCKS_TARGET {
            break;
        }
        if !used[by * bx_n + bx] {
            used[by * bx_n + bx] = true;
            pilot_blocks.push((bx, by));
        }
    }
    if pilot_blocks.is_empty() {
        bail!("no pilot blocks available in frame layout");
    }

    let mut slot_blocks = Vec::with_capacity(profile.slots_per_frame);
    for by in MARGIN_BLOCKS..(by_n - MARGIN_BLOCKS) {
        for bx in MARGIN_BLOCKS..(bx_n - MARGIN_BLOCKS) {
            if used[by * bx_n + bx] {
                continue;
            }
            slot_blocks.push((bx, by));
            if slot_blocks.len() >= profile.slots_per_frame {
                break;
            }
        }
        if slot_blocks.len() >= profile.slots_per_frame {
            break;
        }
    }
    if slot_blocks.len() < profile.slots_per_frame {
        bail!(
            "not enough slot blocks: need {}, got {}",
            profile.slots_per_frame,
            slot_blocks.len()
        );
    }

    let y_bytes = width * height;
    let uv_bytes = y_bytes / 4;
    Ok(FrameLayout {
        width,
        height,
        bx_n,
        by_n,
        frame_bytes: y_bytes + uv_bytes * 2,
        pilot_blocks,
        slot_blocks,
    })
}

fn locator_coords(bx_n: usize, by_n: usize) -> [(usize, usize); LOCATOR_BLOCKS] {
    let m = MARGIN_BLOCKS;
    [
        (m, m),
        (bx_n - 1 - m, m),
        (m, by_n - 1 - m),
        (bx_n - 1 - m, by_n - 1 - m),
    ]
}

fn perimeter_scan_blocks(bx_n: usize, by_n: usize) -> Vec<(usize, usize)> {
    let m = MARGIN_BLOCKS;
    let mut out = Vec::new();
    let x0 = m;
    let x1 = bx_n - 1 - m;
    let y0 = m;
    let y1 = by_n - 1 - m;

    for bx in x0..=x1 {
        out.push((bx, y0));
    }
    if y1 > y0 {
        for by in (y0 + 1)..=y1 {
            out.push((x1, by));
        }
    }
    if y1 > y0 {
        for bx in (x0..x1).rev() {
            out.push((bx, y1));
        }
    }
    if x1 > x0 && y1 > y0 + 1 {
        for by in ((y0 + 1)..y1).rev() {
            out.push((x0, by));
        }
    }
    out
}

fn set_luma_pixel(y_plane: &mut [u8], width: usize, x: usize, y: usize, v: u8) {
    y_plane[y * width + x] = v;
}

fn fill_block(
    y_plane: &mut [u8],
    width: usize,
    bx: usize,
    by: usize,
    f: impl Fn(usize, usize) -> u8,
) {
    let x0 = bx * Y_BLOCK;
    let y0 = by * Y_BLOCK;
    for ly in 0..Y_BLOCK {
        for lx in 0..Y_BLOCK {
            set_luma_pixel(y_plane, width, x0 + lx, y0 + ly, f(lx, ly));
        }
    }
}

fn locator_luma(corner_idx: usize, lx: usize, ly: usize) -> u8 {
    let cell = 4usize;
    let mut bit = ((lx / cell) + (ly / cell)) & 1;
    bit ^= corner_idx & 1;
    if (8..24).contains(&lx) && (8..24).contains(&ly) {
        bit ^= 1;
    }
    if bit == 0 { 240 } else { 16 }
}

fn pilot_luma(pilot_idx: usize, lx: usize, ly: usize) -> u8 {
    let stripe = 4usize;
    let bit = (((lx / stripe) + pilot_idx) ^ (ly / (stripe * 2))) & 1;
    if bit == 0 { 216 } else { 40 }
}

fn block_match_score(
    y_plane: &[u8],
    width: usize,
    bx: usize,
    by: usize,
    expected: impl Fn(usize, usize) -> u8,
) -> f32 {
    let x0 = bx * Y_BLOCK;
    let y0 = by * Y_BLOCK;
    let mut sum_abs = 0f32;
    for ly in 0..Y_BLOCK {
        for lx in 0..Y_BLOCK {
            let got = y_plane[(y0 + ly) * width + (x0 + lx)] as f32;
            let exp = expected(lx, ly) as f32;
            sum_abs += (got - exp).abs();
        }
    }
    let denom = (Y_BLOCK * Y_BLOCK) as f32 * 255.0;
    (1.0 - (sum_abs / denom)).clamp(0.0, 1.0)
}

fn block_match_score_with_offset(
    y_plane: &[u8],
    width: usize,
    layout: &FrameLayout,
    bx: usize,
    by: usize,
    offset: BlockOffset,
    expected: impl Fn(usize, usize) -> u8,
) -> f32 {
    let Some((sx, sy)) = apply_block_offset(bx, by, layout.bx_n, layout.by_n, offset) else {
        return 0.0;
    };
    block_match_score(y_plane, width, sx, sy, expected)
}

fn zero_slot_llr(profile: ProfileCfg) -> SlotLlr {
    SlotLlr {
        sys_llr: vec![0.0; profile.b_sys],
        par_llr: vec![0.0; profile.b_par],
        q_pilot: 0.0,
    }
}

fn slot_bit_luma(bit: u8) -> u8 {
    if bit & 1 == 0 { LUMA_ZERO } else { LUMA_ONE }
}

fn slot_llr_from_luma(v: u8, pilot_score: f32) -> f32 {
    let gain = (0.5 + pilot_score).clamp(0.25, 1.5);
    ((v as f32) - (LUMA_MID as f32)) / 16.0 * gain
}

fn draw_slot_bits(
    y_plane: &mut [u8],
    width: usize,
    bx: usize,
    by: usize,
    slot: Option<&SlotPayloadBits>,
    profile: ProfileCfg,
) {
    fill_block(y_plane, width, bx, by, |_, _| LUMA_MID);
    let Some(slot) = slot else {
        return;
    };
    let x0 = bx * Y_BLOCK;
    let y0 = by * Y_BLOCK;
    let mut i = 0usize;
    for &bit in &slot.sys_bits {
        let lx = i % Y_BLOCK;
        let ly = i / Y_BLOCK;
        set_luma_pixel(y_plane, width, x0 + lx, y0 + ly, slot_bit_luma(bit));
        i += 1;
    }
    for &bit in &slot.par_bits {
        if i >= profile.coded_bits_per_slot {
            break;
        }
        let lx = i % Y_BLOCK;
        let ly = i / Y_BLOCK;
        set_luma_pixel(y_plane, width, x0 + lx, y0 + ly, slot_bit_luma(bit));
        i += 1;
    }
}

fn extract_slot_llr(
    y_plane: &[u8],
    width: usize,
    bx: usize,
    by: usize,
    profile: ProfileCfg,
    q_pilot: f32,
) -> SlotLlr {
    let x0 = bx * Y_BLOCK;
    let y0 = by * Y_BLOCK;
    let mut sys_llr = vec![0f32; profile.b_sys];
    let mut par_llr = vec![0f32; profile.b_par];
    for (i, dst) in sys_llr.iter_mut().enumerate() {
        let lx = i % Y_BLOCK;
        let ly = i / Y_BLOCK;
        let v = y_plane[(y0 + ly) * width + (x0 + lx)];
        *dst = slot_llr_from_luma(v, q_pilot);
    }
    for (j, dst) in par_llr.iter_mut().enumerate() {
        let i = profile.b_sys + j;
        let lx = i % Y_BLOCK;
        let ly = i / Y_BLOCK;
        let v = y_plane[(y0 + ly) * width + (x0 + lx)];
        *dst = slot_llr_from_luma(v, q_pilot);
    }
    SlotLlr {
        sys_llr,
        par_llr,
        q_pilot,
    }
}

fn locator_score_for_offset(
    y_plane: &[u8],
    width: usize,
    layout: &FrameLayout,
    offset: BlockOffset,
) -> f32 {
    let mut locator_score = 0f32;
    for (corner_idx, &(bx, by)) in locator_coords(layout.bx_n, layout.by_n).iter().enumerate() {
        locator_score += block_match_score_with_offset(y_plane, width, layout, bx, by, offset, |lx, ly| {
            locator_luma(corner_idx, lx, ly)
        });
    }
    locator_score / LOCATOR_BLOCKS as f32
}

fn pilot_score_for_offset(
    y_plane: &[u8],
    width: usize,
    layout: &FrameLayout,
    offset: BlockOffset,
) -> f32 {
    let mut pilot_score = 0f32;
    for (pilot_idx, &(bx, by)) in layout.pilot_blocks.iter().enumerate() {
        pilot_score += block_match_score_with_offset(y_plane, width, layout, bx, by, offset, |lx, ly| {
            pilot_luma(pilot_idx, lx, ly)
        });
    }
    pilot_score / layout.pilot_blocks.len() as f32
}

fn find_block_offset(y_plane: &[u8], width: usize, layout: &FrameLayout, heavy_geom: bool) -> BlockOffset {
    if !heavy_geom {
        return BlockOffset::default();
    }

    let mut best_offset = BlockOffset::default();
    let mut best_score = -1.0f32;
    for dy in -1..=1 {
        for dx in -1..=1 {
            let offset = BlockOffset { dx, dy };
            let score = locator_score_for_offset(y_plane, width, layout, offset);
            if score > best_score {
                best_score = score;
                best_offset = offset;
            }
        }
    }
    best_offset
}

pub fn pack_slots_to_yuv420(
    slots: &[SlotPayloadBits],
    width: usize,
    height: usize,
    profile: ProfileCfg,
) -> Result<Vec<u8>> {
    let layout = frame_layout(width, height, profile)?;
    let frame_count = slots.len().div_ceil(profile.slots_per_frame).max(1);
    let mut out = Vec::<u8>::with_capacity(frame_count * layout.frame_bytes);

    for frame_idx in 0..frame_count {
        let mut frame = vec![LUMA_MID; layout.frame_bytes];
        let y_len = width * height;
        let y_plane = &mut frame[..y_len];

        for (corner_idx, &(bx, by)) in locator_coords(layout.bx_n, layout.by_n).iter().enumerate() {
            fill_block(y_plane, width, bx, by, |lx, ly| {
                locator_luma(corner_idx, lx, ly)
            });
        }
        for (pilot_idx, &(bx, by)) in layout.pilot_blocks.iter().enumerate() {
            fill_block(y_plane, width, bx, by, |lx, ly| {
                pilot_luma(pilot_idx, lx, ly)
            });
        }

        let base_slot = frame_idx * profile.slots_per_frame;
        for (slot_i, &(bx, by)) in layout.slot_blocks.iter().enumerate() {
            let src = slots.get(base_slot + slot_i);
            draw_slot_bits(y_plane, width, bx, by, src, profile);
        }

        out.extend_from_slice(&frame);
    }
    Ok(out)
}

pub fn unpack_yuv420_to_slots(
    bytes: &[u8],
    width: usize,
    height: usize,
    profile: ProfileCfg,
    heavy_geom: bool,
) -> Result<PhyDecodeResult> {
    let layout = frame_layout(width, height, profile)?;
    if bytes.len() % layout.frame_bytes != 0 {
        bail!(
            "raw yuv size {} is not multiple of frame bytes {} ({}x{} yuv420p)",
            bytes.len(),
            layout.frame_bytes,
            layout.width,
            layout.height
        );
    }
    let frame_count = bytes.len() / layout.frame_bytes;
    let mut result = PhyDecodeResult {
        slots: Vec::with_capacity(frame_count * profile.slots_per_frame),
        frames_total: frame_count,
        frames_synced: 0,
    };
    let y_len = width * height;
    let locator_threshold = if heavy_geom { 0.55 } else { 0.70 };

    for frame_idx in 0..frame_count {
        let frame = &bytes[frame_idx * layout.frame_bytes..(frame_idx + 1) * layout.frame_bytes];
        let y_plane = &frame[..y_len];
        let block_offset = find_block_offset(y_plane, width, &layout, heavy_geom);
        let locator_score = locator_score_for_offset(y_plane, width, &layout, block_offset);
        let pilot_score = pilot_score_for_offset(y_plane, width, &layout, block_offset);

        let synced = locator_score >= locator_threshold;
        if synced {
            result.frames_synced += 1;
        }
        let q_frame = if synced {
            (0.5 * locator_score + 0.5 * pilot_score).clamp(0.0, 1.0)
        } else {
            0.0
        };

        for &(bx, by) in &layout.slot_blocks {
            if synced {
                if let Some((sx, sy)) =
                    apply_block_offset(bx, by, layout.bx_n, layout.by_n, block_offset)
                {
                    result
                        .slots
                        .push(extract_slot_llr(y_plane, width, sx, sy, profile, q_frame));
                } else {
                    result.slots.push(zero_slot_llr(profile));
                }
            } else {
                result.slots.push(zero_slot_llr(profile));
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::cfg;

    fn mk_slot(profile: ProfileCfg, seed: usize) -> SlotPayloadBits {
        let mut sys_bits = vec![0u8; profile.b_sys];
        let mut par_bits = vec![0u8; profile.b_par];
        for (i, b) in sys_bits.iter_mut().enumerate() {
            *b = ((i + seed) % 2) as u8;
        }
        for (i, b) in par_bits.iter_mut().enumerate() {
            *b = (((i * 3) + seed) % 2) as u8;
        }
        SlotPayloadBits { sys_bits, par_bits }
    }

    fn shift_first_frame_luma_by_blocks(yuv: &mut [u8], width: usize, height: usize, dx: isize, dy: isize) {
        let y_len = width * height;
        let src = yuv[..y_len].to_vec();
        yuv[..y_len].fill(LUMA_MID);
        for by in 0..(height / Y_BLOCK) {
            for bx in 0..(width / Y_BLOCK) {
                let sx = bx as isize - dx;
                let sy = by as isize - dy;
                if sx < 0 || sy < 0 || sx >= (width / Y_BLOCK) as isize || sy >= (height / Y_BLOCK) as isize {
                    continue;
                }
                let sx = sx as usize;
                let sy = sy as usize;
                for ly in 0..Y_BLOCK {
                    let dst_y = by * Y_BLOCK + ly;
                    let src_y = sy * Y_BLOCK + ly;
                    let dst_row = dst_y * width;
                    let src_row = src_y * width;
                    let dst_x = bx * Y_BLOCK;
                    let src_x = sx * Y_BLOCK;
                    yuv[dst_row + dst_x..dst_row + dst_x + Y_BLOCK]
                        .copy_from_slice(&src[src_row + src_x..src_row + src_x + Y_BLOCK]);
                }
            }
        }
    }

    #[test]
    fn yuv_slot_roundtrip_clean_frame() {
        let profile = cfg(0).unwrap();
        let slots = (0..10).map(|i| mk_slot(profile, i)).collect::<Vec<_>>();
        let yuv = pack_slots_to_yuv420(&slots, 1920, 1088, profile).unwrap();
        let decoded = unpack_yuv420_to_slots(&yuv, 1920, 1088, profile, false).unwrap();
        assert_eq!(decoded.frames_total, 1);
        assert_eq!(decoded.frames_synced, 1);
        for i in 0..slots.len() {
            let got = &decoded.slots[i];
            assert!(got.q_pilot > 0.8);
            let hard_sys = got
                .sys_llr
                .iter()
                .map(|&v| if v < 0.0 { 1 } else { 0 })
                .collect::<Vec<_>>();
            let hard_par = got
                .par_llr
                .iter()
                .map(|&v| if v < 0.0 { 1 } else { 0 })
                .collect::<Vec<_>>();
            assert_eq!(hard_sys, slots[i].sys_bits);
            assert_eq!(hard_par, slots[i].par_bits);
        }
    }

    #[test]
    fn broken_locator_causes_frame_erasure() {
        let profile = cfg(0).unwrap();
        let slots = (0..(profile.slots_per_frame + 1))
            .map(|i| mk_slot(profile, i))
            .collect::<Vec<_>>();
        let mut yuv = pack_slots_to_yuv420(&slots, 1920, 1088, profile).unwrap();
        let layout = frame_layout(1920, 1088, profile).unwrap();
        let y_plane_len = 1920 * 1088;
        for (bx, by) in locator_coords(layout.bx_n, layout.by_n) {
            let x0 = bx * Y_BLOCK;
            let y0 = by * Y_BLOCK;
            for ly in 0..Y_BLOCK {
                for lx in 0..Y_BLOCK {
                    yuv[(y0 + ly) * 1920 + (x0 + lx)] = 127;
                }
            }
        }
        let decoded = unpack_yuv420_to_slots(
            &yuv[..(layout.frame_bytes * 2).min(yuv.len())],
            1920,
            1088,
            profile,
            false,
        )
        .unwrap();
        assert_eq!(decoded.frames_total, 2);
        assert_eq!(decoded.frames_synced, 1);
        assert!(decoded.slots[0].q_pilot <= 0.01);
        assert!(decoded.slots[profile.slots_per_frame].q_pilot > 0.8);
        let _ = y_plane_len;
    }

    #[test]
    fn heavy_geom_recovers_block_shifted_frame() {
        let profile = cfg(0).unwrap();
        let slots = (0..32).map(|i| mk_slot(profile, i)).collect::<Vec<_>>();
        let mut yuv = pack_slots_to_yuv420(&slots, 1920, 1088, profile).unwrap();
        shift_first_frame_luma_by_blocks(&mut yuv, 1920, 1088, 1, 0);

        let fast = unpack_yuv420_to_slots(&yuv, 1920, 1088, profile, false).unwrap();
        let heavy = unpack_yuv420_to_slots(&yuv, 1920, 1088, profile, true).unwrap();

        assert_eq!(fast.frames_total, 1);
        assert_eq!(heavy.frames_total, 1);
        assert_eq!(fast.frames_synced, 0);
        assert_eq!(heavy.frames_synced, 1);
    }
}
