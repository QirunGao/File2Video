use anyhow::{bail, Result};

pub const Y_BLOCK: usize = 32;
pub const MARGIN_BLOCKS: usize = 1;

pub const META_MAGIC: &[u8; 4] = b"F2V7";
pub const META_VERSION: u8 = 7;

#[derive(Clone, Copy, Debug)]
pub struct ProfileCfg {
    pub id: u8,
    pub pam_order: u8,
    pub slots_per_frame: usize,
    pub coded_bits_per_slot: usize,
    pub b_sys: usize,
    pub b_par: usize,
    pub chunk_bytes: usize,
    pub r_meta: usize,
    pub window_chunks_suggest: usize,
}

impl ProfileCfg {
    pub fn validate(self) -> Result<Self> {
        if self.b_sys + self.b_par != self.coded_bits_per_slot {
            bail!(
                "invalid profile {}: b_sys + b_par must equal coded_bits_per_slot",
                self.id
            );
        }
        if self.b_sys == 0 || self.b_par == 0 {
            bail!("invalid profile {}: b_sys/b_par must both be > 0", self.id);
        }
        if self.chunk_bytes == 0 {
            bail!("invalid profile {}: chunk_bytes must be > 0", self.id);
        }
        Ok(self)
    }
}

pub const PROFILE_CFGS: [ProfileCfg; 3] = [
    ProfileCfg {
        id: 0,
        pam_order: 2,
        slots_per_frame: 384,
        coded_bits_per_slot: 384,
        b_sys: 224,
        b_par: 160,
        chunk_bytes: 16 * 1024,
        r_meta: 4,
        window_chunks_suggest: 256,
    },
    ProfileCfg {
        id: 1,
        pam_order: 4,
        slots_per_frame: 384,
        coded_bits_per_slot: 512,
        b_sys: 288,
        b_par: 224,
        chunk_bytes: 16 * 1024,
        r_meta: 4,
        window_chunks_suggest: 256,
    },
    ProfileCfg {
        id: 2,
        pam_order: 8,
        slots_per_frame: 384,
        coded_bits_per_slot: 640,
        b_sys: 320,
        b_par: 320,
        chunk_bytes: 16 * 1024,
        r_meta: 4,
        window_chunks_suggest: 256,
    },
];

pub fn cfg(profile: u8) -> Result<ProfileCfg> {
    let p = PROFILE_CFGS
        .iter()
        .copied()
        .find(|p| p.id == profile)
        .ok_or_else(|| anyhow::anyhow!("unknown profile: {}", profile))?;
    p.validate()
}
