use crc32fast::Hasher;

pub(crate) const STREAM_MAGIC: &[u8; 4] = b"SLD1";
pub(crate) const STREAM_HEADER_LEN: usize = 24;
pub(crate) const STREAM_VERSION: u8 = 4;

#[derive(Clone, Debug)]
pub(crate) struct StreamHeader {
    pub(crate) profile: u8,
    pub(crate) file_len: u64,
    pub(crate) file_crc: u32,
}

pub(crate) fn make_stream_header(profile: u8, file_len: u64, file_crc: u32) -> [u8; STREAM_HEADER_LEN] {
    let mut b = [0u8; STREAM_HEADER_LEN];
    b[0..4].copy_from_slice(STREAM_MAGIC);
    b[4] = STREAM_VERSION;
    b[5] = profile;
    b[8..16].copy_from_slice(&file_len.to_be_bytes());
    b[16..20].copy_from_slice(&file_crc.to_be_bytes());
    let hcrc = crc32(&b[..20]);
    b[20..24].copy_from_slice(&hcrc.to_be_bytes());
    b
}

pub(crate) fn parse_stream_header(bytes: &[u8]) -> Option<StreamHeader> {
    if bytes.len() != STREAM_HEADER_LEN {
        return None;
    }
    if &bytes[0..4] != STREAM_MAGIC || bytes[4] != STREAM_VERSION {
        return None;
    }
    let got = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    let exp = crc32(&bytes[..20]);
    if got != exp {
        return None;
    }
    Some(StreamHeader {
        profile: bytes[5],
        file_len: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
        file_crc: u32::from_be_bytes(bytes[16..20].try_into().ok()?),
    })
}

fn crc32(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}