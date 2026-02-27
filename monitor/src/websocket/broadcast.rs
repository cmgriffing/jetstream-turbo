use crate::stats::StreamStats;
use crate::storage::Storage;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use std::sync::Arc;
use tokio::sync::broadcast;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State((tx, _, _)): State<(Arc<broadcast::Sender<StreamStats>>, Arc<Storage>, Arc<std::sync::RwLock<crate::stats::UptimeTracker>>)>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, tx.subscribe()))
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
