//! HTTP + WebSocket surface. One page, one socket, one JSON protocol.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::mpsc::unbounded_channel;

use crate::state::App;

const PAGE: &str = include_str!("static/index.html");

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/ws", any(socket))
        .with_state(app)
}

async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE).into_response()
}

async fn state(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(app.last_state())
}

async fn socket(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |sock| session(app, sock))
}

async fn session(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = unbounded_channel::<Message>();
    app.add_client(tx.clone());
    let _ = sink.send(Message::Text(app.last_state().to_string())).await;

    // One pump owns the write half, interleaving JSON state pushes with the
    // captured PCM frames so neither starves the other.
    let mut audio = app.subscribe_audio();
    let dropped = app.audio_dropped.clone();
    let pump = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => { if sink.send(m).await.is_err() { break } }
                    None => break,
                },
                frame = audio.recv() => match frame {
                    Ok(bytes) => {
                        if sink.send(Message::Binary(bytes.into())).await.is_err() { break }
                    }
                    // The socket stalled long enough for frames to fall out of
                    // the queue. Those are real chunks of audio, so count them
                    // rather than pretending the stream is intact.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        dropped.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            continue;
        };
        let reply = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(cmd) => match app.command(&cmd).await {
                Ok(s) if s.is_empty() => None,
                Ok(s) => Some(s),
                Err(e) => Some(format!("error: {e}")),
            },
            Err(e) => Some(format!("error: bad request ({e})")),
        };
        if let Some(r) = reply {
            let _ = tx.send(Message::Text(
                serde_json::json!({ "toast": r }).to_string(),
            ));
        }
    }

    pump.abort();
    app.drop_client(&tx);
}
