use crate::ClientInitRequest;
use anyhow::Result;
use futures_util::{stream, Stream, StreamExt};
use std::future;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;

#[derive(Debug, Clone)]
pub enum Event {
    Reset((usize, usize), f32, Option<String>),
    Stdout(f32, String),
    Resize(f32, usize, usize),
    Offline,
}

pub async fn stream(
    clients_tx: &mpsc::Sender<ClientInitRequest>,
) -> Result<impl Stream<Item = Event>> {
    use Event::*;

    let (tx, rx) = oneshot::channel();
    clients_tx.send(tx).await?;
    let resp = rx.await?;

    let init_event = if resp.online {
        Reset((resp.cols, resp.rows), resp.stream_time, Some(resp.init))
    } else {
        Offline
    };

    let s1 = stream::once(future::ready(init_event));

    let s2 = BroadcastStream::new(resp.broadcast_rx)
        .take_while(|r| future::ready(r.is_ok()))
        .map(Result::unwrap);

    Ok(s1.chain(s2))
}
