use anyhow::{bail, Result};
use clap::ArgEnum;
use log::info;
use regex::Regex;
use serde::Deserialize;
use std::pin::Pin;
use std::time;
use tokio::fs::File;
use tokio::io::AsyncBufReadExt;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

#[derive(Clone, Debug, ArgEnum)]
pub enum Format {
    Asciicast,
    Raw,
}

#[derive(Deserialize)]
pub struct Header {
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone)]
pub enum Event {
    Reset(Option<(usize, usize)>),
    Stdout(f32, String),
    Closed,
}

type Reader = Pin<Box<dyn AsyncRead + Send + 'static>>;

pub async fn read(
    filename: Option<String>,
    format: Format,
    stream_tx: mpsc::Sender<Event>,
) -> Result<()> {
    let mut file: Reader = match &filename {
        Some(filename) => {
            info!("reading from {filename}");
            Box::pin(Box::new(File::open(filename).await?))
        }

        None => {
            info!("reading from stdin");
            Box::pin(Box::new(tokio::io::stdin()))
        }
    };

    loop {
        match format {
            Format::Asciicast => read_asciicast_file(file, &stream_tx).await?,
            Format::Raw => read_raw_file(file, &stream_tx).await?,
        }

        stream_tx.send(Event::Closed).await?;

        if let Some(filename) = &filename {
            file = Box::pin(Box::new(File::open(filename).await?));
        } else {
            break;
        }
    }

    Ok(())
}

async fn read_asciicast_file<F>(file: F, stream_tx: &mpsc::Sender<Event>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
    use Event::*;

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
                let (time, event_type, text) = serde_json::from_str::<(f32, &str, String)>(&line)?;

                if event_type == "o" {
                    stream_tx.send(Stdout(time, text)).await?;
                }
            }

            _ => bail!("invalid input line"),
        }

        first_read = false;
    }

    Ok(())
}

lazy_static::lazy_static! {
    static ref SCRIPT_HEADER_RE: Regex = Regex::new(r#"\[.*COLUMNS="(\d{1,3})" LINES="(\d{1,3})".*\]"#).unwrap();
    static ref RESIZE_SEQ_RE: Regex = Regex::new(r#"\x1b\[8;(\d{1,3});(\d{1,3})t"#).unwrap();
}

async fn read_raw_file<F>(mut file: F, stream_tx: &mpsc::Sender<Event>) -> Result<()>
where
    F: AsyncReadExt + std::marker::Unpin,
{
    use Event::*;

    let mut buffer = [0; 1024];
    let now = time::Instant::now();

    if let Ok(n) = file.read(&mut buffer[..]).await {
        let text = String::from_utf8_lossy(&buffer[..n]).into_owned();

        let size = if let Some(caps) = SCRIPT_HEADER_RE.captures(&text) {
            let cols: usize = caps[1].parse().unwrap();
            let rows: usize = caps[2].parse().unwrap();
            Some((cols, rows))
        } else if let Some(caps) = RESIZE_SEQ_RE.captures(&text) {
            let cols: usize = caps[2].parse().unwrap();
            let rows: usize = caps[1].parse().unwrap();
            Some((cols, rows))
        } else {
            None
        };

        stream_tx.send(Reset(size)).await?;
        let time = now.elapsed().as_secs_f32();
        stream_tx.send(Stdout(time, text)).await?;
    }

    while let Ok(n) = file.read(&mut buffer[..]).await {
        if n == 0 {
            break;
        }

        let time = now.elapsed().as_secs_f32();
        let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
        stream_tx.send(Stdout(time, text)).await?;
    }

    Ok(())
}
