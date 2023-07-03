use crate::client;
use crate::ClientInitRequest;
use anyhow::Result;
use futures_util::{stream, Stream, StreamExt};
use std::future;
use tokio::sync::mpsc;

pub(crate) struct Encoder {}

impl Default for Encoder {
    fn default() -> Self {
        Encoder::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Encoder {}
    }

    pub fn header(&self) -> Vec<u8> {
        "ALiS\x01\x00\x00\x00\x00\x00".as_bytes().into()
    }

    pub fn encode(&mut self, event: client::Event) -> Vec<u8> {
        use client::Event::*;

        match event {
            Reset((cols, rows), time, init) => {
                let cols_bytes = (cols as u16).to_le_bytes();
                let rows_bytes = (rows as u16).to_le_bytes();
                let time_bytes = time.to_le_bytes();
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

            Stdout(time, text) => {
                let time_bytes = time.to_le_bytes();
                let text_len = text.len() as u32;
                let text_len_bytes = text_len.to_le_bytes();

                let mut msg = vec![b'o']; // 1 byte
                msg.extend_from_slice(&time_bytes); // 4 bytes
                msg.extend_from_slice(&text_len_bytes); // 4 bytes
                msg.extend_from_slice(text.as_bytes()); // text_len bytes

                msg
            }

            Offline => vec![0x04],
        }
    }
}

pub async fn stream(
    clients_tx: &mpsc::Sender<ClientInitRequest>,
) -> Result<impl Stream<Item = Vec<u8>>> {
    let mut encoder = Encoder::default();

    let s1 = stream::once(future::ready(encoder.header()));

    let s2 = client::stream(clients_tx)
        .await?
        .map(move |e| encoder.encode(e));

    Ok(s1.chain(s2))
}
