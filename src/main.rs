use anyhow::{bail, Result};
use clap::{ArgEnum, Parser};
use env_logger::Env;
use futures_util::{FutureExt, SinkExt, StreamExt};
use log::{debug, info};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use vt::VT;
use warp::ws::{Message, WebSocket, Ws};
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

impl From<Event> for Message {
    fn from(event: Event) -> Self {
        match event {
            Event::Stdout(data) => Message::binary(data.as_bytes()),

            Event::Reset(cols, rows) => {
                Message::text(format!("{{\"cols\": {}, \"rows\": {}}}", cols, rows))
            }
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

async fn handle_client(
    websocket: WebSocket,
    initial_events: Vec<Event>,
    mut broadcast_rx: broadcast::Receiver<Event>,
) -> Result<()> {
    let (mut sender, _) = websocket.split();

    for event in initial_events {
        sender.feed(event.into()).await?;
    }

    sender.flush().await?;

    while let Ok(event) = broadcast_rx.recv().await {
        sender.send(event.into()).await?;
    }

    Ok(())
}

fn initial_events(vt: &VT) -> Vec<Event> {
    vec![Event::Reset(vt.columns, vt.rows), Event::Stdout(vt.dump())]
}

async fn handle_events(
    cols: usize,
    rows: usize,
    mut stream_rx: mpsc::Receiver<Event>,
    mut clients_rx: mpsc::Receiver<(SocketAddr, WebSocket)>,
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

            value = clients_rx.recv() => {
                let (addr, websocket) = value?;
                info!("client connected: {:?}", addr);
                let events = initial_events(&vt);
                let broadcast_rx = broadcast_tx.subscribe();

                tokio::spawn(async move {
                    let result = handle_client(websocket, events, broadcast_rx).await;
                    info!("client disconnected: {:?}", addr);

                    if let Err(e) = result {
                        debug!("client err: {:?}", e);
                    }
                });
            }
        }
    }
}

fn ws_handler(
    addr: Option<SocketAddr>,
    ws: Ws,
    clients_tx: mpsc::Sender<(SocketAddr, WebSocket)>,
) -> impl Reply {
    ws.on_upgrade(move |websocket| async move {
        if let Some(addr) = addr {
            let _ = clients_tx.send((addr, websocket)).await;
        }
    })
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
    let clients_tx = warp::any().map(move || clients_tx.clone());

    let ws_route = warp::path("ws")
        .and(warp::addr::remote())
        .and(warp::ws())
        .and(clients_tx)
        .map(ws_handler);

    let static_route = warp_embed::embed(&Assets);
    let routes = ws_route.or(static_route);

    let listen_addr: SocketAddr = cli.listen_addr.parse()?;
    info!("serving assets from ./public at http://{}", listen_addr);
    info!("streaming at ws://{}/ws", listen_addr);
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
