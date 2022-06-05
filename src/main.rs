use anyhow::{bail, Result};
use async_stream::stream;
use clap::{ArgEnum, Parser};
use env_logger::Env;
use futures_util::{FutureExt, SinkExt, Stream, StreamExt};
use log::{debug, info};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use vt::VT;
use warp::http::{self, Response};
use warp::hyper::Body;
use warp::sse;
use warp::ws;
use warp::{Filter, Reply};

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

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    /// Input pipe filename (defaults to stdin)
    filename: Option<String>,

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
    Stdout(String),
    Reset(usize, usize),
}

impl From<Event> for ws::Message {
    fn from(event: Event) -> Self {
        use Event::*;

        match event {
            Stdout(data) => ws::Message::binary(data.as_bytes()),
            Reset(cols, rows) => {
                ws::Message::text(format!("{{\"cols\": {}, \"rows\": {}}}", cols, rows))
            }
        }
    }
}

impl From<Event> for sse::Event {
    fn from(event: Event) -> Self {
        use Event::*;

        let e = sse::Event::default();

        match event {
            Stdout(data) => e.json_data((0.0, "o", data)).unwrap(),
            Reset(cols, rows) => e.data(format!("{{\"cols\": {}, \"rows\": {}}}", cols, rows)),
        }
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
                    .send(Event::Reset(header.width, header.height))
                    .await?;
            }

            Some('[') => {
                let (_, event_type, data) = serde_json::from_str::<(f32, &str, String)>(&line)?;

                if event_type == "o" {
                    stream_tx.send(Event::Stdout(data)).await?;
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

    while let Ok(n) = file.read(&mut buffer[..]).await {
        if n == 0 {
            break;
        }

        stream_tx
            .send(Event::Stdout(
                String::from_utf8_lossy(&buffer[..n]).into_owned(),
            ))
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

        if let Some(filename) = &filename {
            file = Box::pin(Box::new(File::open(filename).await?));
        } else {
            break;
        }
    }

    Ok(())
}

fn feed_event(mut vt: VT, event: &Event) -> VT {
    match &event {
        Event::Reset(cols, rows) => {
            vt = VT::new(*cols, *rows);
        }

        Event::Stdout(text) => {
            vt.feed_str(text);
        }
    }

    vt
}

fn initial_events(vt: &VT) -> Vec<Event> {
    vec![Event::Reset(vt.columns, vt.rows), Event::Stdout(vt.dump())]
}

type ClientInitResponse = (Vec<Event>, broadcast::Receiver<Event>);
type ClientInitRequest = oneshot::Sender<ClientInitResponse>;

async fn handle_events(
    cols: usize,
    rows: usize,
    mut stream_rx: mpsc::Receiver<Event>,
    mut clients_rx: mpsc::Receiver<ClientInitRequest>,
) -> Option<()> {
    let mut vt = VT::new(cols, rows);
    let (broadcast_tx, _) = broadcast::channel(1024);

    loop {
        tokio::select! {
            value = stream_rx.recv() => {
                let event = value?;
                debug!("stream event: {:?}", event);
                vt = feed_event(vt, &event);
                let _ = broadcast_tx.send(event);
            }

            request = clients_rx.recv() => {
                let reply_tx = request?;
                let events = initial_events(&vt);
                let broadcast_rx = broadcast_tx.subscribe();
                reply_tx.send((events, broadcast_rx)).unwrap();
            }
        }
    }
}

async fn init_client(clients_tx: mpsc::Sender<ClientInitRequest>) -> Result<ClientInitResponse> {
    let (tx, rx) = oneshot::channel();
    clients_tx.send(tx).await?;

    Ok(rx.await?)
}

async fn handle_websocket(
    websocket: ws::WebSocket,
    clients_tx: mpsc::Sender<ClientInitRequest>,
) -> Result<()> {
    let (events, mut broadcast_rx) = init_client(clients_tx).await?;
    let (mut sender, _) = websocket.split();

    for event in events {
        sender.feed(event.into()).await?;
    }

    sender.flush().await?;

    while let Ok(event) = broadcast_rx.recv().await {
        sender.send(event.into()).await?;
    }

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
    let (events, mut broadcast_rx) = init_client(clients_tx).await?;

    let stream = stream! {
        for event in events {
            yield Ok::<sse::Event, Infallible>(event.into());
        }

        while let Ok(event) = broadcast_rx.recv().await {
            yield Ok(event.into());
        }

        yield Ok(sse::Event::default().event("done").data("done"));
    };

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
