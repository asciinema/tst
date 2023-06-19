use crate::Event;

pub(crate) struct AlisEncoder {}

impl Default for AlisEncoder {
    fn default() -> Self {
        AlisEncoder::new()
    }
}

impl AlisEncoder {
    pub fn new() -> Self {
        AlisEncoder {}
    }

    pub fn header(&self) -> Vec<u8> {
        "ALiS\x01\x00\x00\x00\x00\x00".as_bytes().into()
    }

    pub fn encode(&mut self, event: Event) -> Vec<u8> {
        match event {
            Event::Reset(cols, rows, init, time) => {
                let cols_bytes = (cols as u16).to_le_bytes();
                let rows_bytes = (rows as u16).to_le_bytes();
                let time_bytes = time.unwrap_or(0.0).to_le_bytes();
                let init = init.unwrap_or_else(|| "".to_owned());
                let init_len = init.len() as u32;
                let init_len_bytes = init_len.to_le_bytes();

                let mut msg = vec![0x01]; // 1 byte
                msg.extend_from_slice(&cols_bytes); // 2 bytes
                msg.extend_from_slice(&rows_bytes); // 2 bytes
                msg.extend_from_slice(&time_bytes); // 4 bytes
                msg.extend_from_slice(&init_len_bytes); // 4 bytes
                msg.extend_from_slice(init.as_bytes()); // init_len bytes

                msg
            }

            Event::Stdout(time, text) => {
                let time_bytes = time.to_le_bytes();
                let text_len = text.len() as u32;
                let text_len_bytes = text_len.to_le_bytes();

                let mut msg = vec![b'o']; // 1 byte
                msg.extend_from_slice(&time_bytes); // 4 bytes
                msg.extend_from_slice(&text_len_bytes); // 4 bytes
                msg.extend_from_slice(text.as_bytes()); // text_len bytes

                msg
            }

            Event::Offline => vec![0x04],
        }
    }
}
