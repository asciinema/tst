use crate::alis;
use crate::ClientInitRequest;
use anyhow::Result;
use futures_util::{sink, stream, StreamExt};
use log::{debug, info};
use std::time;
use tokio::sync::mpsc;
use tokio_stream::wrappers::IntervalStream;

const WS_PING_INTERVAL: u64 = 15;
const MAX_RECONNECT_DELAY: u64 = 5000;

pub async fn forward(clients_tx: mpsc::Sender<ClientInitRequest>, url: url::Url) -> Result<()> {
    let mut reconnect_attempt = 0;

    info!("forwarding to {url}");

    loop {
        let time = time::Instant::now();
        let result = forward_once(&clients_tx, &url).await;
        debug!("forwarder: {:?}", &result);

        if time.elapsed().as_secs_f32() > 1.0 {
            reconnect_attempt = 0;
        }

        let delay = exponential_delay(reconnect_attempt);
        reconnect_attempt = (reconnect_attempt + 1).min(10);
        info!("connection closed, reconnecting in {delay}");
        tokio::time::sleep(time::Duration::from_millis(delay)).await;
    }
}

async fn forward_once(clients_tx: &mpsc::Sender<ClientInitRequest>, url: &url::Url) -> Result<()> {
    use tokio_tungstenite::tungstenite;

    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
    info!("connected to endpoint");
    let (write, read) = ws.split();

    tokio::spawn(async {
        let _ = read.map(Ok).forward(sink::drain()).await;
    });

    let alis_stream = alis::stream(clients_tx)
        .await?
        .map(tungstenite::Message::binary);

    let interval = tokio::time::interval(time::Duration::from_secs(WS_PING_INTERVAL));

    let ping_stream = IntervalStream::new(interval)
        .skip(1)
        .map(|_| tungstenite::Message::Ping(vec![]));

    stream::select(alis_stream, ping_stream)
        .map(Ok)
        .forward(write)
        .await?;

    Ok(())
}

fn exponential_delay(attempt: usize) -> u64 {
    (2_u64.pow(attempt as u32) * 500).min(MAX_RECONNECT_DELAY)
}
