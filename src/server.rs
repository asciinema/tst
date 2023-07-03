use crate::alis;
use crate::{event_stream, ClientInitRequest, StreamEvent};
use anyhow::Result;
use futures_util::{stream, FutureExt, Stream, StreamExt};
use log::{debug, info};
use rust_embed::RustEmbed;
use std::convert::Infallible;
use std::future;
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};
use warp::http::{self, Response};
use warp::hyper::Body;
use warp::sse;
use warp::ws;
use warp::{Filter, Reply};

#[derive(RustEmbed)]
#[folder = "public"]
struct Assets;

pub fn serve(
    listen_addr: SocketAddr,
    clients_tx: mpsc::Sender<ClientInitRequest>,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<tokio::task::JoinHandle<()>> {
    let ws_clients_tx = clients_tx.clone();
    let ws_clients_tx = warp::any().map(move || ws_clients_tx.clone());
    let ws_route = warp::path("ws")
        .and(warp::addr::remote())
        .map(Option::unwrap)
        .and(warp::ws())
        .and(ws_clients_tx)
        .map(ws_handler);

    let sse_clients_tx = warp::any().map(move || clients_tx.clone());
    let sse_route = warp::path("sse")
        .and(warp::addr::remote())
        .map(Option::unwrap)
        .and(warp::get())
        .and(sse_clients_tx)
        .then(sse_handler);

    let routes = ws_route.or(sse_route).or(warp_embed::embed(&Assets));

    info!("streaming via WebSocket at ws://{}/ws", listen_addr);
    info!("streaming via SSE at http://{}/sse", listen_addr);
    info!("serving assets from ./public at http://{}", listen_addr);
    let signal = shutdown_rx.map(|_| ());
    let (_, server) = warp::serve(routes).try_bind_with_graceful_shutdown(listen_addr, signal)?;
    let handle = tokio::spawn(server);

    Ok(handle)
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

async fn handle_websocket(
    websocket: ws::WebSocket,
    clients_tx: mpsc::Sender<ClientInitRequest>,
) -> Result<()> {
    let s1 = alis::stream(&clients_tx).await?.map(ws::Message::binary);
    let s2 = stream::once(future::ready(ws::Message::close_with(1000u16, "done")));
    s1.chain(s2).map(Ok).forward(websocket).await?;

    Ok(())
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

async fn sse_stream(
    clients_tx: mpsc::Sender<ClientInitRequest>,
) -> Result<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let s1 = event_stream(&clients_tx).await?.map(|e| e.into());
    let s2 = stream::iter(vec![sse::Event::default().event("done").data("done")]);
    let stream = s1.chain(s2).map(Ok);

    Ok(stream)
}

impl From<StreamEvent> for sse::Event {
    fn from(event: StreamEvent) -> Self {
        use StreamEvent::*;

        let json = match event {
            Reset((cols, rows), time, init) => serde_json::json!({
                "cols": cols,
                "rows": rows,
                "time": time,
                "init": init,
            }),

            Stdout(time, data) => serde_json::json!((time, "o", data)),

            Offline => serde_json::json!({ "status": "offline" }),
        };

        sse::Event::default().data(json.to_string())
    }
}
