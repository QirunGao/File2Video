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
        for _ in 0..2 {
            acc ^= bit;
            par_stream.push(acc & 1);
        }
    }

    FecEncoded {
        sys_stream,
        par_stream,
    }
}
