pub fn bytes_to_bits_msb(data: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(data.len() * 8);
    for &b in data {
        for i in (0..8).rev() {
            bits.push((b >> i) & 1);
        }
    }
    bits
}

pub fn bits_to_bytes_msb(bits: &[u8]) -> Vec<u8> {
    if bits.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &bit) in bits.iter().enumerate() {
        if bit & 1 == 1 {
            out[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    out
}

pub fn hard_llr_to_bits(llr: &[f32]) -> Vec<u8> {
    llr.iter().map(|&v| if v < 0.0 { 1 } else { 0 }).collect()
}
