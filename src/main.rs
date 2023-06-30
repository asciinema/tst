use anyhow::{bail, Result};
use avt::Vt;
use clap::{ArgEnum, Parser};
use env_logger::Env;
use futures_util::{sink, stream, FutureExt, Stream, StreamExt};
use log::{debug, info};
use regex::Regex;
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::convert::Infallible;
use std::future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use warp::http::{self, Response};
use warp::hyper::Body;
use warp::sse;
use warp::ws;
use warp::{Filter, Reply};
mod alis_encoder;

const WS_PING_INTERVAL: u64 = 15;

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

#[derive(RustEmbed)]
#[folder = "public"]
struct Assets;

#[derive(Debug, Clone)]
enum Event {
    Reset(usize, usize, Option<String>, Option<f32>),
    Stdout(f32, String),
    Offline,
}

impl From<Event> for serde_json::Value {
    fn from(event: Event) -> Self {
        use Event::*;

        match event {
            Reset(cols, rows, init, time) => serde_json::json!({
                "cols": cols,
                "rows": rows,
                "init": init,
                "time": time
            }),

            Stdout(time, data) => serde_json::json!((time, "o", data)),

            Offline => serde_json::json!({ "state": "offline" }),
        }
    }
}

impl From<Event> for sse::Event {
    fn from(event: Event) -> Self {
        let sse_event = sse::Event::default();
        let json_value: serde_json::Value = event.into();

        sse_event.data(json_value.to_string())
    }
}

async fn read_asciicast_file<F>(file: F, stream_tx: &mpsc::Sender<Event>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
    let buf_reader = tokio::io::BufReader::new(file);
    let mut lines = buf_reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        match line.chars().next() {
            Some('{') => {
                let header = serde_json::from_str::<Header>(&line)?;
                stream_tx
                    .send(Event::Reset(header.width, header.height, None, None))
                    .await?;
            }

            Some('[') => {
                let (time, event_type, data) = serde_json::from_str::<(f32, &str, String)>(&line)?;

                if event_type == "o" {
                    stream_tx.send(Event::Stdout(time, data)).await?;
                }
            }

            _ => bail!("invalid input line"),
        }
    }

    Ok(())
}

async fn read_raw_file<F>(mut file: F, stream_tx: &mpsc::Sender<Event>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
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
            if let Some(caps) = script_header_re.captures(&str) {
                let cols: usize = caps[1].parse().unwrap();
                let rows: usize = caps[2].parse().unwrap();
                stream_tx.send(Event::Reset(cols, rows, None, None)).await?;
            } else if let Some(caps) = resize_seq_re.captures(&str) {
                let cols: usize = caps[2].parse().unwrap();
                let rows: usize = caps[1].parse().unwrap();
                stream_tx.send(Event::Reset(cols, rows, None, None)).await?;
            }

            first_read = false;
        }

        stream_tx
            .send(Event::Stdout(now.elapsed().as_secs_f32(), str.into_owned()))
            .await?;
    }

    Ok(())
}

type Reader = Pin<Box<dyn AsyncRead + Send + 'static>>;

async fn read_file(
    filename: Option<String>,
    format: InputFormat,
    stream_tx: mpsc::Sender<Event>,
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

        stream_tx.send(Event::Offline).await?;

        if let Some(filename) = &filename {
            file = Box::pin(Box::new(File::open(filename).await?));
        } else {
            break;
        }
    }

    Ok(())
}

async fn forward(clients_tx: &mpsc::Sender<ClientInitRequest>, url: &url::Url) -> Result<()> {
    use tokio_tungstenite::tungstenite;

    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
    info!("forwarder: connected to endpoint");
    let (write, read) = ws.split();

    tokio::spawn(async {
        let _ = read.map(Ok).forward(sink::drain()).await;
    });

    let alis_stream = alis_stream(clients_tx)
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
    (2_u64.pow(attempt as u32) * 500).min(5000)
}

async fn forwarder(clients_tx: mpsc::Sender<ClientInitRequest>, url: url::Url) -> Result<()> {
    let mut reconnect_attempt = 0;

    loop {
        let time = time::Instant::now();
        let result = forward(&clients_tx, &url).await;
        debug!("forwarder: {:?}", &result);

        if time.elapsed().as_secs_f32() > 1.0 {
            reconnect_attempt = 0;
        }

        let delay = exponential_delay(reconnect_attempt);
        reconnect_attempt += 1;
        info!("forwarder: connection closed, reconnecting in {delay}");
        tokio::time::sleep(time::Duration::from_millis(delay)).await;
    }
}

#[derive(Debug)]
struct ClientInitResponse {
    online: bool,
    stream_time: f32,
    cols: usize,
    rows: usize,
    init: String,
    broadcast_rx: broadcast::Receiver<Event>,
}

impl ClientInitResponse {
    fn new(
        online: bool,
        stream_time: f32,
        vt: &Vt,
        broadcast_rx: broadcast::Receiver<Event>,
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
    cols: usize,
    rows: usize,
    mut stream_rx: mpsc::Receiver<Event>,
    mut clients_rx: mpsc::Receiver<ClientInitRequest>,
) -> Option<()> {
    let mut vt = Vt::new(cols, rows);
    let (broadcast_tx, _) = broadcast::channel(1024);
    let mut last_stream_time = 0.0;
    let mut last_feed_time = Instant::now();
    let mut online = false;

    loop {
        tokio::select! {
            value = stream_rx.recv() => {
                let event = value?;
                debug!("stream event: {:?}", event);

                match &event {
                    Event::Reset(cols, rows, _, _) => {
                        vt = Vt::new(*cols, *rows);
                        last_stream_time = 0.0;
                        last_feed_time = Instant::now();
                        online = true;
                    }

                    Event::Stdout(time, data) => {
                        vt.feed_str(data);
                        last_stream_time = *time;
                        last_feed_time = Instant::now();
                    }

                    Event::Offline => {
                        online = false;
                    }
                }

                let _ = broadcast_tx.send(event);
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
) -> Result<impl Stream<Item = Event>> {
    use Event::*;

    let (tx, rx) = oneshot::channel();
    clients_tx.send(tx).await?;
    let resp = rx.await?;

    let init_event = if resp.online {
        Reset(
            resp.cols,
            resp.rows,
            Some(resp.init),
            Some(resp.stream_time),
        )
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
    let mut alis_encoder = alis_encoder::AlisEncoder::default();

    let s1 = stream::once(future::ready(alis_encoder.header()));

    let s2 = event_stream(clients_tx)
        .await?
        .map(move |e| alis_encoder.encode(e));

    Ok(s1.chain(s2))
}

async fn handle_websocket(
    websocket: ws::WebSocket,
    clients_tx: mpsc::Sender<ClientInitRequest>,
) -> Result<()> {
    let s1 = alis_stream(&clients_tx).await?.map(ws::Message::binary);
    let s2 = stream::once(future::ready(ws::Message::close_with(1000u16, "done")));
    s1.chain(s2).map(Ok).forward(websocket).await?;

    Ok(())
}

fn ws_handler(
    addr: SocketAddr,
    ws: ws::Ws,
    clients_tx: mpsc::Sender<ClientInitRequest>,
) -> impl Reply {
    ws.on_upgrade(move |websocket| async move {
        info!("ws client connected: {:?}", addr);

        let result = handle_websocket(websocket, clients_tx).await;
        info!("ws client disconnected: {:?}", addr);

        if let Err(e) = result {
            debug!("ws client err: {:?}", e);
        }
    })
}

async fn sse_stream(
    clients_tx: mpsc::Sender<ClientInitRequest>,
) -> Result<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let s1 = event_stream(&clients_tx).await?.map(|e| e.into());
    let s2 = stream::iter(vec![sse::Event::default().event("done").data("done")]);
    let stream = s1.chain(s2).map(Ok);

    Ok(stream)
}

async fn sse_handler(
    addr: SocketAddr,
    clients_tx: mpsc::Sender<ClientInitRequest>,
) -> Response<Body> {
    info!("sse client connected: {:?}", addr);

    match sse_stream(clients_tx).await {
        Ok(stream) => sse::reply(sse::keep_alive().stream(stream)).into_response(),

        Err(e) => {
            debug!("sse client err: {:?}", e);
            http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        false => "info",
        true => "debug",
    };

    env_logger::Builder::from_env(Env::default().default_filter_or(log_level)).init();

    let (stream_tx, stream_rx) = mpsc::channel(1024);
    let (clients_tx, clients_rx) = mpsc::channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let ws_clients_tx = clients_tx.clone();
    let ws_clients_tx = warp::any().map(move || ws_clients_tx.clone());
    let ws_route = warp::path("ws")
        .and(warp::addr::remote())
        .map(Option::unwrap)
        .and(warp::ws())
        .and(ws_clients_tx)
        .map(ws_handler);

    let sse_clients_tx = clients_tx.clone();
    let sse_clients_tx = warp::any().map(move || sse_clients_tx.clone());
    let sse_route = warp::path("sse")
        .and(warp::addr::remote())
        .map(Option::unwrap)
        .and(warp::get())
        .and(sse_clients_tx)
        .then(sse_handler);

    let routes = ws_route.or(sse_route).or(warp_embed::embed(&Assets));

    let listen_addr: SocketAddr = cli.listen_addr.parse()?;
    info!("streaming via WebSocket at ws://{}/ws", listen_addr);
    info!("streaming via SSE at http://{}/sse", listen_addr);
    info!("serving assets from ./public at http://{}", listen_addr);
    let signal = shutdown_rx.map(|_| ());
    let (_, server) = warp::serve(routes).try_bind_with_graceful_shutdown(listen_addr, signal)?;
    let mut server_handle = tokio::spawn(server);

    let source_name = cli.filename.clone().unwrap_or_else(|| "stdin".to_string());
    info!("reading from {}", source_name);
    let reader = read_file(cli.filename, cli.in_fmt, stream_tx);
    let mut reader_handle = tokio::spawn(reader);

    if let Some(url) = cli.forward_url {
        info!("forwarding to {}", &url);
        tokio::spawn(forwarder(clients_tx, url));
    }

    tokio::spawn(handle_events(cli.cols, cli.rows, stream_rx, clients_rx));

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
