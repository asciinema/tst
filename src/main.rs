use anyhow::Result;
use avt::Vt;
use clap::{ArgGroup, Parser};
use env_logger::Env;
use log::debug;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;
mod alis;
mod client;
mod forwarder;
mod input;
mod server;

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
#[clap(group(ArgGroup::new("output").args(&["forward-url", "listen-addr"]).required(true).multiple(true)))]
struct Cli {
    /// Input filename [default: stdin]
    #[clap(short, long)]
    input: Option<String>,

    /// WebSocket forwarding address
    #[clap(short, long, validator = validate_forward_url)]
    forward_url: Option<url::Url>,

    /// Input format
    #[clap(long, arg_enum, default_value_t = input::Format::Asciicast)]
    in_fmt: input::Format,

    /// HTTP listen address [default: 0.0.0.0:8765]
    #[clap(short, long, default_missing_value = "0.0.0.0:8765")]
    listen_addr: Option<String>,

    /// Set terminal width fallback
    #[clap(long)]
    cols: Option<usize>,

    /// Set terminal height fallback
    #[clap(long)]
    rows: Option<usize>,

    /// Enable verbose logging
    #[clap(short, long)]
    verbose: bool,
}

#[derive(Debug)]
pub struct ClientInitResponse {
    online: bool,
    stream_time: f32,
    cols: usize,
    rows: usize,
    init: String,
    broadcast_rx: broadcast::Receiver<client::Event>,
}

impl ClientInitResponse {
    fn new(
        online: bool,
        stream_time: f32,
        vt: &Vt,
        broadcast_rx: broadcast::Receiver<client::Event>,
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
    mut input_rx: mpsc::Receiver<input::Event>,
    mut clients_rx: mpsc::Receiver<ClientInitRequest>,
) -> Option<()> {
    debug!("default term size: {default_cols}x{default_rows}");
    let mut vt = Vt::new(default_cols, default_rows);
    let (broadcast_tx, _) = broadcast::channel(1024);
    let mut last_stream_time = 0.0;
    let mut last_feed_time = Instant::now();
    let mut online = false;

    loop {
        tokio::select! {
            value = input_rx.recv() => {
                let event = value?;

                match &event {
                    input::Event::Reset(size) => {
                        let (cols, rows) = size.unwrap_or((default_cols, default_rows));
                        vt = Vt::new(cols, rows);
                        last_stream_time = 0.0;
                        last_feed_time = Instant::now();
                        online = true;
                        let _ = broadcast_tx.send(client::Event::Reset((cols, rows), 0.0, None));
                        debug!("stream reset ({cols}x{rows})");
                    }

                    input::Event::Stdout(time, data) => {
                        vt.feed_str(data);
                        last_stream_time = *time;
                        last_feed_time = Instant::now();
                        let _ = broadcast_tx.send(client::Event::Stdout(*time, data.clone()));
                    }

                    input::Event::Closed => {
                        debug!("stream closed");
                        online = false;
                        let _ = broadcast_tx.send(client::Event::Offline);
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match (
        &cli.input.is_some(),
        atty::is(atty::Stream::Stdin),
        atty::is(atty::Stream::Stderr),
        cli.verbose,
    ) {
        (false, false, true, _) => "off",
        (_, _, _, false) => "info",
        (_, _, _, true) => "debug",
    };

    env_logger::Builder::from_env(Env::default().default_filter_or(log_level)).init();

    let (input_tx, input_rx) = mpsc::channel(1024);
    let (clients_tx, clients_rx) = mpsc::channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    if let Some(listen_addr) = cli.listen_addr {
        let addr = listen_addr.parse()?;
        tokio::spawn(server::serve(addr, clients_tx.clone(), shutdown_rx)?);
    }

    if let Some(url) = cli.forward_url {
        tokio::spawn(forwarder::forward(clients_tx, url));
    }

    let term_size = termion::terminal_size().unwrap_or((80, 24));
    let cols = cli.cols.unwrap_or(term_size.0 as usize);
    let rows = cli.rows.unwrap_or(term_size.1 as usize);
    tokio::spawn(handle_events(cols, rows, input_rx, clients_rx));

    let result = tokio::spawn(input::read(cli.input, cli.in_fmt, input_tx)).await;
    debug!("reader finished: {:?}", &result);
    let _ = shutdown_tx.send(());
    result??;

    Ok(())
}
