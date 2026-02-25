/* ------------------------------- 固定布局参数 ------------------------------- */

pub(crate) const MAGIC: &[u8; 2] = b"YV";
pub(crate) const VERSION: u8 = 10;

pub(crate) const Y_BLOCK: usize = 32;
pub(crate) const MARGIN_BLOCKS: usize = 1;
pub(crate) const HEADER_ROWS: usize = 8;

pub(crate) const BOOT_MSG_LEN: usize = 12; // magic2 + ver1 + profile1 + frame_idx4 + crc32(4)
pub(crate) const BOOT_RS_N: usize = 24; // shortened RS(24,12) over GF(256)
pub(crate) const BOOT_RS_K: usize = BOOT_MSG_LEN;
pub(crate) const BOOT_RS_PARITY: usize = BOOT_RS_N - BOOT_RS_K;
pub(crate) const BOOT_RS_REPEAT: usize = 1; // reduced from 2: spatial diversity from copies is sufficient
pub(crate) const BOOT_CODE_BITS: usize =
    BOOT_RS_N * 8 * BOOT_RS_REPEAT;
pub(crate) const BOOT_LEN: usize = (BOOT_CODE_BITS + 7) / 8; // coded bytes on the frame
pub(crate) const BOOT_COPIES_MAX: usize = 2;
pub(crate) const BOOT_FIXED_ROWS: usize = 4;


pub(crate) const LOC_Y0: u8 = 64;
pub(crate) const LOC_Y1: u8 = 192;
pub(crate) const LOC_U0: u8 = 80;
pub(crate) const LOC_U1: u8 = 176;
pub(crate) const LOC_V0: u8 = 80;
pub(crate) const LOC_V1: u8 = 176;

pub(crate) const MAX_FILE_LEN: u64 = 2_000_000_000;


/* -------------------------------- ProfileCfg -------------------------------- */

#[derive(Clone, Copy)]
pub(crate) struct ProfileCfg {
    pub(crate) profile: u8,
    pub(crate) pam_m: usize,
    pub(crate) bits_per_plane: usize,
    pub(crate) boot_copies: usize,
    pub(crate) cal_repeats: usize,
    pub(crate) min_gap_y: f64,
    pub(crate) min_gap_uv: f64,
    pub(crate) locator_size: usize,
    pub(crate) ldpc_n: usize,
    pub(crate) ldpc_k: usize,
    pub(crate) ldpc_col_w: usize,
    pub(crate) ldpc_iters: usize,
    pub(crate) ldpc_seed: u64,
    pub(crate) y_levels: [u8; 8],
    pub(crate) u_levels: [u8; 8],
    pub(crate) v_levels: [u8; 8],
}

impl ProfileCfg {
    pub(crate) fn bits_per_block(&self) -> usize {
        self.bits_per_plane
    }
    pub(crate) fn ldpc_m(&self) -> usize {
        self.ldpc_n.saturating_sub(self.ldpc_k)
    }
    pub(crate) fn y_slice(&self) -> &[u8] {
        &self.y_levels[..self.pam_m]
    }
    pub(crate) fn u_slice(&self) -> &[u8] {
        &self.u_levels[..self.pam_m]
    }
    pub(crate) fn v_slice(&self) -> &[u8] {
        &self.v_levels[..self.pam_m]
    }
}

pub(crate) const LEVELS_Y_PAM2: [u8; 8] = [64, 192, 192, 192, 192, 192, 192, 192];
pub(crate) const LEVELS_UV_PAM2: [u8; 8] = [80, 176, 176, 176, 176, 176, 176, 176];

pub(crate) const LEVELS_Y_PAM4: [u8; 8] = [32, 96, 160, 224, 224, 224, 224, 224];
pub(crate) const LEVELS_UV_PAM4: [u8; 8] = [48, 96, 160, 208, 208, 208, 208, 208];

pub(crate) const LEVELS_Y_PAM8: [u8; 8] = [32, 59, 86, 113, 142, 169, 196, 224];
pub(crate) const LEVELS_UV_PAM8: [u8; 8] = [48, 71, 94, 117, 140, 163, 186, 208];

pub(crate) const PROFILE_CFGS: [ProfileCfg; 10] = [
    ProfileCfg {
        profile: 0,
        pam_m: 8,
        bits_per_plane: 3,
        boot_copies: 1,
        cal_repeats: 1,
        min_gap_y: 10.0,
        min_gap_uv: 8.0,
        locator_size: 1,
        ldpc_n: 768,
        ldpc_k: 640,
        ldpc_col_w: 3,
        ldpc_iters: 18,
        ldpc_seed: 0x1000_0000_0000_0000,
        y_levels: LEVELS_Y_PAM8,
        u_levels: LEVELS_UV_PAM8,
        v_levels: LEVELS_UV_PAM8,
    },
    ProfileCfg {
        profile: 1,
        pam_m: 8,
        bits_per_plane: 3,
        boot_copies: 1,
        cal_repeats: 2,
        min_gap_y: 10.0,
        min_gap_uv: 8.0,
        locator_size: 1,
        ldpc_n: 768,
        ldpc_k: 640,
        ldpc_col_w: 3,
        ldpc_iters: 18,
        ldpc_seed: 0x1000_0000_0000_0001,
        y_levels: LEVELS_Y_PAM8,
        u_levels: LEVELS_UV_PAM8,
        v_levels: LEVELS_UV_PAM8,
    },
    ProfileCfg {
        profile: 2,
        pam_m: 8,
        bits_per_plane: 3,
        boot_copies: 1,
        cal_repeats: 2,
        min_gap_y: 10.0,
        min_gap_uv: 8.0,
        locator_size: 1,
        ldpc_n: 768,
        ldpc_k: 640,
        ldpc_col_w: 3,
        ldpc_iters: 18,
        ldpc_seed: 0x1000_0000_0000_0002,
        y_levels: LEVELS_Y_PAM8,
        u_levels: LEVELS_UV_PAM8,
        v_levels: LEVELS_UV_PAM8,
    },
    ProfileCfg {
        profile: 3,
        pam_m: 4,
        bits_per_plane: 2,
        boot_copies: 1,
        cal_repeats: 2,
        min_gap_y: 14.0,
        min_gap_uv: 10.0,
        locator_size: 1,
        ldpc_n: 768,
        ldpc_k: 512,
        ldpc_col_w: 3,
        ldpc_iters: 20,
        ldpc_seed: 0x2000_0000_0000_0003,
        y_levels: LEVELS_Y_PAM4,
        u_levels: LEVELS_UV_PAM4,
        v_levels: LEVELS_UV_PAM4,
    },
    ProfileCfg {
        profile: 4,
        pam_m: 4,
        bits_per_plane: 2,
        boot_copies: 1,
        cal_repeats: 2,
        min_gap_y: 14.0,
        min_gap_uv: 10.0,
        locator_size: 1,
        ldpc_n: 768,
        ldpc_k: 512,
        ldpc_col_w: 3,
        ldpc_iters: 20,
        ldpc_seed: 0x2000_0000_0000_0004,
        y_levels: LEVELS_Y_PAM4,
        u_levels: LEVELS_UV_PAM4,
        v_levels: LEVELS_UV_PAM4,
    },
    ProfileCfg {
        profile: 5,
        pam_m: 4,
        bits_per_plane: 2,
        boot_copies: 2,
        cal_repeats: 3,
        min_gap_y: 14.0,
        min_gap_uv: 10.0,
        locator_size: 2,
        ldpc_n: 768,
        ldpc_k: 512,
        ldpc_col_w: 3,
        ldpc_iters: 20,
        ldpc_seed: 0x2000_0000_0000_0005,
        y_levels: LEVELS_Y_PAM4,
        u_levels: LEVELS_UV_PAM4,
        v_levels: LEVELS_UV_PAM4,
    },
    ProfileCfg {
        profile: 6,
        pam_m: 4,
        bits_per_plane: 2,
        boot_copies: 2,
        cal_repeats: 3,
        min_gap_y: 14.0,
        min_gap_uv: 10.0,
        locator_size: 2,
        ldpc_n: 768,
        ldpc_k: 512,
        ldpc_col_w: 3,
        ldpc_iters: 20,
        ldpc_seed: 0x2000_0000_0000_0006,
        y_levels: LEVELS_Y_PAM4,
        u_levels: LEVELS_UV_PAM4,
        v_levels: LEVELS_UV_PAM4,
    },
    ProfileCfg {
        profile: 7,
        pam_m: 2,
        bits_per_plane: 1,
        boot_copies: 2,
        cal_repeats: 4,
        min_gap_y: 20.0,
        min_gap_uv: 14.0,
        locator_size: 2,
        ldpc_n: 768,
        ldpc_k: 384,
        ldpc_col_w: 4,
        ldpc_iters: 24,
        ldpc_seed: 0x3000_0000_0000_0007,
        y_levels: LEVELS_Y_PAM2,
        u_levels: LEVELS_UV_PAM2,
        v_levels: LEVELS_UV_PAM2,
    },
    ProfileCfg {
        profile: 8,
        pam_m: 2,
        bits_per_plane: 1,
        boot_copies: 2,
        cal_repeats: 4,
        min_gap_y: 20.0,
        min_gap_uv: 14.0,
        locator_size: 3,
        ldpc_n: 768,
        ldpc_k: 384,
        ldpc_col_w: 4,
        ldpc_iters: 24,
        ldpc_seed: 0x3000_0000_0000_0008,
        y_levels: LEVELS_Y_PAM2,
        u_levels: LEVELS_UV_PAM2,
        v_levels: LEVELS_UV_PAM2,
    },
    ProfileCfg {
        profile: 9,
        pam_m: 2,
        bits_per_plane: 1,
        boot_copies: 2,
        cal_repeats: 4,
        min_gap_y: 20.0,
        min_gap_uv: 14.0,
        locator_size: 3,
        ldpc_n: 768,
        ldpc_k: 384,
        ldpc_col_w: 4,
        ldpc_iters: 24,
        ldpc_seed: 0x3000_0000_0000_0009,
        y_levels: LEVELS_Y_PAM2,
        u_levels: LEVELS_UV_PAM2,
        v_levels: LEVELS_UV_PAM2,
    },
];

pub(crate) fn cfg(profile: u8) -> &'static ProfileCfg {
    &PROFILE_CFGS[profile as usize]
}

pub(crate) fn cal_max_blocks_all_profiles() -> usize {
    PROFILE_CFGS
        .iter()
        .map(|p| p.pam_m.saturating_mul(p.cal_repeats))
        .max()
        .unwrap_or(0)
}

