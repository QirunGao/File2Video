use crate::params::*;
use anyhow::{bail, Context, Result};
use crc32fast::Hasher;
use std::collections::HashSet;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;

#[path = "encoder.rs"]
pub(crate) mod encoder;
#[path = "decoder.rs"]
pub(crate) mod decoder;
#[path = "geom.rs"]
mod geom;
#[path = "heavygeom.rs"]
mod heavygeom;
mod llr;
mod protocol;
mod sampling;
mod pam;
use self::geom::Geom;
use self::heavygeom::{GeomEstimate, GeomFusionState, GeomSearchHint, GeometryCorrector, LocatorThresholds};
use self::llr::*;
use self::protocol::*;
use self::pam::*;
use self::sampling::*;

/* ----------------------------- RNG / shuffle ------------------------------ */

#[derive(Clone, Debug)]
struct SplitMix64 {
    state: u64,
}
impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        let mut z = self.state.wrapping_add(0x9E3779B97F4A7C15);
        self.state = z;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn gen_range_usize(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            0
        } else {
            (self.next_u64() % (upper as u64)) as usize
        }
    }
}

fn mix_seed(seed: u64, id: u32) -> u64 {
    let x = seed ^ (id as u64).wrapping_mul(0xD6E8FEB86659FD93);
    let mut sm = SplitMix64::new(x);
    sm.next_u64()
}

fn shuffle_in_place<T>(v: &mut [T], rng: &mut SplitMix64) {
    // Fisher-Yates
    for i in (1..v.len()).rev() {
        let j = rng.gen_range_usize(i + 1);
        v.swap(i, j);
    }
}

/* ----------------------------- Header encoding ---------------------------- */

#[derive(Clone)]
struct RsCodec {
    nsym: usize,
    exp: [u8; 512],
    log: [u8; 256],
    generator: Vec<u8>,
}

impl RsCodec {
    fn new(nsym: usize) -> Self {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if (x & 0x100) != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        let mut rs = Self { nsym, exp, log, generator: vec![1] };
        rs.generator = rs.make_generator();
        rs
    }

    fn pow_alpha(&self, p: usize) -> u8 {
        self.exp[p % 255]
    }
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }
    fn inv(&self, a: u8) -> u8 {
        self.exp[(255 - self.log[a as usize] as usize) % 255]
    }
    fn poly_add(&self, a: &[u8], b: &[u8]) -> Vec<u8> {
        let n = a.len().max(b.len());
        let mut out = vec![0u8; n];
        for i in 0..n {
            let ai = if i + a.len() >= n { a[i + a.len() - n] } else { 0 };
            let bi = if i + b.len() >= n { b[i + b.len() - n] } else { 0 };
            out[i] = ai ^ bi;
        }
        // Trim leading zeros without O(n²) repeated remove(0)
        let start = out.iter().position(|&x| x != 0).unwrap_or(out.len().saturating_sub(1));
        if start > 0 {
            out.drain(..start);
        }
        out
    }
    fn poly_scale(&self, p: &[u8], x: u8) -> Vec<u8> {
        p.iter().map(|&v| self.mul(v, x)).collect()
    }
    fn poly_mul(&self, p: &[u8], q: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; p.len() + q.len() - 1];
        for (i, &pv) in p.iter().enumerate() {
            if pv == 0 {
                continue;
            }
            for (j, &qv) in q.iter().enumerate() {
                if qv == 0 {
                    continue;
                }
                out[i + j] ^= self.mul(pv, qv);
            }
        }
        out
    }
    fn poly_eval(&self, p: &[u8], x: u8) -> u8 {
        let mut y = 0u8;
        for &c in p {
            y = self.mul(y, x) ^ c;
        }
        y
    }
    fn make_generator(&self) -> Vec<u8> {
        let mut g = vec![1u8];
        for i in 0..self.nsym {
            g = self.poly_mul(&g, &[1, self.pow_alpha(i)]);
        }
        g
    }
    fn encode(&self, msg: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; msg.len() + self.nsym];
        buf[..msg.len()].copy_from_slice(msg);
        for i in 0..msg.len() {
            let coef = buf[i];
            if coef == 0 {
                continue;
            }
            for j in 1..self.generator.len() {
                buf[i + j] ^= self.mul(self.generator[j], coef);
            }
        }
        let mut out = Vec::with_capacity(msg.len() + self.nsym);
        out.extend_from_slice(msg);
        out.extend_from_slice(&buf[msg.len()..]);
        out
    }
    fn syndromes(&self, code: &[u8]) -> Vec<u8> {
        (0..self.nsym).map(|i| self.poly_eval(code, self.pow_alpha(i))).collect()
    }
    fn berlekamp_massey(&self, synd: &[u8]) -> Option<Vec<u8>> {
        let mut err_loc = vec![1u8];
        let mut old_loc = vec![1u8];
        for i in 0..synd.len() {
            old_loc.push(0);
            let mut delta = synd[i];
            for j in 1..err_loc.len() {
                if i >= j {
                    delta ^= self.mul(err_loc[err_loc.len() - 1 - j], synd[i - j]);
                }
            }
            if delta != 0 {
                if old_loc.len() > err_loc.len() {
                    let new_loc = self.poly_scale(&old_loc, delta);
                    old_loc = self.poly_scale(&err_loc, self.inv(delta));
                    err_loc = new_loc;
                }
                err_loc = self.poly_add(&err_loc, &self.poly_scale(&old_loc, delta));
            }
        }
        while err_loc.len() > 1 && err_loc[0] == 0 {
            err_loc.remove(0);
        }
        let errs = err_loc.len().saturating_sub(1);
        if errs == 0 || errs * 2 > self.nsym {
            return None;
        }
        Some(err_loc)
    }
    fn find_error_positions(&self, err_loc: &[u8], nmsg: usize) -> Option<Vec<usize>> {
        let errs = err_loc.len().saturating_sub(1);
        let mut pos = Vec::with_capacity(errs);
        for i in 0..nmsg {
            if self.poly_eval(err_loc, self.pow_alpha(i)) == 0 {
                pos.push(nmsg - 1 - i);
            }
        }
        if pos.len() == errs { Some(pos) } else { None }
    }
    fn solve_linear(&self, mut a: Vec<Vec<u8>>, mut b: Vec<u8>) -> Option<Vec<u8>> {
        let n = a.len();
        if n == 0 || b.len() != n || a.iter().any(|r| r.len() != n) {
            return None;
        }
        for col in 0..n {
            let mut piv = col;
            while piv < n && a[piv][col] == 0 {
                piv += 1;
            }
            if piv >= n {
                return None;
            }
            if piv != col {
                a.swap(piv, col);
                b.swap(piv, col);
            }
            let inv = self.inv(a[col][col]);
            for c in col..n {
                a[col][c] = self.mul(a[col][c], inv);
            }
            b[col] = self.mul(b[col], inv);
            for r in 0..n {
                if r == col || a[r][col] == 0 {
                    continue;
                }
                let f = a[r][col];
                for c in col..n {
                    a[r][c] ^= self.mul(f, a[col][c]);
                }
                b[r] ^= self.mul(f, b[col]);
            }
        }
        Some(b)
    }
    fn decode(&self, code: &[u8]) -> Option<Vec<u8>> {
        if code.len() < self.nsym {
            return None;
        }
        let mut out = code.to_vec();
        let synd = self.syndromes(&out);
        if synd.iter().all(|&s| s == 0) {
            return Some(out[..out.len() - self.nsym].to_vec());
        }
        let err_loc = self.berlekamp_massey(&synd)?;
        let err_pos = self.find_error_positions(&err_loc, out.len())?;
        let t = err_pos.len();
        if t == 0 || t * 2 > self.nsym {
            return None;
        }
        let mut mat = vec![vec![0u8; t]; t];
        let mut rhs = vec![0u8; t];
        for row in 0..t {
            rhs[row] = synd[row];
            for (ci, &pos) in err_pos.iter().enumerate() {
                let coef = out.len() - 1 - pos;
                mat[row][ci] = if row == 0 { 1 } else { self.pow_alpha((coef * row) % 255) };
            }
        }
        let mags = self.solve_linear(mat, rhs)?;
        for (&pos, &mag) in err_pos.iter().zip(mags.iter()) {
            out[pos] ^= mag;
        }
        let synd2 = self.syndromes(&out);
        if synd2.iter().any(|&s| s != 0) {
            return None;
        }
        Some(out[..out.len() - self.nsym].to_vec())
    }
}

fn boot_rs_codec() -> &'static RsCodec {
    static BOOT_RS: OnceLock<RsCodec> = OnceLock::new();
    BOOT_RS.get_or_init(|| RsCodec::new(BOOT_RS_PARITY))
}

fn boot_encode_bits_rs(msg: &[u8; BOOT_MSG_LEN]) -> Vec<u8> {
    let code = boot_rs_codec().encode(msg);
    let raw_bits = bytes_to_bits_msb(&code);
    let mut out = Vec::<u8>::with_capacity(BOOT_CODE_BITS);
    for bit in raw_bits {
        for _ in 0..BOOT_RS_REPEAT {
            out.push(bit);
        }
    }
    out
}

fn boot_decode_msg_from_llr(llr: &[f32]) -> Option<[u8; BOOT_MSG_LEN]> {
    if llr.len() < BOOT_CODE_BITS {
        return None;
    }
    let mut bits = Vec::<u8>::with_capacity(BOOT_RS_N * 8);
    for i in 0..(BOOT_RS_N * 8) {
        let mut acc = 0f32;
        for r in 0..BOOT_RS_REPEAT {
            acc += llr[i * BOOT_RS_REPEAT + r];
        }
        bits.push(if acc < 0.0 { 1 } else { 0 });
    }
    let code = bits_to_bytes_msb(&bits);
    let msg = boot_rs_codec().decode(&code)?;
    if msg.len() != BOOT_MSG_LEN {
        return None;
    }
    let mut out = [0u8; BOOT_MSG_LEN];
    out.copy_from_slice(&msg);
    Some(out)
}

fn make_boot(profile: u8, frame_idx: u32) -> [u8; BOOT_LEN] {
    let mut msg = [0u8; BOOT_MSG_LEN];
    msg[0..2].copy_from_slice(MAGIC);
    msg[2] = VERSION;
    msg[3] = profile;
    msg[4..8].copy_from_slice(&frame_idx.to_be_bytes());
    let crc = crc32(&msg[..BOOT_MSG_LEN - 4]);
    msg[BOOT_MSG_LEN - 4..].copy_from_slice(&crc.to_be_bytes());
    let bytes = bits_to_bytes_msb(&boot_encode_bits_rs(&msg));
    let mut out = [0u8; BOOT_LEN];
    out.copy_from_slice(&bytes[..BOOT_LEN]);
    out
}

fn parse_boot_msg(bytes: &[u8]) -> Option<(u8, u32)> {
    if bytes.len() != BOOT_MSG_LEN {
        return None;
    }
    if &bytes[0..2] != MAGIC || bytes[2] != VERSION {
        return None;
    }
    let got = u32::from_be_bytes(bytes[BOOT_MSG_LEN - 4..].try_into().ok()?);
    let exp = crc32(&bytes[..BOOT_MSG_LEN - 4]);
    if got != exp {
        return None;
    }
    let profile = bytes[3];
    let frame_idx = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    Some((profile, frame_idx))
}

/* ---------------------------------- Layout -------------------------------- */

#[derive(Clone)]
struct BootLayout {
    copies: Vec<Vec<(usize, usize)>>,
}

#[derive(Clone)]
struct ProfileLayout {
    bx_n: usize,
    _by_n: usize,
    _header_by_start: usize,
    _header_by_end: usize,
    payload_by_start: usize,
    payload_by_end: usize,

    cal_pos: Vec<(usize, usize, u8)>,      // (bx,by,level_idx)
    payload_pos: Vec<(usize, usize)>,
}

const BOOT_CAL_KEYFRAME_INTERVAL: u32 = 8;

fn frame_uses_boot_cal(frame_idx: u32) -> bool {
    frame_idx % BOOT_CAL_KEYFRAME_INTERVAL == 0
}

fn build_boot_layout(width: usize, height: usize, boot_copies: usize, cal_reserved_blocks: usize) -> Result<BootLayout> {
    let bx_n = width / Y_BLOCK;
    let by_n = height / Y_BLOCK;
    if bx_n == 0 || by_n == 0 { bail!("invalid geometry"); }

    let usable_bx = bx_n.saturating_sub(2 * MARGIN_BLOCKS);
    if usable_bx == 0 { bail!("not enough horizontal blocks after margin"); }
    if by_n <= 2 * MARGIN_BLOCKS + HEADER_ROWS { bail!("not enough vertical blocks for margin+HEADER_ROWS"); }

    let header_by_start = MARGIN_BLOCKS;
    let header_by_end = header_by_start + HEADER_ROWS;

    // BOOT each copy uses BOOT_LEN * 8 blocks (1 bit/block)
    let bits_per_copy = BOOT_LEN * 8;
    let reserve_copies = boot_copies.clamp(1, BOOT_COPIES_MAX);
    let total_need = bits_per_copy * reserve_copies;

    let bx_max = bx_n - MARGIN_BLOCKS - 1;

    let mut pool = Vec::<(usize, usize)>::new();
    for by in header_by_start..header_by_end {
        let row_cal_reserved = if by == header_by_start { cal_reserved_blocks } else { 0 };
        let bx_min = (MARGIN_BLOCKS + row_cal_reserved).min(bx_n.saturating_sub(MARGIN_BLOCKS));
        for bx in (bx_min..=bx_max).rev() {
            pool.push((bx, by));
            if pool.len() >= total_need {
                break;
            }
        }
        if pool.len() >= total_need {
            break;
        }
    }
    if pool.len() < total_need {
        bail!(
            "header too small for boot in top-right area: need {} blocks, have {} (usable_bx={}, header_rows={}, reserve_copies={}, cal_reserved_blocks={})",
            total_need, pool.len(), usable_bx, HEADER_ROWS, reserve_copies, cal_reserved_blocks
        );
    }

    let mut copies = Vec::with_capacity(reserve_copies);
    for c in 0..reserve_copies {
        let s = c * bits_per_copy;
        let e = s + bits_per_copy;
        copies.push(pool[s..e].to_vec());
    }

    Ok(BootLayout { copies })
}

fn build_profile_layout(width: usize, height: usize, boot: &BootLayout, pcfg: &ProfileCfg) -> Result<ProfileLayout> {
    let bx_n = width / Y_BLOCK;
    let by_n = height / Y_BLOCK;

    let usable_bx = bx_n.saturating_sub(2 * MARGIN_BLOCKS);
    if usable_bx == 0 { bail!("not enough horizontal blocks after margin"); }
    if by_n <= 2 * MARGIN_BLOCKS + HEADER_ROWS { bail!("not enough vertical blocks for margin+HEADER_ROWS"); }

    let header_by_start = MARGIN_BLOCKS;
    let header_by_end = header_by_start + HEADER_ROWS;
    let payload_by_start = header_by_end;
    let payload_by_end = by_n - MARGIN_BLOCKS;

    // CAL strip is placed on the first header row, from the left margin.
    let cal_blocks = pcfg.pam_m * pcfg.cal_repeats;
    if cal_blocks == 0 { bail!("cal_blocks == 0"); }
    if cal_blocks > usable_bx {
        bail!("cal strip too long: need {} blocks, usable_bx={}", cal_blocks, usable_bx);
    }
    let cal_row = header_by_start;
    let mut cal_pos = Vec::with_capacity(cal_blocks);
    for i in 0..cal_blocks {
        let bx = MARGIN_BLOCKS + i;
        let lvl = (i / pcfg.cal_repeats) as u8; // 0..pam_m-1
        cal_pos.push((bx, cal_row, lvl));
    }

    // Enforce no overlap among cal / boot regions.
    let mut used = HashSet::<(usize, usize)>::new();
    for &(bx, by, _) in &cal_pos {
        if !used.insert((bx, by)) {
            bail!("layout overlap: CAL");
        }
    }
    for copy in &boot.copies {
        for &(bx, by) in copy {
            if !used.insert((bx, by)) {
                bail!("layout overlap: BOOT with CAL");
            }
        }
    }

    let payload_pos = build_payload_positions_vec(bx_n, payload_by_start, payload_by_end);

    Ok(ProfileLayout {
        bx_n,
        _by_n: by_n,
        _header_by_start: header_by_start,
        _header_by_end: header_by_end,
        payload_by_start, payload_by_end,
        cal_pos,
        payload_pos,
    })
}

fn build_tracking_layout(width: usize, height: usize) -> Result<ProfileLayout> {
    let bx_n = width / Y_BLOCK;
    let by_n = height / Y_BLOCK;
    if bx_n == 0 || by_n == 0 {
        bail!("invalid geometry");
    }
    if bx_n <= 2 * MARGIN_BLOCKS || by_n <= 2 * MARGIN_BLOCKS {
        bail!("not enough blocks for tracking layout margins");
    }
    Ok(ProfileLayout {
        bx_n,
        _by_n: by_n,
        _header_by_start: MARGIN_BLOCKS,
        _header_by_end: MARGIN_BLOCKS,
        payload_by_start: MARGIN_BLOCKS,
        payload_by_end: by_n - MARGIN_BLOCKS,
        cal_pos: Vec::new(),
        payload_pos: build_payload_positions_vec(bx_n, MARGIN_BLOCKS, by_n - MARGIN_BLOCKS),
    })
}

/* ------------------------------- Video Model ------------------------------ */

#[derive(Clone)]
struct Yuv420Frame {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    width: usize,
    height: usize,
}

impl Yuv420Frame {
    fn new(width: usize, height: usize) -> Self {
        let y = vec![0u8; width * height];
        let u = vec![128u8; (width / 2) * (height / 2)];
        let v = vec![128u8; (width / 2) * (height / 2)];
        Self { y, u, v, width, height }
    }
    fn clear(&mut self, y_val: u8, uv_val: u8) {
        self.y.fill(y_val);
        self.u.fill(uv_val);
        self.v.fill(uv_val);
    }
}

/* --------------------------- Gray (general n-bit) --------------------------- */

fn gray_to_binary_u8(mut g: u8) -> u8 {
    let mut b = 0u8;
    while g != 0 {
        b ^= g;
        g >>= 1;
    }
    b
}

fn binary_to_gray_u8(b: u8) -> u8 {
    b ^ (b >> 1)
}

/* -------------------------- PAM{2,4,8} encode/decode ----------------------- */

/* --------------------------------- BOOT IO -------------------------------- */

fn draw_boot_bit(frame: &mut Yuv420Frame, width: usize, height: usize, bx: usize, by: usize, bit: u8) {
    let yv = if bit == 0 { LOC_Y0 } else { LOC_Y1 };
    let uv = if bit == 0 { LOC_U0 } else { LOC_U1 };
    let vv = if bit == 0 { LOC_V0 } else { LOC_V1 };

    fill_y_block(&mut frame.y, width, Y_BLOCK, bx, by, yv);
    fill_uv_block_420(&mut frame.u, width, height, bx, by, Y_BLOCK, uv);
    fill_uv_block_420(&mut frame.v, width, height, bx, by, Y_BLOCK, vv);
}

fn write_boot(frame: &mut Yuv420Frame, width: usize, height: usize, boot_layout: &BootLayout, boot: &[u8; BOOT_LEN], copies: usize) {
    let mut bits = Vec::<u8>::with_capacity(BOOT_LEN * 8);
    for byte in boot {
        for i in (0..8).rev() {
            bits.push((byte >> i) & 1);
        }
    }

    let copies = copies.min(BOOT_COPIES_MAX).min(boot_layout.copies.len()).max(1);
    for c in 0..copies {
        let pos = &boot_layout.copies[c];
        for (i, &(bx, by)) in pos.iter().enumerate() {
            let bit = bits.get(i).copied().unwrap_or(0);
            draw_boot_bit(frame, width, height, bx, by, bit);
        }
    }
}

fn median_f64(mut v: Vec<f64>) -> f64 {
    if v.is_empty() { return 0.0; }
    let n = v.len();
    let mid = n / 2;
    v.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if n % 2 == 1 {
        v[mid]
    } else {
        // v[mid] is the (mid+1)-th smallest; find max of left partition for the mid-th.
        let left_max = v[..mid].iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (left_max + v[mid]) * 0.5
    }
}

#[derive(Clone, Copy, Debug)]
struct BootRead {
    profile: u8,
    frame_idx: u32,
    geom: Geom,
    geom_est: Option<GeomEstimate>,
}

fn estimate_locator_geom_heavy(
    frame: &Yuv420Frame,
    bx_n: usize,
    by_n: usize,
    locator_size: usize,
    hint: Option<GeomSearchHint>,
) -> GeomEstimate {
    GeometryCorrector::default().estimate_global_shift_scored(
        bx_n,
        by_n,
        locator_size,
        [
            (LOC_Y0 as f64, LOC_U0 as f64, LOC_V0 as f64),
            (LOC_Y1 as f64, LOC_U0 as f64, LOC_V1 as f64),
            (LOC_Y0 as f64, LOC_U1 as f64, LOC_V1 as f64),
            (LOC_Y1 as f64, LOC_U1 as f64, LOC_V0 as f64),
        ],
        hint,
        |bx, by, g| {
            (
                avg_y_block_geom(&frame.y, frame.width, frame.height, Y_BLOCK, bx, by, g),
                avg_uv_block_420_geom(&frame.u, frame.width, frame.height, Y_BLOCK, bx, by, g),
                avg_uv_block_420_geom(&frame.v, frame.width, frame.height, Y_BLOCK, bx, by, g),
            )
        },
    )
}

fn locator_corner_positions(bx_n: usize, by_n: usize, locator_size: usize) -> [(usize, usize); 4] {
    let ls = locator_size.clamp(1, bx_n.min(by_n));
    [
        (0, 0),
        (bx_n.saturating_sub(ls), 0),
        (0, by_n.saturating_sub(ls)),
        (bx_n.saturating_sub(ls), by_n.saturating_sub(ls)),
    ]
}

// Decode locator thresholds from locator blocks with robust 0/1 grouping.
fn read_locator_thresholds_heavy(frame: &Yuv420Frame, bx_n: usize, by_n: usize, locator_size: usize, geom: Geom) -> (f64, f64, f64) {
    let gc = GeometryCorrector::default();
    let t: LocatorThresholds = gc.locator_thresholds_from_known_corners(
        bx_n,
        by_n,
        locator_size,
        geom,
        |bx0, by0, g| {
            let ls = locator_size.clamp(1, bx_n.min(by_n));
            let mut ys = Vec::with_capacity(ls * ls);
            let mut us = Vec::with_capacity(ls * ls);
            let mut vs = Vec::with_capacity(ls * ls);
            for ddy in 0..ls {
                for ddx in 0..ls {
                    let bx = bx0 + ddx;
                    let by = by0 + ddy;
                    ys.push(avg_y_block_geom(&frame.y, frame.width, frame.height, Y_BLOCK, bx, by, g));
                    us.push(avg_uv_block_420_geom(&frame.u, frame.width, frame.height, Y_BLOCK, bx, by, g));
                    vs.push(avg_uv_block_420_geom(&frame.v, frame.width, frame.height, Y_BLOCK, bx, by, g));
                }
            }
            (median_f64(ys), median_f64(us), median_f64(vs))
        },
    );
    (t.y, t.u, t.v)
}

fn read_locator_thresholds_simple(frame: &Yuv420Frame, bx_n: usize, by_n: usize, locator_size: usize, geom: Geom) -> (f64, f64, f64) {
    let corners = locator_corner_positions(bx_n, by_n, locator_size);
    let ls = locator_size.clamp(1, bx_n.min(by_n));
    let mut c = [(0.0, 0.0, 0.0); 4];
    for (i, &(bx0, by0)) in corners.iter().enumerate() {
        let mut ys = Vec::with_capacity(ls * ls);
        let mut us = Vec::with_capacity(ls * ls);
        let mut vs = Vec::with_capacity(ls * ls);
        for ddy in 0..ls {
            for ddx in 0..ls {
                let bx = bx0 + ddx;
                let by = by0 + ddy;
                ys.push(avg_y_block_geom(&frame.y, frame.width, frame.height, Y_BLOCK, bx, by, geom));
                us.push(avg_uv_block_420_geom(&frame.u, frame.width, frame.height, Y_BLOCK, bx, by, geom));
                vs.push(avg_uv_block_420_geom(&frame.v, frame.width, frame.height, Y_BLOCK, bx, by, geom));
            }
        }
        c[i] = (median_f64(ys), median_f64(us), median_f64(vs));
    }
    let avg2 = |a: f64, b: f64| (a + b) * 0.5;
    (
        avg2(c[0].0, c[2].0) * 0.5 + avg2(c[1].0, c[3].0) * 0.5,
        avg2(c[0].1, c[1].1) * 0.5 + avg2(c[2].1, c[3].1) * 0.5,
        avg2(c[0].2, c[3].2) * 0.5 + avg2(c[1].2, c[2].2) * 0.5,
    )
}

fn read_boot_try_llr(
    frame: &Yuv420Frame,
    thr_y: f64,
    thr_u: f64,
    thr_v: f64,
    pos: &[(usize, usize)],
    geom: Geom,
) -> Option<Vec<f32>> {
    let mut llr: Vec<f32> = Vec::with_capacity(BOOT_CODE_BITS);
    for &(bx, by) in pos {
        let yv = avg_y_block_geom(&frame.y, frame.width, frame.height, Y_BLOCK, bx, by, geom);
        let uv = avg_uv_block_420_geom(&frame.u, frame.width, frame.height, Y_BLOCK, bx, by, geom);
        let vv = avg_uv_block_420_geom(&frame.v, frame.width, frame.height, Y_BLOCK, bx, by, geom);
        // Positive LLR => bit 0 is more likely, negative => bit 1.
        let lly = ((thr_y - yv) / 8.0) as f32;
        let llu = ((thr_u - uv) / 10.0) as f32;
        let llv = ((thr_v - vv) / 10.0) as f32;
        llr.push(lly + 0.7 * llu + 0.7 * llv);
        if llr.len() >= BOOT_CODE_BITS {
            break;
        }
    }
    if llr.len() < BOOT_CODE_BITS { return None; }
    Some(llr)
}

fn read_boot(
    frame: &Yuv420Frame,
    boot_layout: &BootLayout,
    bx_n: usize,
    by_n: usize,
    locator_size: usize,
    heavy_geom: bool,
    geom_hint: Option<GeomSearchHint>,
) -> Option<BootRead> {
    let (geom, geom_est) = if heavy_geom {
        let est = estimate_locator_geom_heavy(frame, bx_n, by_n, locator_size, geom_hint);
        (est.geom, Some(est))
    } else {
        (Geom::identity(), None)
    };
    let (thr_y, thr_u, thr_v) = if heavy_geom {
        read_locator_thresholds_heavy(frame, bx_n, by_n, locator_size, geom)
    } else {
        read_locator_thresholds_simple(frame, bx_n, by_n, locator_size, geom)
    };
    let mut agg_llr = vec![0f32; BOOT_CODE_BITS];
    let mut have_copy = false;
    for c in 0..boot_layout.copies.len() {
        let pos = &boot_layout.copies[c];
        let Some(v) = read_boot_try_llr(frame, thr_y, thr_u, thr_v, pos, geom) else {
            continue;
        };
        for (dst, src) in agg_llr.iter_mut().zip(v.into_iter()) {
            *dst += src;
        }
        have_copy = true;
    }
    if !have_copy {
        return None;
    }
    let msg = boot_decode_msg_from_llr(&agg_llr)?;
    if let Some((profile, frame_idx)) = parse_boot_msg(&msg) {
        return Some(BootRead { profile, frame_idx, geom, geom_est });
    }

    // Fallback: try each copy independently (can help if one copy is badly corrupted).
    for c in 0..boot_layout.copies.len() {
        let pos = &boot_layout.copies[c];
        let Some(v) = read_boot_try_llr(frame, thr_y, thr_u, thr_v, pos, geom) else {
            continue;
        };
        let msg = boot_decode_msg_from_llr(&v)?;
        if let Some((profile, frame_idx)) = parse_boot_msg(&msg) {
            return Some(BootRead { profile, frame_idx, geom, geom_est });
        }
    }
    None
}

/* ------------------------------- Locators / CAL ----------------------------- */

fn draw_locators(frame: &mut Yuv420Frame, width: usize, height: usize, bx_n: usize, by_n: usize, locator_size: usize) {
    let ls = locator_size.max(1).min(bx_n.min(by_n));

    let corners = [
        (0usize, 0usize, 0u8, 0u8, 0u8),
        (bx_n - ls, 0usize, 1u8, 0u8, 1u8),
        (0usize, by_n - ls, 0u8, 1u8, 1u8),
        (bx_n - ls, by_n - ls, 1u8, 1u8, 0u8),
    ];

    for (bx0, by0, yb, ub, vb) in corners {
        let yv = if yb == 0 { LOC_Y0 } else { LOC_Y1 };
        let uv = if ub == 0 { LOC_U0 } else { LOC_U1 };
        let vv = if vb == 0 { LOC_V0 } else { LOC_V1 };
        for dy in 0..ls {
            for dx in 0..ls {
                let bx = bx0 + dx;
                let by = by0 + dy;
                fill_y_block(&mut frame.y, width, Y_BLOCK, bx, by, yv);
                fill_uv_block_420(&mut frame.u, width, height, bx, by, Y_BLOCK, uv);
                fill_uv_block_420(&mut frame.v, width, height, bx, by, Y_BLOCK, vv);
            }
        }
    }
}

fn bits_to_bytes_msb(bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    bits_to_bytes_msb_into(&mut out, bits);
    out
}

fn bits_to_bytes_msb_into(out: &mut Vec<u8>, bits: &[u8]) {
    let n = bits.len().div_ceil(8);
    out.clear();
    out.resize(n, 0);
    for (i, &b) in bits.iter().enumerate() {
        let byte = i / 8;
        let bit = i % 8;
        out[byte] |= (b & 1) << (7 - bit);
    }
}

/* --------------------------------- Payload interleaving --------------------------------- */

const PAYLOAD_TEMPORAL_PHASES: usize = 1; // was 4: every frame now uses all blocks (4× throughput gain)
const PAYLOAD_TILE_BX: usize = 8;
const PAYLOAD_TILE_BY: usize = 8;

fn build_payload_positions_vec(bx_n: usize, payload_by_start: usize, payload_by_end: usize) -> Vec<(usize, usize)> {
    let inner_w = bx_n.saturating_sub(2 * MARGIN_BLOCKS);
    let inner_h = payload_by_end.saturating_sub(payload_by_start);
    let tile_w = PAYLOAD_TILE_BX.max(1).min(inner_w.max(1));
    let tile_h = PAYLOAD_TILE_BY.max(1).min(inner_h.max(1));
    let tile_cols = inner_w.div_ceil(tile_w);
    let tile_rows = inner_h.div_ceil(tile_h);

    let mut tiles = Vec::<(usize, usize, usize, usize)>::with_capacity(tile_cols.saturating_mul(tile_rows));
    for ty in 0..tile_rows {
        for tx in 0..tile_cols {
            let x0 = MARGIN_BLOCKS + tx * tile_w;
            let y0 = payload_by_start + ty * tile_h;
            let tw = tile_w.min(inner_w.saturating_sub(tx * tile_w));
            let th = tile_h.min(inner_h.saturating_sub(ty * tile_h));
            if tw == 0 || th == 0 {
                continue;
            }
            tiles.push((x0, y0, tw, th));
        }
    }
    let mut tile_order: Vec<usize> = (0..tiles.len()).collect();
    let tile_seed = 0xA173_E9C4_51B4_002Du64
        ^ ((bx_n as u64) << 32)
        ^ ((payload_by_start as u64) << 16)
        ^ (payload_by_end as u64);
    let mut rng = SplitMix64::new(tile_seed);
    shuffle_in_place(&mut tile_order, &mut rng);

    let mut pos = Vec::<(usize, usize)>::with_capacity(inner_w.saturating_mul(inner_h));
    for ti in tile_order {
        let (x0, y0, tw, th) = tiles[ti];
        for by in y0..(y0 + th) {
            for bx in x0..(x0 + tw) {
                pos.push((bx, by));
            }
        }
    }
    pos
}

fn payload_phase(frame_idx: u32) -> usize {
    (frame_idx as usize) % PAYLOAD_TEMPORAL_PHASES.max(1)
}

fn payload_tile_index(layout: &ProfileLayout, bx: usize, by: usize) -> usize {
    let inner_w = layout.bx_n.saturating_sub(2 * MARGIN_BLOCKS).max(1);
    let tile_w = PAYLOAD_TILE_BX.max(1).min(inner_w);
    let tile_cols = inner_w.div_ceil(tile_w).max(1);
    let local_x = bx.saturating_sub(MARGIN_BLOCKS);
    let local_y = by.saturating_sub(layout.payload_by_start);
    let tile_x = (local_x / tile_w).min(tile_cols - 1);
    let tile_y = local_y / PAYLOAD_TILE_BY.max(1);
    tile_y.saturating_mul(tile_cols).saturating_add(tile_x)
}

fn payload_block_is_active(layout: &ProfileLayout, bx: usize, by: usize, frame_idx: u32) -> bool {
    let phases = PAYLOAD_TEMPORAL_PHASES.max(1);
    if phases <= 1 {
        return true;
    }
    (payload_tile_index(layout, bx, by) % phases) == payload_phase(frame_idx)
}

fn payload_active_positions_count(layout: &ProfileLayout, frame_idx: u32) -> usize {
    layout
        .payload_pos
        .iter()
        .filter(|&&(bx, by)| payload_block_is_active(layout, bx, by, frame_idx))
        .count()
}

fn payload_capacity_bits(layout: &ProfileLayout, pcfg: &ProfileCfg) -> usize {
    payload_capacity_bits_for_frame(layout, pcfg, 0)
}

fn payload_capacity_bits_for_frame(layout: &ProfileLayout, pcfg: &ProfileCfg, frame_idx: u32) -> usize {
    payload_active_positions_count(layout, frame_idx).saturating_mul(pcfg.bits_per_block())
}

fn gcd_usize(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn affine_prp_params(len: usize, seed: u64) -> (usize, usize) {
    if len <= 1 {
        return (1, 0);
    }
    let mut a = ((seed as usize) | 1) % len;
    if a == 0 {
        a = 1;
    }
    while gcd_usize(a, len) != 1 {
        a = (a + 2) % len;
        if a == 0 {
            a = 1;
        }
    }
    let b = ((seed >> 17) as usize) % len;
    (a, b)
}

fn affine_prp_map(i: usize, len: usize, a: usize, b: usize) -> usize {
    if len <= 1 {
        0
    } else {
        (a.wrapping_mul(i).wrapping_add(b)) % len
    }
}

fn frame_bit_permute_seed(profile: u8, frame_idx: u32) -> u64 {
    // Use GOP-level seed: frames within the same GOP share the same permutation,
    // making consecutive frames visually similar → much better H.264 temporal prediction.
    let gop_idx = frame_idx / (BOOT_CAL_KEYFRAME_INTERVAL as u32);
    mix_seed(0x5C11_DA7A_FEED_B17Eu64 ^ (profile as u64), gop_idx)
}

fn bytes_to_bits_msb(bytes: &[u8]) -> Vec<u8> {
    let mut bits = Vec::new();
    bytes_to_bits_msb_into(&mut bits, bytes);
    bits
}

fn bytes_to_bits_msb_into(bits: &mut Vec<u8>, bytes: &[u8]) {
    bits.clear();
    bits.reserve(bytes.len().saturating_mul(8).saturating_sub(bits.capacity()));
    for &b in bytes {
        for i in (0..8).rev() {
            bits.push((b >> i) & 1);
        }
    }
}

/* ---------------------------------- Sink ---------------------------------- */

enum Sink {
    Raw(BufWriter<Box<dyn Write + Send>>),
    Ffmpeg { child: Child, stdin: BufWriter<ChildStdin> },
}

impl Sink {
    fn stdout_raw() -> Self {
        Sink::Raw(BufWriter::new(Box::new(std::io::stdout())))
    }
    fn file_raw(path: &Path) -> Result<Self> {
        let f = fs::File::create(path).with_context(|| format!("create {:?}", path))?;
        Ok(Sink::Raw(BufWriter::new(Box::new(f))))
    }
    fn spawn_ffmpeg(
        ffmpeg: &str,
        width: usize,
        height: usize,
        fps: u32,
        preset: &str,
        crf: u8,
        fragmented_mp4: bool,
        out: &Path,
    ) -> Result<Self> {
        let mut cmd = Command::new(ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-loglevel").arg("warning")
            .arg("-f").arg("rawvideo")
            .arg("-pix_fmt").arg("yuv420p")
            .arg("-video_size").arg(format!("{}x{}", width, height))
            .arg("-framerate").arg(format!("{}", fps))
            .arg("-i").arg("pipe:0")
            .arg("-c:v").arg("libx264")
            .arg("-preset").arg(preset)
            .arg("-crf").arg(format!("{}", crf))
            .arg("-pix_fmt").arg("yuv420p");
        if fragmented_mp4 {
            cmd.arg("-movflags").arg("+frag_keyframe+empty_moov+default_base_moof");
        } else {
            cmd.arg("-movflags").arg("+faststart");
        }
        cmd.arg(out.as_os_str());
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::inherit());
        let mut child = cmd.spawn().with_context(|| format!("spawn ffmpeg ({})", ffmpeg))?;
        let stdin = child.stdin.take().context("failed to take ffmpeg stdin")?;
        Ok(Sink::Ffmpeg { child, stdin: BufWriter::new(stdin) })
    }
    fn write_yuv420p(&mut self, frame: &Yuv420Frame) -> Result<()> {
        match self {
            Sink::Raw(w) => { w.write_all(&frame.y)?; w.write_all(&frame.u)?; w.write_all(&frame.v)?; Ok(()) }
            Sink::Ffmpeg { stdin, .. } => { stdin.write_all(&frame.y)?; stdin.write_all(&frame.u)?; stdin.write_all(&frame.v)?; Ok(()) }
        }
    }
    fn finish(self) -> Result<()> {
        match self {
            Sink::Raw(mut w) => { w.flush()?; Ok(()) }
            Sink::Ffmpeg { mut child, mut stdin } => {
                stdin.flush()?;
                drop(stdin);
                let status = child.wait()?;
                if !status.success() { bail!("ffmpeg exited with status {status}"); }
                Ok(())
            }
        }
    }
}

/* --------------------------------- Source --------------------------------- */

enum Source {
    Raw(BufReader<Box<dyn Read + Send>>),
    Ffmpeg { child: Child, stdout: BufReader<ChildStdout> },
}

impl Source {
    fn file_raw(path: &Path) -> Result<Self> {
        let f = fs::File::open(path).with_context(|| format!("open {:?}", path))?;
        Ok(Source::Raw(BufReader::new(Box::new(f))))
    }
    fn spawn_ffmpeg_decode(ffmpeg: &str, input: &Path) -> Result<Self> {
        let mut cmd = Command::new(ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-loglevel").arg("warning")
            .arg("-i").arg(input.as_os_str())
            .arg("-an").arg("-sn").arg("-dn")
            .arg("-f").arg("rawvideo")
            .arg("-pix_fmt").arg("yuv420p")
            .arg("pipe:1");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        let mut child = cmd.spawn().with_context(|| format!("spawn ffmpeg ({})", ffmpeg))?;
        let stdout = child.stdout.take().context("failed to take ffmpeg stdout")?;
        Ok(Source::Ffmpeg { child, stdout: BufReader::new(stdout) })
    }
    fn read_exact_or_eof(r: &mut dyn Read, buf: &mut [u8]) -> Result<bool> {
        match r.read_exact(buf) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    fn read_yuv420p(&mut self, frame: &mut Yuv420Frame) -> Result<bool> {
        match self {
            Source::Raw(r) => {
                if !Self::read_exact_or_eof(r.get_mut().as_mut(), &mut frame.y)? { return Ok(false); }
                if !Self::read_exact_or_eof(r.get_mut().as_mut(), &mut frame.u)? { return Ok(false); }
                if !Self::read_exact_or_eof(r.get_mut().as_mut(), &mut frame.v)? { return Ok(false); }
                Ok(true)
            }
            Source::Ffmpeg { stdout, .. } => {
                if !Self::read_exact_or_eof(stdout, &mut frame.y)? { return Ok(false); }
                if !Self::read_exact_or_eof(stdout, &mut frame.u)? { return Ok(false); }
                if !Self::read_exact_or_eof(stdout, &mut frame.v)? { return Ok(false); }
                Ok(true)
            }
        }
    }
    fn finish(&mut self) -> Result<()> {
        if let Source::Ffmpeg { child, .. } = self {
            let status = child.wait()?;
            if !status.success() { bail!("ffmpeg decode exited with status {status}"); }
        }
        Ok(())
    }
}

fn probe_video_wh(ffprobe: &str, input: &Path) -> Result<(usize, usize)> {
    let out = Command::new(ffprobe)
        .arg("-v").arg("error")
        .arg("-select_streams").arg("v:0")
        .arg("-show_entries").arg("stream=width,height")
        .arg("-of").arg("default=nw=1:nk=1")
        .arg(input.as_os_str())
        .output()
        .with_context(|| format!("spawn ffprobe ({})", ffprobe))?;
    if !out.status.success() { bail!("ffprobe failed"); }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.lines();
    let w: usize = it.next().context("ffprobe missing width")?.trim().parse()?;
    let h: usize = it.next().context("ffprobe missing height")?.trim().parse()?;
    Ok((w, h))
}

fn is_mp4_like(p: &Path) -> bool {
    match p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()) {
        Some(ext) if ext == "mp4" || ext == "mov" => true,
        _ => false,
    }
}

/* ------------------------------- Drawing ops ------------------------------ */

fn fill_y_block(y: &mut [u8], width: usize, block: usize, bx: usize, by: usize, val: u8) {
    let x0 = bx * block;
    let y0 = by * block;
    for yy in y0..(y0 + block) {
        let row = yy * width;
        let start = row + x0;
        y[start..start + block].fill(val);
    }
}

fn fill_uv_block_420(
    plane: &mut [u8],
    width: usize,
    height: usize,
    bx: usize,
    by: usize,
    y_block: usize,
    val: u8,
) {
    let uv_w = width / 2;
    let uv_h = height / 2;
    let x0 = (bx * y_block) / 2;
    let y0 = (by * y_block) / 2;
    let c_block = y_block / 2;
    if x0 + c_block > uv_w || y0 + c_block > uv_h { return; }
    for yy in y0..(y0 + c_block) {
        let row = yy * uv_w;
        let start = row + x0;
        plane[start..start + c_block].fill(val);
    }
}

/* -------------------------------- Protocol -------------------------------- */

fn crc32(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/* ------------------------------- Stream + LDPC ------------------------------- */

#[derive(Clone, Copy, Debug)]
struct QcLdpcSpec {
    z: usize,
    kb: usize,
    mb: usize,
}

fn qc_spec_from_profile(pcfg: &ProfileCfg) -> Option<QcLdpcSpec> {
    let z = 32usize;
    if pcfg.ldpc_n % z != 0 || pcfg.ldpc_k % z != 0 || pcfg.ldpc_m() % z != 0 {
        return None;
    }
    let nb = pcfg.ldpc_n / z;
    let kb = pcfg.ldpc_k / z;
    let mb = pcfg.ldpc_m() / z;
    if nb != kb + mb {
        return None;
    }
    Some(QcLdpcSpec {
        z,
        kb,
        mb,
    })
}

const QC_H1_R5_6_MB4_KB20: [[i16; 20]; 4] = [
    [0, 24, 16, 19, 11, 3, 6, 30, 22, 25, 17, 9, 12, 4, 28, 31, 23, 15, 18, 10],
    [4, 28, -1, 23, 15, -1, 10, 2, -1, 29, 21, -1, 16, 8, -1, 3, 27, -1, 22, 14],
    [8, -1, 28, 27, -1, 15, 14, -1, 2, 1, -1, 21, 20, -1, 8, 7, -1, 27, 26, -1],
    [-1, 8, 0, -1, 27, 19, -1, 14, 6, -1, 1, 25, -1, 20, 12, -1, 7, 31, -1, 26],
];

const QC_H2_MB4: [[i16; 4]; 4] = [
    [0, -1, -1, 1],
    [4, 0, -1, -1],
    [-1, 7, 0, -1],
    [-1, -1, 10, 0],
];

const QC_H1_R2_3_MB8_KB16: [[i16; 16]; 8] = [
    [0, -1, 16, 19, -1, 3, 6, -1, 22, 25, -1, 9, 12, -1, 28, 31],
    [-1, 28, -1, -1, 15, -1, -1, 2, -1, -1, 21, -1, -1, 8, -1, -1],
    [8, -1, -1, 27, -1, -1, 14, -1, -1, 1, -1, -1, 20, -1, -1, 7],
    [-1, -1, 0, -1, -1, 19, -1, -1, 6, -1, -1, 25, -1, -1, 12, -1],
    [-1, 12, -1, -1, 31, -1, -1, 18, -1, -1, 5, -1, -1, 24, -1, -1],
    [24, -1, -1, 11, -1, -1, 30, -1, -1, 17, -1, -1, 4, -1, -1, 23],
    [-1, -1, 16, -1, -1, 3, -1, -1, 22, -1, -1, 9, -1, -1, 28, -1],
    [-1, 28, -1, -1, 15, -1, -1, 2, -1, -1, 21, -1, -1, 8, -1, -1],
];

const QC_H2_MB8: [[i16; 8]; 8] = [
    [0, -1, -1, -1, -1, -1, -1, 1],
    [4, 0, -1, -1, -1, -1, -1, -1],
    [-1, 7, 0, -1, -1, -1, -1, -1],
    [-1, -1, 10, 0, -1, -1, -1, -1],
    [-1, -1, -1, 13, 0, -1, -1, -1],
    [-1, -1, -1, -1, 16, 0, -1, -1],
    [-1, -1, -1, -1, -1, 19, 0, -1],
    [-1, -1, -1, -1, -1, -1, 22, 0],
];

const QC_H1_R1_2_MB12_KB12: [[i16; 12]; 12] = [
    [0, 24, 16, -1, -1, -1, -1, -1, -1, 25, 17, 9],
    [-1, -1, -1, 23, 15, 7, -1, -1, -1, -1, -1, -1],
    [-1, -1, 28, -1, -1, -1, 14, 6, -1, -1, -1, 21],
    [12, -1, -1, -1, 27, 19, -1, -1, -1, 5, -1, -1],
    [-1, -1, -1, -1, -1, -1, 26, 18, 10, -1, -1, -1],
    [24, 16, 8, -1, -1, -1, -1, -1, -1, 17, 9, 1],
    [-1, -1, -1, 15, 7, -1, -1, -1, 22, -1, -1, -1],
    [-1, 28, 20, -1, -1, -1, 6, -1, -1, -1, 21, 13],
    [-1, -1, -1, 27, 19, 11, -1, -1, -1, -1, -1, -1],
    [-1, -1, -1, -1, -1, -1, 18, 10, 2, -1, -1, -1],
    [16, 8, -1, -1, -1, 23, -1, -1, -1, 9, 1, -1],
    [-1, -1, -1, 7, -1, -1, -1, 22, 14, -1, -1, -1],
];

const QC_H2_MB12: [[i16; 12]; 12] = [
    [0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 1],
    [4, 0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    [-1, 7, 0, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    [-1, -1, 10, 0, -1, -1, -1, -1, -1, -1, -1, -1],
    [-1, -1, -1, 13, 0, -1, -1, -1, -1, -1, -1, -1],
    [-1, -1, -1, -1, 16, 0, -1, -1, -1, -1, -1, -1],
    [-1, -1, -1, -1, -1, 19, 0, -1, -1, -1, -1, -1],
    [-1, -1, -1, -1, -1, -1, 22, 0, -1, -1, -1, -1],
    [-1, -1, -1, -1, -1, -1, -1, 25, 0, -1, -1, -1],
    [-1, -1, -1, -1, -1, -1, -1, -1, 28, 0, -1, -1],
    [-1, -1, -1, -1, -1, -1, -1, -1, -1, 31, 0, -1],
    [-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 2, 0],
];

fn qc_base_h1(spec: QcLdpcSpec) -> Vec<Vec<i16>> {
    match (spec.z, spec.kb, spec.mb) {
        (32, 20, 4) => QC_H1_R5_6_MB4_KB20.iter().map(|r| r.to_vec()).collect(),
        (32, 16, 8) => QC_H1_R2_3_MB8_KB16.iter().map(|r| r.to_vec()).collect(),
        (32, 12, 12) => QC_H1_R1_2_MB12_KB12.iter().map(|r| r.to_vec()).collect(),
        _ => panic!("unsupported QC-LDPC H1 table for z={}, kb={}, mb={}", spec.z, spec.kb, spec.mb),
    }
}

fn qc_base_h2(spec: QcLdpcSpec) -> Vec<Vec<i16>> {
    match (spec.z, spec.mb) {
        (32, 4) => QC_H2_MB4.iter().map(|r| r.to_vec()).collect(),
        (32, 8) => QC_H2_MB8.iter().map(|r| r.to_vec()).collect(),
        (32, 12) => QC_H2_MB12.iter().map(|r| r.to_vec()).collect(),
        _ => panic!("unsupported QC-LDPC H2 table for z={}, mb={}", spec.z, spec.mb),
    }
}

fn qc_expand_rows(base: &[Vec<i16>], z: usize) -> Vec<Vec<usize>> {
    let rb_n = base.len();
    let cb_n = base.first().map(|r| r.len()).unwrap_or(0);
    let mut rows = vec![Vec::<usize>::new(); rb_n * z];
    for rb in 0..rb_n {
        for zr in 0..z {
            let row_idx = rb * z + zr;
            let mut cols = Vec::<usize>::new();
            for cb in 0..cb_n {
                let sh = base[rb][cb];
                if sh < 0 {
                    continue;
                }
                let sh = sh as usize % z;
                let col = cb * z + ((zr + sh) % z);
                cols.push(col);
            }
            cols.sort_unstable();
            cols.dedup();
            rows[row_idx] = cols;
        }
    }
    rows
}

#[derive(Clone)]
struct LdpcCode {
    n: usize,
    k: usize,
    m: usize,
    h1_rows: Vec<Vec<usize>>,
    h2_inv_rows: Vec<Vec<u8>>,
    h2_inv_words: Vec<Vec<u64>>,
    row_edges: Vec<Vec<usize>>,
    row_edge_offsets: Vec<usize>,
    row_edge_flat: Vec<usize>,
    col_edges: Vec<Vec<usize>>,
    edge_cols: Vec<usize>,
}

#[derive(Default)]
struct LdpcEncodeScratch {
    out: Vec<u8>,
    rhs: Vec<u8>,
    rhs_words: Vec<u64>,
}

impl LdpcEncodeScratch {
    fn for_code(code: &LdpcCode) -> Self {
        Self {
            out: vec![0; code.n],
            rhs: vec![0; code.m],
            rhs_words: vec![0; code.m.div_ceil(64)],
        }
    }

    fn ensure_for_code(&mut self, code: &LdpcCode) {
        if self.out.len() != code.n {
            self.out.resize(code.n, 0);
        }
        if self.rhs.len() != code.m {
            self.rhs.resize(code.m, 0);
        }
        let words = code.m.div_ceil(64);
        if self.rhs_words.len() != words {
            self.rhs_words.resize(words, 0);
        }
    }
}

#[derive(Default)]
struct LdpcDecodeScratch {
    v2c: Vec<f32>,
    c2v: Vec<f32>,
    post: Vec<f32>,
    hard: Vec<u8>,
}

impl LdpcDecodeScratch {
    fn for_code(code: &LdpcCode) -> Self {
        let e_cnt = code.edge_cols.len();
        Self {
            v2c: vec![0.0; e_cnt],
            c2v: vec![0.0; e_cnt],
            post: vec![0.0; code.n],
            hard: vec![0; code.n],
        }
    }

    fn ensure_for_code(&mut self, code: &LdpcCode) {
        let e_cnt = code.edge_cols.len();
        if self.v2c.len() != e_cnt { self.v2c.resize(e_cnt, 0.0); }
        if self.c2v.len() != e_cnt { self.c2v.resize(e_cnt, 0.0); }
        if self.post.len() != code.n { self.post.resize(code.n, 0.0); }
        if self.hard.len() != code.n { self.hard.resize(code.n, 0); }
    }
}

fn gf2_invert_dense(a: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let n = a.len();
    if n == 0 || a.iter().any(|r| r.len() != n) {
        return None;
    }
    let mut left = a.to_vec();
    let mut right = vec![vec![0u8; n]; n];
    for i in 0..n {
        right[i][i] = 1;
    }

    for col in 0..n {
        let mut piv = col;
        while piv < n && (left[piv][col] & 1) == 0 {
            piv += 1;
        }
        if piv >= n {
            return None;
        }
        if piv != col {
            left.swap(piv, col);
            right.swap(piv, col);
        }
        for r in 0..n {
            if r != col && (left[r][col] & 1) != 0 {
                for c in col..n {
                    left[r][c] ^= left[col][c];
                }
                for c in 0..n {
                    right[r][c] ^= right[col][c];
                }
            }
        }
    }
    Some(right)
}

fn dense_from_sparse_rows(rows: &[Vec<usize>], m: usize) -> Vec<Vec<u8>> {
    let mut out = vec![vec![0u8; m]; m];
    for (r, cols) in rows.iter().enumerate().take(m) {
        for &c in cols {
            if c < m {
                out[r][c] ^= 1;
            }
        }
    }
    out
}

fn gen_ldpc_h2_rows_and_inv(m: usize, seed: u64, info_col_w: usize) -> (Vec<Vec<usize>>, Vec<Vec<u8>>) {
    let extra_w = if info_col_w >= 4 { 2 } else { 1 }; // total parity column degree >= 2
    for attempt in 0..64u64 {
        let mut rng = SplitMix64::new(seed ^ 0xA5F0_91C3_D27B_4E11 ^ attempt);
        let mut rows: Vec<Vec<usize>> = vec![Vec::new(); m];
        for j in 0..m {
            rows[j].push(j); // diagonal ones => good baseline rank
        }
        for j in 0..m {
            let mut picked = HashSet::<usize>::new();
            while picked.len() < extra_w {
                let r = rng.gen_range_usize(m.max(1));
                if r == j {
                    continue;
                }
                picked.insert(r);
            }
            for r in picked {
                rows[r].push(j);
            }
        }
        for r in &mut rows {
            r.sort_unstable();
            r.dedup();
        }
        let dense = dense_from_sparse_rows(&rows, m);
        if let Some(inv) = gf2_invert_dense(&dense) {
            return (rows, inv);
        }
    }

    // Fallback: dual-diagonal lower triangular (always invertible; only edge case keeps one degree-1 parity node).
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); m];
    for r in 0..m {
        rows[r].push(r);
        if r > 0 {
            rows[r].push(r - 1);
        }
    }
    let dense = dense_from_sparse_rows(&rows, m);
    let inv = gf2_invert_dense(&dense).unwrap_or_else(|| {
        let mut id = vec![vec![0u8; m]; m];
        for i in 0..m {
            id[i][i] = 1;
        }
        id
    });
    (rows, inv)
}

impl LdpcCode {
    fn from_profile(pcfg: &ProfileCfg) -> Self {
        let n = pcfg.ldpc_n;
        let k = pcfg.ldpc_k;
        let m = pcfg.ldpc_m();
        let spec = qc_spec_from_profile(pcfg).unwrap_or_else(|| QcLdpcSpec {
            z: 32,
            kb: k / 32,
            mb: m / 32,
        });
        let h1_base = qc_base_h1(spec);
        let mut h2_base = qc_base_h2(spec);
        let h1_rows = qc_expand_rows(&h1_base, spec.z);
        let mut h2_rows_local = qc_expand_rows(&h2_base, spec.z);
        let mut h2_inv_rows = gf2_invert_dense(&dense_from_sparse_rows(&h2_rows_local, m));
        if h2_inv_rows.is_none() && spec.mb > 1 {
            // Try alternate wrap shifts while keeping the QC base matrix fixed-structure.
            for sh in 2..spec.z {
                h2_base[0][spec.mb - 1] = sh as i16;
                h2_rows_local = qc_expand_rows(&h2_base, spec.z);
                h2_inv_rows = gf2_invert_dense(&dense_from_sparse_rows(&h2_rows_local, m));
                if h2_inv_rows.is_some() {
                    break;
                }
            }
        }
        let (h2_rows_final, h2_inv_rows) = if let Some(inv) = h2_inv_rows {
            (h2_rows_local, inv)
        } else {
            gen_ldpc_h2_rows_and_inv(m, pcfg.ldpc_seed, pcfg.ldpc_col_w.max(2))
        };

        let words_per_row = m.div_ceil(64);
        let mut h2_inv_words = vec![vec![0u64; words_per_row]; m];
        for r in 0..m {
            for c in 0..m {
                if (h2_inv_rows[r][c] & 1) != 0 {
                    h2_inv_words[r][c / 64] |= 1u64 << (c % 64);
                }
            }
        }

        let mut row_edges: Vec<Vec<usize>> = vec![Vec::new(); m];
        let mut col_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut edge_cols: Vec<usize> = Vec::new();

        for r in 0..m {
            for &c in &h1_rows[r] {
                let e = edge_cols.len();
                edge_cols.push(c);
                row_edges[r].push(e);
                col_edges[c].push(e);
            }
            for &pc in &h2_rows_final[r] {
                let parity_col = k + pc;
                let e = edge_cols.len();
                edge_cols.push(parity_col);
                row_edges[r].push(e);
                col_edges[parity_col].push(e);
            }
        }

        let mut row_edge_offsets = Vec::with_capacity(m + 1);
        let mut row_edge_flat = Vec::new();
        row_edge_offsets.push(0);
        for row in &row_edges {
            row_edge_flat.extend_from_slice(row);
            row_edge_offsets.push(row_edge_flat.len());
        }

        Self {
            n,
            k,
            m,
            h1_rows,
            h2_inv_rows,
            h2_inv_words,
            row_edges,
            row_edge_offsets,
            row_edge_flat,
            col_edges,
            edge_cols,
        }
    }

    fn encode(&self, info_bits: &[u8]) -> Vec<u8> {
        let mut scratch = LdpcEncodeScratch::for_code(self);
        self.encode_with_scratch(info_bits, &mut scratch).to_vec()
    }

    fn encode_with_scratch<'a>(&self, info_bits: &[u8], scratch: &'a mut LdpcEncodeScratch) -> &'a [u8] {
        scratch.ensure_for_code(self);
        let out = &mut scratch.out;
        out.fill(0);
        let copy_n = info_bits.len().min(self.k);
        out[..copy_n].copy_from_slice(&info_bits[..copy_n]);
        let rhs = &mut scratch.rhs;
        rhs.fill(0);
        for r in 0..self.m {
            let mut v = 0u8;
            for &c in &self.h1_rows[r] {
                v ^= out[c] & 1;
            }
            rhs[r] = v & 1;
        }
        let rhs_words = &mut scratch.rhs_words;
        rhs_words.fill(0);
        for (i, &b) in rhs.iter().enumerate() {
            if (b & 1) != 0 {
                rhs_words[i / 64] |= 1u64 << (i % 64);
            }
        }
        for r in 0..self.m {
            let mut parity_acc = 0u32;
            for (wrow, wrhs) in self.h2_inv_words[r].iter().zip(rhs_words.iter()) {
                parity_acc ^= (wrow & wrhs).count_ones();
            }
            out[self.k + r] = (parity_acc & 1) as u8;
        }
        &out[..]
    }

    #[allow(dead_code)]
    fn decode_soft(&self, ch_llr: &[f32], max_iters: usize) -> Option<Vec<u8>> {
        let mut scratch = LdpcDecodeScratch::for_code(self);
        self.decode_soft_with_scratch(ch_llr, max_iters, &mut scratch)
    }

    fn decode_soft_with_scratch(
        &self,
        ch_llr: &[f32],
        max_iters: usize,
        scratch: &mut LdpcDecodeScratch,
    ) -> Option<Vec<u8>> {
        if ch_llr.len() != self.n {
            return None;
        }
        scratch.ensure_for_code(self);
        scratch.post[..self.n].copy_from_slice(ch_llr);
        scratch.c2v.fill(0.0);

        let alpha = 0.875f32;
        for _ in 0..max_iters.max(1) {
            for r in 0..self.m {
                let rs = self.row_edge_offsets[r];
                let re = self.row_edge_offsets[r + 1];
                let row = &self.row_edge_flat[rs..re];
                if row.is_empty() { continue; }
                let mut sign_prod: f32 = 1.0;
                let mut min1 = f32::INFINITY;
                let mut min2 = f32::INFINITY;
                let mut min1_edge = usize::MAX;

                for &e in row {
                    let col = self.edge_cols[e];
                    let q = scratch.post[col] - scratch.c2v[e];
                    scratch.v2c[e] = q;
                    if q < 0.0 { sign_prod = -sign_prod; }
                    let a = q.abs();
                    if a < min1 {
                        min2 = min1;
                        min1 = a;
                        min1_edge = e;
                    } else if a < min2 {
                        min2 = a;
                    }
                }

                for &e in row {
                    let q = scratch.v2c[e];
                    let s = if q < 0.0 { -1.0 } else { 1.0 };
                    let sign_ex = sign_prod * s;
                    let mag = if e == min1_edge { min2 } else { min1 };
                    let new_r = sign_ex * (alpha * mag);
                    let delta = new_r - scratch.c2v[e];
                    scratch.c2v[e] = new_r;
                    let col = self.edge_cols[e];
                    scratch.post[col] += delta;
                }
            }

            for col in 0..self.n {
                scratch.hard[col] = if scratch.post[col] < 0.0 { 1 } else { 0 };
            }
            let mut ok = true;
            for r in 0..self.m {
                let rs = self.row_edge_offsets[r];
                let re = self.row_edge_offsets[r + 1];
                let row = &self.row_edge_flat[rs..re];
                let mut p = 0u8;
                for &e in row {
                    p ^= scratch.hard[self.edge_cols[e]] & 1;
                }
                if p != 0 {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some(scratch.hard[..self.k].to_vec());
            }
        }
        None
    }
}

// Protocol v4: drop per-codeword CRC16 tail to reduce expansion.
// Integrity is still protected by LDPC parity checks + stream header CRC + final file CRC32.
const CW_INFO_TAIL_CRC16_LEN: usize = 0;

fn ldpc_info_bytes(code: &LdpcCode) -> Result<usize> {
    if code.k % 8 != 0 {
        bail!("LDPC info bits K={} must be byte-aligned", code.k);
    }
    Ok(code.k / 8)
}

fn ldpc_user_payload_bytes(code: &LdpcCode) -> Result<usize> {
    let info_bytes = ldpc_info_bytes(code)?;
    let overhead = CW_INFO_TAIL_CRC16_LEN;
    if info_bytes <= overhead {
        bail!(
            "LDPC K={} too small for codeword envelope overhead {} bytes",
            code.k,
            overhead
        );
    }
    Ok(info_bytes - overhead)
}

fn pack_cw_info_block(
    code: &LdpcCode,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    pack_cw_info_block_into(code, payload, &mut b)?;
    Ok(b)
}

fn pack_cw_info_block_into(
    code: &LdpcCode,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    let info_bytes = ldpc_info_bytes(code)?;
    let payload_cap = ldpc_user_payload_bytes(code)?;
    if payload.len() > payload_cap {
        bail!("payload too large for codeword block: {} > {}", payload.len(), payload_cap);
    }

    out.clear();
    out.resize(info_bytes, 0);
    out[..payload.len()].copy_from_slice(payload);
    if CW_INFO_TAIL_CRC16_LEN >= 2 {
        let crc_off = info_bytes - CW_INFO_TAIL_CRC16_LEN;
        let crc = (crc32(&out[..crc_off]) & 0xffff) as u16;
        out[crc_off..info_bytes].copy_from_slice(&crc.to_be_bytes());
    }
    Ok(())
}

fn unpack_cw_info_block(code: &LdpcCode, bytes: &[u8]) -> Option<Vec<u8>> {
    Some(unpack_cw_info_block_view(code, bytes)?.to_vec())
}

fn unpack_cw_info_block_view<'a>(code: &LdpcCode, bytes: &'a [u8]) -> Option<&'a [u8]> {
    let info_bytes = ldpc_info_bytes(code).ok()?;
    if bytes.len() != info_bytes {
        return None;
    }
    if CW_INFO_TAIL_CRC16_LEN >= 2 {
        let crc_off = info_bytes - CW_INFO_TAIL_CRC16_LEN;
        let got = u16::from_be_bytes(bytes[crc_off..info_bytes].try_into().ok()?);
        let exp = (crc32(&bytes[..crc_off]) & 0xffff) as u16;
        if got != exp {
            return None;
        }
        return Some(&bytes[..crc_off]);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldpc_noiseless_roundtrip_all_profiles() {
        for pcfg in PROFILE_CFGS.iter() {
            let code = LdpcCode::from_profile(pcfg);
            let mut info = vec![0u8; code.k];
            for (i, b) in info.iter_mut().enumerate() {
                *b = (((i * 17 + pcfg.profile as usize) ^ (i >> 1)) & 1) as u8;
            }
            let cw = code.encode(&info);
            for r in 0..code.m {
                let rs = code.row_edge_offsets[r];
                let re = code.row_edge_offsets[r + 1];
                let mut p = 0u8;
                for &e in &code.row_edge_flat[rs..re] {
                    p ^= cw[code.edge_cols[e]] & 1;
                }
                assert_eq!(p, 0, "encoded codeword parity check failed: profile {} row {}", pcfg.profile, r);
            }
            let llr: Vec<f32> = cw
                .iter()
                .map(|&b| if b == 0 { 12.0 } else { -12.0 })
                .collect();
            let got = code
                .decode_soft(&llr, pcfg.ldpc_iters.max(4))
                .expect("ldpc decode should succeed on noiseless llr");
            assert_eq!(got, info, "profile {}", pcfg.profile);
        }
    }

    #[test]
    fn codeword_envelope_roundtrip() {
        let code = LdpcCode::from_profile(cfg(0));
        let payload_cap = ldpc_user_payload_bytes(&code).unwrap();
        let mut payload = vec![0u8; payload_cap - 3];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(13).wrapping_add(7);
        }
        let packed = pack_cw_info_block(&code, &payload).unwrap();
        let unpacked = unpack_cw_info_block(&code, &packed).unwrap();
        assert_eq!(&unpacked[..payload.len()], &payload[..]);
        assert!(unpacked[payload.len()..].iter().all(|&x| x == 0));
    }
}
