use anyhow::{bail, Result};
use avt::Vt;
use clap::{ArgEnum, Parser};
use env_logger::Env;
use futures_util::{stream, Stream, StreamExt};
use log::{debug, info};
use regex::Regex;
use serde::Deserialize;
use std::future;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;
use tokio_stream::wrappers::BroadcastStream;
mod alis;
mod forwarder;
mod server;

#[derive(Clone, Debug, ArgEnum)]
enum InputFormat {
    Asciicast,
    Raw,
}

#[derive(Deserialize)]
pub struct Header {
    pub width: usize,
    pub height: usize,
}

fn validate_forward_url(s: &str) -> Result<(), String> {
    match url::Url::parse(s) {
        Ok(url) => {
            let scheme = url.scheme();

            if scheme == "ws" || scheme == "wss" {
                Ok(())
            } else {
                Err("must be WebSocket URL (ws:// or wss://)".to_owned())
            }
        }

        Err(e) => Err(e.to_string()),
    }
}

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    /// Input pipe filename (defaults to stdin)
    filename: Option<String>,

    /// WebSocket forwarding address
    #[clap(validator = validate_forward_url)]
    forward_url: Option<url::Url>,

    /// Input format
    #[clap(long, arg_enum, default_value_t = InputFormat::Asciicast)]
    in_fmt: InputFormat,

    /// Listen address
    #[clap(short, long, default_value_t = String::from("0.0.0.0:8765"))]
    listen_addr: String,

    /// Virtual terminal width
    #[clap(long, default_value_t = 80)]
    cols: usize,

    /// Virtual terminal height
    #[clap(long, default_value_t = 24)]
    rows: usize,

    /// Enable verbose logging
    #[clap(short, long)]
    verbose: bool,
}

#[derive(Debug, Clone)]
enum InputEvent {
    Reset(Option<(usize, usize)>),
    Stdout(f32, String),
    Closed,
}

#[derive(Debug, Clone)]
enum StreamEvent {
    Reset((usize, usize), f32, Option<String>),
    Stdout(f32, String),
    Offline,
}

impl From<StreamEvent> for serde_json::Value {
    fn from(event: StreamEvent) -> Self {
        use StreamEvent::*;

        match event {
            Reset((cols, rows), time, init) => serde_json::json!({
                "cols": cols,
                "rows": rows,
                "time": time,
                "init": init,
            }),

            Stdout(time, data) => serde_json::json!((time, "o", data)),

            Offline => serde_json::json!({ "status": "offline" }),
        }
    }
}

async fn read_asciicast_file<F>(file: F, stream_tx: &mpsc::Sender<InputEvent>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
    use InputEvent::*;

    let buf_reader = tokio::io::BufReader::new(file);
    let mut lines = buf_reader.lines();
    let mut first_read = true;

    while let Ok(Some(line)) = lines.next_line().await {
        match (line.chars().next(), first_read) {
            (Some('{'), _) => {
                let header = serde_json::from_str::<Header>(&line)?;
                stream_tx
                    .send(Reset(Some((header.width, header.height))))
                    .await?;
            }

            (Some('['), false) => {
                let (time, event_type, data) = serde_json::from_str::<(f32, &str, String)>(&line)?;

                if event_type == "o" {
                    stream_tx.send(Stdout(time, data)).await?;
                }
            }

            _ => bail!("invalid input line"),
        }

        first_read = false;
    }

    Ok(())
}

async fn read_raw_file<F>(mut file: F, stream_tx: &mpsc::Sender<InputEvent>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
    use InputEvent::*;

    let mut buffer = [0; 1024];
    let mut first_read = true;
    let script_header_re = Regex::new(r#"\[.*COLUMNS="(\d{1,3})" LINES="(\d{1,3})".*\]"#).unwrap();
    let resize_seq_re = Regex::new(r#"\x1b\[8;(\d{1,3});(\d{1,3})t"#).unwrap();
    let now = Instant::now();

    while let Ok(n) = file.read(&mut buffer[..]).await {
        if n == 0 {
            break;
        }

        let str = String::from_utf8_lossy(&buffer[..n]);

        if first_read {
            let size = if let Some(caps) = script_header_re.captures(&str) {
                let cols: usize = caps[1].parse().unwrap();
                let rows: usize = caps[2].parse().unwrap();
                Some((cols, rows))
            } else if let Some(caps) = resize_seq_re.captures(&str) {
                let cols: usize = caps[2].parse().unwrap();
                let rows: usize = caps[1].parse().unwrap();
                Some((cols, rows))
            } else {
                None
            };

            stream_tx.send(Reset(size)).await?;

            first_read = false;
        }

        stream_tx
            .send(Stdout(now.elapsed().as_secs_f32(), str.into_owned()))
            .await?;
    }

    Ok(())
}

type Reader = Pin<Box<dyn AsyncRead + Send + 'static>>;

async fn read_file(
    filename: Option<String>,
    format: InputFormat,
    stream_tx: mpsc::Sender<InputEvent>,
) -> Result<()> {
    let mut file: Reader = match &filename {
        Some(filename) => Box::pin(Box::new(File::open(filename).await?)),
        None => Box::pin(Box::new(tokio::io::stdin())),
    };

    loop {
        match format {
            InputFormat::Asciicast => read_asciicast_file(file, &stream_tx).await?,
            InputFormat::Raw => read_raw_file(file, &stream_tx).await?,
        }

        stream_tx.send(InputEvent::Closed).await?;

        if let Some(filename) = &filename {
            file = Box::pin(Box::new(File::open(filename).await?));
        } else {
            break;
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct ClientInitResponse {
    online: bool,
    stream_time: f32,
    cols: usize,
    rows: usize,
    init: String,
    broadcast_rx: broadcast::Receiver<StreamEvent>,
}

impl ClientInitResponse {
    fn new(
        online: bool,
        stream_time: f32,
        vt: &Vt,
        broadcast_rx: broadcast::Receiver<StreamEvent>,
    ) -> Self {
        Self {
            online,
            stream_time,
            cols: vt.cols,
            rows: vt.rows,
            init: vt.dump(),
            broadcast_rx,
        }
    }
}

type ClientInitRequest = oneshot::Sender<ClientInitResponse>;

async fn handle_events(
    default_cols: usize,
    default_rows: usize,
    mut input_rx: mpsc::Receiver<InputEvent>,
    mut clients_rx: mpsc::Receiver<ClientInitRequest>,
) -> Option<()> {
    let mut vt = Vt::new(default_cols, default_rows);
    let (broadcast_tx, _) = broadcast::channel(1024);
    let mut last_stream_time = 0.0;
    let mut last_feed_time = Instant::now();
    let mut online = false;

    loop {
        tokio::select! {
            value = input_rx.recv() => {
                let event = value?;
                debug!("stream event: {:?}", event);

                match &event {
                    InputEvent::Reset(size) => {
                        let (cols, rows) = size.unwrap_or((default_cols, default_rows));
                        vt = Vt::new(cols, rows);
                        last_stream_time = 0.0;
                        last_feed_time = Instant::now();
                        online = true;
                        let _ = broadcast_tx.send(StreamEvent::Reset((cols, rows), 0.0, None));
                    }

                    InputEvent::Stdout(time, data) => {
                        vt.feed_str(data);
                        last_stream_time = *time;
                        last_feed_time = Instant::now();
                        let _ = broadcast_tx.send(StreamEvent::Stdout(*time, data.clone()));
                    }

                    InputEvent::Closed => {
                        online = false;
                        let _ = broadcast_tx.send(StreamEvent::Offline);
                    }
                }
            }

            request = clients_rx.recv() => {
                let reply_tx = request?;
                let stream_time = last_stream_time + (Instant::now() - last_feed_time).as_secs_f32();
                let response = ClientInitResponse::new(online, stream_time, &vt, broadcast_tx.subscribe());
                reply_tx.send(response).unwrap();
            }
        }
    }
}

async fn event_stream(
    clients_tx: &mpsc::Sender<ClientInitRequest>,
) -> Result<impl Stream<Item = StreamEvent>> {
    use StreamEvent::*;

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

async fn alis_stream(
    clients_tx: &mpsc::Sender<ClientInitRequest>,
) -> Result<impl Stream<Item = Vec<u8>>> {
    let mut alis_encoder = alis::Encoder::default();

    let s1 = stream::once(future::ready(alis_encoder.header()));

    let s2 = event_stream(clients_tx)
        .await?
        .map(move |e| alis_encoder.encode(e));

    Ok(s1.chain(s2))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        false => "info",
        true => "debug",
    };

    env_logger::Builder::from_env(Env::default().default_filter_or(log_level)).init();

    let (input_tx, input_rx) = mpsc::channel(1024);
    let (clients_tx, clients_rx) = mpsc::channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let listen_addr: SocketAddr = cli.listen_addr.parse()?;
    let mut server_handle = server::serve(listen_addr, clients_tx.clone(), shutdown_rx)?;

    let source_name = cli.filename.clone().unwrap_or_else(|| "stdin".to_string());
    info!("reading from {}", source_name);
    let reader = read_file(cli.filename, cli.in_fmt, input_tx);
    let mut reader_handle = tokio::spawn(reader);

    if let Some(url) = cli.forward_url {
        info!("forwarding to {}", &url);
        tokio::spawn(forwarder::forward(clients_tx, url));
    }

    tokio::spawn(handle_events(cli.cols, cli.rows, input_rx, clients_rx));

    tokio::select! {
        result = &mut reader_handle => {
            debug!("reader finished: {:?}", &result);
            let _ = shutdown_tx.send(());
            result??;
        }

        result = &mut server_handle => {
            debug!("server finished: {:?}", &result);
            result?;
        }
    }

    Ok(())
}
