use crate::stats::StreamStats;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Minimal router state for the dashboard WebSocket.
#[derive(Clone)]
pub struct WsState {
    pub broadcast_tx: Arc<broadcast::Sender<StreamStats>>,
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<WsState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state.broadcast_tx.subscribe()))
}

async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<StreamStats>) {
    loop {
        match rx.recv().await {
            Ok(stats) => {
                let json = serde_json::to_string(&stats).unwrap();
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
        }
    }
}
