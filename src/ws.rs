use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::models::UpdateEvent;

pub async fn handle_ws_connection(
    stream: WebSocket,
    mut updates: broadcast::Receiver<UpdateEvent>,
) {
    let (mut sender, mut receiver) = stream.split();

    loop {
        tokio::select! {
            result = updates.recv() => {
                match result {
                    Ok(event) => {
                        if let Ok(message) = serde_json::to_string(&event) {
                            if sender.send(Message::Text(message.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
}
