use crate::compressor::Compressor;
use std::collections::HashMap;

const MAX_DICT_SIZE: u16 = 4096;
const RESET_CODE: u16 = 256;

pub(crate) struct LzwCompressor {
    dictionary: HashMap<Vec<u8>, u16>,
    next_code: u16,
}

impl LzwCompressor {
    pub fn new() -> Self {
        let mut encoder = LzwCompressor {
            dictionary: HashMap::new(),
            next_code: RESET_CODE + 1,
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
}

impl Compressor for LzwCompressor {
    fn compress(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output: Vec<u16> = Vec::new();
        let mut seq: Vec<u8> = Vec::new();

        for c in input {
            let mut seq_c = seq.clone();
            seq_c.push(*c);

            if self.dictionary.contains_key(&seq_c) {
                seq = seq_c;
            } else {
                output.push(self.dictionary[&seq]);

                if self.next_code < MAX_DICT_SIZE {
                    self.dictionary.insert(seq_c, self.next_code);
                    self.next_code += 1;
                } else {
                    self.reset_dictionary();
                    output.push(RESET_CODE);
                }

                seq = vec![*c];
            }
        }

        if !seq.is_empty() {
            output.push(self.dictionary[&seq]);
        }

        output
            .into_iter()
            .flat_map(|code| code.to_le_bytes())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Compressor;
    use super::LzwCompressor;

    #[test]
    fn encode() {
        let mut compressor = LzwCompressor::new();

        let in1: Vec<u8> = vec![1, 2, 3, 2, 3, 1, 3, 4, 5, 2, 4, 2, 3, 1, 5];
        let out1 = to_vec_u16(compressor.compress(&in1));

        assert_eq!(out1, vec![1, 2, 3, 257, 1, 3, 4, 5, 2, 4, 259, 5]);

        let in2: Vec<u8> = vec![1, 2, 3, 2, 3, 1, 3, 4, 5, 2, 4, 2, 3, 1, 5];
        let out2 = to_vec_u16(compressor.compress(&in2));

        assert_eq!(out2, vec![256, 258, 3, 260, 262, 264, 266]);

        let in3: Vec<u8> = vec![6, 2, 3, 1, 5, 6, 2];
        let out3 = to_vec_u16(compressor.compress(&in3));

        assert_eq!(out3, vec![6, 266, 273]);
    }

    fn to_vec_u16(input: Vec<u8>) -> Vec<u16> {
        input
            .chunks(2)
            .map(|x| u16::from_le_bytes([x[0], x[1]]))
            .collect()
    }
}
