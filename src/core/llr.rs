use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub(crate) struct LlrCodeword {
    pub(crate) values: Vec<f32>,
}

impl LlrCodeword {
    pub(crate) fn as_slice(&self) -> &[f32] {
        &self.values
    }
}

#[derive(Default)]
pub(crate) struct LlrAssembler {
    ldpc_n: usize,
    pending_frame_llrs: BTreeMap<u32, Vec<f32>>,
    channel_llrs: Vec<f32>,
    consume_off: usize,
    next_frame_seq: Option<u32>,
}

impl LlrAssembler {
    pub(crate) fn new(ldpc_n: usize) -> Self {
        Self {
            ldpc_n,
            pending_frame_llrs: BTreeMap::new(),
            channel_llrs: Vec::new(),
            consume_off: 0,
            next_frame_seq: None,
        }
    }

    pub(crate) fn next_expected_frame(&self) -> Option<u32> {
        self.next_frame_seq
    }

    pub(crate) fn append_frame_llr(&mut self, frame_idx: u32, llr: Vec<f32>) {
        if self.next_frame_seq.is_none() {
            self.next_frame_seq = Some(frame_idx);
        }
        self.pending_frame_llrs.entry(frame_idx).or_insert(llr);
        self.drain_in_order();
    }

    pub(crate) fn available_llr_len(&self) -> usize {
        self.channel_llrs.len().saturating_sub(self.consume_off)
    }

    pub(crate) fn next_codeword(&mut self) -> Option<LlrCodeword> {
        if self.available_llr_len() < self.ldpc_n {
            return None;
        }
        let start = self.consume_off;
        let end = start + self.ldpc_n;
        let values = self.channel_llrs[start..end].to_vec();
        self.consume_off = end;
        if self.consume_off >= self.ldpc_n * 8 && self.consume_off * 2 >= self.channel_llrs.len() {
            self.channel_llrs.drain(..self.consume_off);
            self.consume_off = 0;
        }
        Some(LlrCodeword { values })
    }

    fn drain_in_order(&mut self) {
        while let Some(seq) = self.next_frame_seq {
            if let Some(v) = self.pending_frame_llrs.remove(&seq) {
                self.channel_llrs.extend_from_slice(&v);
                self.next_frame_seq = Some(seq.wrapping_add(1));
            } else {
                break;
            }
        }
    }
}