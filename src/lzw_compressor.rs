use crate::compressor::Compressor;
use std::collections::HashMap;

const MAX_DICT_SIZE: u16 = 4096;
const RESET_CODE: u16 = 256;
const CHECKPOINT_INTERVAL: usize = 16 * 1024;

pub(crate) struct LzwCompressor {
    dictionary: HashMap<Vec<u8>, u16>,
    next_code: u16,
    input_len: usize,
    output_len: f32,
    checkpoint: usize,
    ratio: f32,
}

impl LzwCompressor {
    pub fn new() -> Self {
        let mut encoder = LzwCompressor {
            dictionary: HashMap::new(),
            next_code: RESET_CODE + 1,
            input_len: 0,
            output_len: 0.0,
            checkpoint: 0,
            ratio: 0.0,
        };

        encoder.reset_dictionary();

        encoder
    }

    fn reset_dictionary(&mut self) {
        self.dictionary.clear();
        self.next_code = RESET_CODE + 1;

        for c in 0..=255 {
            self.dictionary.insert(vec![c], c as u16);
        }
    }

    fn encode(chunk: &[u16]) -> Vec<u8> {
        match *chunk {
            [first, second] => {
                vec![
                    (first >> 4) as u8,
                    (((first & 0x0F) as u8) << 4) | ((second >> 8) as u8),
                    (second & 0xFF) as u8,
                ]
            }

            [only] => {
                vec![(only >> 4) as u8, ((only & 0x0F) as u8) << 4]
            }

            _ => vec![],
        }
    }
}

impl Compressor for LzwCompressor {
    fn compress(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output: Vec<u16> = Vec::new();
        let mut seq: Vec<u8> = Vec::new();

        for c in input {
            self.input_len += 1;
            let mut seq_c = seq.clone();
            seq_c.push(*c);

            if self.dictionary.contains_key(&seq_c) {
                seq = seq_c;
            } else {
                output.push(self.dictionary[&seq]);
                self.output_len += 1.5;

                if self.next_code < MAX_DICT_SIZE {
                    self.dictionary.insert(seq_c, self.next_code);
                    self.next_code += 1;
                } else if self.ratio == 0.0 {
                    self.ratio = ((self.output_len * 3.0) / 4.0) / (self.input_len as f32);
                    self.checkpoint = self.input_len + CHECKPOINT_INTERVAL;
                } else if self.input_len > self.checkpoint {
                    let ratio = ((self.output_len * 3.0) / 4.0) / (self.input_len as f32);

                    if self.ratio - ratio < 0.0 {
                        self.reset_dictionary();
                        output.push(RESET_CODE);
                        self.ratio = 0.0;
                        self.input_len = 0;
                        self.output_len = 0.0;
                    } else {
                        self.ratio = ratio;
                    }

                    self.checkpoint = self.input_len + CHECKPOINT_INTERVAL;
                }

                seq = vec![*c];
            }
        }

        if !seq.is_empty() {
            output.push(self.dictionary[&seq]);
            self.output_len += 1.5;
        }

        output.chunks(2).flat_map(LzwCompressor::encode).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Compressor;
    use super::LzwCompressor;

    #[test]
    fn compress() {
        let mut compressor = LzwCompressor::new();

        let in1: Vec<u8> = vec![1, 2, 3, 2, 3, 1, 3, 4, 5, 2, 4, 2, 3, 1, 5];
        let out1 = to_vec_u16(compressor.compress(&in1));

        assert_eq!(out1, vec![1, 2, 3, 258, 1, 3, 4, 5, 2, 4, 260, 5]);

        let in2: Vec<u8> = vec![1, 2, 3, 2, 3, 1, 3, 4, 5, 2, 4, 2, 3, 1, 5];
        let out2 = to_vec_u16(compressor.compress(&in2));

        assert_eq!(out2, vec![257, 259, 3, 261, 263, 265, 267]);

        let in3: Vec<u8> = vec![6, 2, 3, 1, 5, 6, 2];
        let out3 = to_vec_u16(compressor.compress(&in3));

        assert_eq!(out3, vec![6, 267, 274]);
    }

    fn to_vec_u16(input: Vec<u8>) -> Vec<u16> {
        input.chunks(3).flat_map(decode).collect()
    }

    fn decode(chunk: &[u8]) -> Vec<u16> {
        match *chunk {
            [first, second, third] => {
                vec![
                    ((first as u16) << 4) | ((second as u16) >> 4),
                    (((second as u16) & 0x0F) << 8) | (third as u16),
                ]
            }

            [first, second] => {
                vec![((first as u16) << 4) | ((second as u16) >> 4)]
            }

            _ => vec![],
        }
    }
}
