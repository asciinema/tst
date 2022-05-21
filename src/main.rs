use anyhow::Result;
use clap::{ArgEnum, Parser};
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde::Deserialize;
use std::pin::Pin;
use tokio;
use tokio::fs::File;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tungstenite::Message;
use vt::VT;

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
    /// Optional name to operate on
    filename: Option<String>,

    /// Specify input format
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
}

#[derive(Debug, Clone)]
enum Event {
    Stdout(String),
    Reset(usize, usize),
}

impl From<Event> for Message {
    fn from(event: Event) -> Self {
        match event {
            Event::Stdout(data) => Message::Binary(data.into()),

            Event::Reset(cols, rows) => {
                Message::Text(format!("{{\"cols\": {}, \"rows\": {}}}", cols, rows))
            }
        }
    }
}

async fn read_asciicast_file<F>(file: F, tx: &mpsc::Sender<Event>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
    let buf_reader = tokio::io::BufReader::new(file);
    let mut lines = buf_reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        match line.chars().nth(0) {
            Some('{') => {
                if let Ok(header) = serde_json::from_str::<Header>(&line) {
                    tx.send(Event::Reset(header.width, header.height)).await?;
                }
            }

            Some('[') => {
                if let Ok((_, "o", data)) = serde_json::from_str::<(f32, &str, String)>(&line) {
                    tx.send(Event::Stdout(data)).await?;
                }
            }

            _ => break,
        }
    }

    Ok(())
}

async fn read_raw_file<F>(mut file: F, tx: &mpsc::Sender<Event>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
    let mut buffer = [0; 1024];

    while let Ok(n) = file.read(&mut buffer[..]).await {
        if n == 0 {
            break;
        }

        tx.send(Event::Stdout(
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
    tx: mpsc::Sender<Event>,
) -> Result<()> {
    let mut file: Reader = match &filename {
        Some(filename) => Box::pin(Box::new(File::open(filename).await?)),
        None => Box::pin(Box::new(tokio::io::stdin())),
    };

    loop {
        match format {
            InputFormat::Asciicast => read_asciicast_file(file, &tx).await?,
            InputFormat::Raw => read_raw_file(file, &tx).await?,
        }

        if let Some(filename) = &filename {
            file = Box::pin(Box::new(File::open(filename).await?));
        } else {
            break;
        }
    }

    Ok(())
}

fn handle_event(event: Event, tx: &broadcast::Sender<Event>, mut vt: VT) -> VT {
    match &event {
        Event::Reset(cols, rows) => {
            vt = VT::new(*cols, *rows);
            let _ = tx.send(event);
        }

        Event::Stdout(text) => {
            vt.feed_str(text);
            let _ = tx.send(event);
        }
    }

    vt
}

async fn handle_client(
    tcp_stream: TcpStream,
    initial_events: Vec<Event>,
    mut rx: broadcast::Receiver<Event>,
) -> Result<()> {
    let ws_stream = tokio_tungstenite::accept_async(tcp_stream).await?;
    let (mut sender, _receiver) = ws_stream.split();

    for event in initial_events {
        sender.feed(event.into()).await?;
    }

    sender.flush().await?;

    while let Ok(event) = rx.recv().await {
        sender.send(event.into()).await?;
    }

    Ok(())
}

fn initial_events(vt: &VT) -> Vec<Event> {
    vec![Event::Reset(vt.columns, vt.rows), Event::Stdout(vt.dump())]
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut vt = VT::new(cli.cols, cli.rows);
    let listener = TcpListener::bind(&cli.listen_addr).await?;
    let (tx, mut rx) = mpsc::channel(32);
    let (tx2, _rx2) = broadcast::channel(1024);
    let mut x = tokio::spawn(read_file(cli.filename, cli.in_fmt, tx));

    println!("listening on {}", cli.listen_addr);

    loop {
        tokio::select! {
            a = &mut x => {
                println!("reader finished: {:?}", a);
                return a?;
            }

            Some(event) = rx.recv() => {
                println!("new event: {:?}", event);
                vt = handle_event(event, &tx2, vt);
            }

            Ok((stream, _)) = listener.accept() => {
                println!("new client: {}", stream.peer_addr()?);
                let evs = initial_events(&vt);
                let z = tx2.subscribe();

                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, evs, z).await {
                        println!("client err: {:?}", e);
                    }
                });
            }
        }
    }
}
