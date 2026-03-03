use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tracing::error;

use crate::models::UserScopedUpdateEvent;

pub async fn handle_ws_connection(
    stream: WebSocket,
    user_id: i64,
    mut updates: broadcast::Receiver<UserScopedUpdateEvent>,
) {
    let (mut sender, mut receiver) = stream.split();

    loop {
        tokio::select! {
            result = updates.recv() => {
                match result {
                    Ok(user_event) => {
                        if user_event.user_id != user_id {
                            continue;
                        }

                        match serde_json::to_string(&user_event.event) {
                            Ok(message) => {
                                if sender.send(Message::Text(message.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                error!("Failed to serialize UpdateEvent for WebSocket: {err}");
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
