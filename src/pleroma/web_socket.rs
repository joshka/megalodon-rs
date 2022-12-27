use std::fmt;
use std::thread;
use std::time::Duration;

use super::entities;
use crate::error::{Error, Kind};
use crate::streaming::{Message, Streaming};
use serde::Deserialize;

use futures_util::{SinkExt, StreamExt};
use tokio::runtime::Runtime;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::frame::coding::CloseCode,
    tungstenite::protocol::Message as WebSocketMessage,
};
use url::Url;

const RECONNECT_INTERVAL: u64 = 5000;
const READ_MESSAGE_TIMEOUT_SECONDS: u64 = 60;

#[derive(Debug, Clone)]
pub struct WebSocket {
    url: String,
    stream: String,
    params: Option<Vec<String>>,
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
    event: String,
    payload: String,
}

impl WebSocket {
    pub fn new(
        url: String,
        stream: String,
        params: Option<Vec<String>>,
        access_token: Option<String>,
    ) -> Self {
        Self {
            url,
            stream,
            params,
            access_token,
        }
    }

    fn parse(&self, message: WebSocketMessage) -> Result<Message, Error> {
        if message.is_ping() || message.is_pong() {
            Ok(Message::Heartbeat())
        } else if message.is_text() {
            let text = message.to_text()?;
            let mes = serde_json::from_str::<RawMessage>(text)?;
            match &*mes.event {
                "update" => {
                    let res =
                        serde_json::from_str::<entities::Status>(&mes.payload).map_err(|e| {
                            log::error!(
                                "failed to parse status: {}\n{}",
                                e.to_string(),
                                &mes.payload
                            );
                            e
                        })?;
                    Ok(Message::Update(res.into()))
                }
                "notification" => {
                    let res = serde_json::from_str::<entities::Notification>(&mes.payload)
                        .map_err(|e| {
                            log::error!(
                                "failed to parse notification: {}\n{}",
                                e.to_string(),
                                &mes.payload
                            );
                            e
                        })?;
                    Ok(Message::Notification(res.into()))
                }
                "conversation" => {
                    let res = serde_json::from_str::<entities::Conversation>(&mes.payload)
                        .map_err(|e| {
                            log::error!(
                                "failed to parse conversation: {}\n{}",
                                e.to_string(),
                                &mes.payload
                            );
                            e
                        })?;
                    Ok(Message::Conversation(res.into()))
                }
                "delete" => Ok(Message::Delete(mes.payload)),
                event => Err(Error::new_own(
                    format!("Unknown event is received: {}", event),
                    Kind::ParseError,
                    None,
                    None,
                )),
            }
        } else {
            Err(Error::new_own(
                String::from("Receiving message is not ping, pong or text"),
                Kind::ParseError,
                None,
                None,
            ))
        }
    }

    fn connect(&self, url: &str, callback: Box<dyn Fn(Message)>) {
        loop {
            match Runtime::new()
                .unwrap()
                .block_on(self.do_connect(url, &callback))
            {
                Ok(()) => {
                    log::info!("connection for {} is  closed", url);
                    return;
                }
                Err(err) => match err.kind {
                    InnerKind::ConnectionError
                    | InnerKind::SocketReadError
                    | InnerKind::UnusualSocketCloseError
                    | InnerKind::TimeoutError => {
                        thread::sleep(Duration::from_millis(RECONNECT_INTERVAL));
                        log::info!("Reconnecting to {}", url);
                        continue;
                    }
                },
            }
        }
    }

    async fn do_connect(
        &self,
        url: &str,
        callback: &Box<dyn Fn(Message)>,
    ) -> Result<(), InnerError> {
        let (mut socket, response) =
            connect_async(Url::parse(url).unwrap()).await.map_err(|e| {
                log::error!("Failed to connect: {}", e);
                InnerError::new(InnerKind::ConnectionError)
            })?;

        log::debug!("Connected to {}", url);
        log::debug!("Response HTTP code: {}", response.status());
        log::debug!("Response contains the following headers:");
        for (ref header, _value) in response.headers() {
            log::debug!("* {}", header);
        }

        loop {
            let res = tokio::time::timeout(
                Duration::from_secs(READ_MESSAGE_TIMEOUT_SECONDS),
                socket.next(),
            )
            .await
            .map_err(|e| {
                log::error!("Timeout reading message: {}", e);
                InnerError::new(InnerKind::TimeoutError)
            })?;
            let Some(r) = res else {
                log::warn!("Response is empty");
                continue;
            };
            let msg = r.map_err(|e| {
                log::error!("Failed to read message: {}", e);
                InnerError::new(InnerKind::SocketReadError)
            })?;
            if msg.is_ping() {
                let _ = socket
                    .send(WebSocketMessage::Pong(Vec::<u8>::new()))
                    .await
                    .map_err(|e| {
                        log::error!("{:#?}", e);
                        e
                    });
            }
            if msg.is_close() {
                let _ = socket.close(None).await.map_err(|e| {
                    log::error!("{:#?}", e);
                    e
                });
                if let WebSocketMessage::Close(Some(close)) = msg {
                    log::warn!("Connection to {} is closed because {}", url, close.code);
                    if close.code != CloseCode::Normal {
                        return Err(InnerError::new(InnerKind::UnusualSocketCloseError));
                    }
                }
                return Ok(());
            }
            match self.parse(msg) {
                Ok(message) => {
                    callback(message);
                }
                Err(err) => {
                    log::warn!("{}", err);
                }
            }
        }
    }
}

impl Streaming for WebSocket {
    fn listen(&self, callback: Box<dyn Fn(Message)>) {
        let mut parameter = Vec::<String>::from([format!("stream={}", self.stream)]);
        if let Some(access_token) = &self.access_token {
            parameter.push(format!("access_token={}", access_token));
        }
        if let Some(mut params) = self.params.clone() {
            parameter.append(&mut params);
        }
        let mut url = self.url.clone();
        url = url + "?" + parameter.join("&").as_str();

        self.connect(url.as_str(), callback);
    }
}

#[derive(thiserror::Error)]
#[error("{kind}")]
struct InnerError {
    kind: InnerKind,
}

#[derive(Debug, thiserror::Error)]
enum InnerKind {
    #[error("connection error")]
    ConnectionError,
    #[error("socket read error")]
    SocketReadError,
    #[error("unusual socket close error")]
    UnusualSocketCloseError,
    #[error("timeout error")]
    TimeoutError,
}

impl InnerError {
    pub fn new(kind: InnerKind) -> Self {
        Self { kind }
    }
}

impl fmt::Debug for InnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("megalodon::pleroma::web_socket::InnerError");

        builder.field("kind", &self.kind);
        builder.finish()
    }
}
