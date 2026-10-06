//! Sends signed events to Nostr relays: NIP-01 `EVENT` messages, answered
//! by `OK`. A small websocket client (RFC 6455, text frames only) over TCP,
//! with TLS for `wss://` relays checked against the bundled web roots. One
//! connection per relay and pass.

use std::{sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose};
use nostr::{event::Event, types::Url};
use serde_json::Value;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{self, ClientConfig, RootCertStore, pki_types::ServerName},
};

/// Connecting with both handshakes, and each relay reply, get this long.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
/// Relays answer `EVENT` with short messages; anything larger is refused.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const TEXT: u8 = 0x1;
const BINARY: u8 = 0x2;
const CONTINUATION: u8 = 0x0;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xA;

/// Delivers events to a relay.
pub trait Transport: Send + Sync {
    /// Sends `events` to `relay` in order. An error means the relay could
    /// not be reached and nothing is known to be sent; otherwise there is
    /// one result per event, `Err` with the relay's reason when refused.
    fn publish(
        &self,
        relay: &str,
        events: &[Event],
    ) -> impl Future<Output = Result<Vec<Result<(), String>>, String>> + Send;
}

/// [`Transport`] over websockets.
pub struct WebSocketTransport {
    tls: TlsConnector,
}

impl WebSocketTransport {
    pub fn new() -> Result<Self, String> {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS setup: {error}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self {
            tls: TlsConnector::from(Arc::new(config)),
        })
    }

    async fn connect(&self, relay: &str) -> Result<WebSocket, String> {
        let url = Url::parse(relay).map_err(|error| format!("invalid relay URL: {error}"))?;
        let secure = match url.scheme() {
            "wss" => true,
            "ws" => false,
            other => return Err(format!("unsupported relay scheme {other}")),
        };
        let host = url
            .host_str()
            .ok_or("relay URL has no host")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = url.port().unwrap_or(if secure { 443 } else { 80 });
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|error| format!("connect: {error}"))?;
        let _ = tcp.set_nodelay(true);
        let stream: Box<dyn Socket> = if secure {
            let name = ServerName::try_from(host.clone())
                .map_err(|error| format!("invalid relay host: {error}"))?;
            Box::new(
                self.tls
                    .connect(name, tcp)
                    .await
                    .map_err(|error| format!("TLS: {error}"))?,
            )
        } else {
            Box::new(tcp)
        };
        let mut target = url.path().to_owned();
        if target.is_empty() {
            target.push('/');
        }
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        let authority = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or(&host)),
            None => url.host_str().unwrap_or(&host).to_owned(),
        };
        WebSocket::open(stream, &authority, &target).await
    }
}

impl Transport for WebSocketTransport {
    async fn publish(
        &self,
        relay: &str,
        events: &[Event],
    ) -> Result<Vec<Result<(), String>>, String> {
        let mut socket = timeout(STEP_TIMEOUT, self.connect(relay))
            .await
            .map_err(|_| "timed out connecting".to_owned())??;
        let mut results = Vec::with_capacity(events.len());
        for event in events {
            let reply = async {
                let message = serde_json::json!(["EVENT", event]).to_string();
                socket.send(TEXT, message.as_bytes()).await?;
                socket.accepted(&event.id.to_hex()).await
            };
            match timeout(STEP_TIMEOUT, reply).await {
                Ok(Ok(result)) => results.push(result),
                // The connection is lost: the rest are not sent.
                Ok(Err(error)) => {
                    results.resize(events.len(), Err(error));
                    return Ok(results);
                }
                Err(_) => {
                    results.resize(events.len(), Err("no reply from the relay".to_owned()));
                    return Ok(results);
                }
            }
        }
        let _ = socket.send(CLOSE, &[]).await;
        Ok(results)
    }
}

trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}

/// One websocket connection. Clients mask what they send; servers do not.
struct WebSocket {
    stream: Box<dyn Socket>,
    /// Bytes read past the handshake, consumed before the stream.
    pending: Vec<u8>,
    masked: bool,
}

impl WebSocket {
    /// Upgrades `stream` with the client handshake and checks the
    /// server's `Sec-WebSocket-Accept`.
    async fn open(mut stream: Box<dyn Socket>, host: &str, target: &str) -> Result<Self, String> {
        let key = general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\nUser-Agent: noaa-oracle/{}\r\n\r\n",
            env!("CARGO_PKG_VERSION")
        );
        let io = |error: std::io::Error| format!("handshake: {error}");
        stream.write_all(request.as_bytes()).await.map_err(io)?;
        stream.flush().await.map_err(io)?;
        let (head, pending) = read_head(&mut stream).await?;
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            return Err(format!("relay refused the websocket upgrade: {status}"));
        }
        let accept = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-accept")
                .then(|| value.trim().to_owned())
        });
        if accept.as_deref() != Some(accept_key(&key).as_str()) {
            return Err("relay answered the upgrade with a wrong accept key".to_owned());
        }
        Ok(Self {
            stream,
            pending,
            masked: true,
        })
    }

    async fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let mask_bit = if self.masked { 0x80 } else { 0 };
        match payload.len() {
            length if length < 126 => frame.push(mask_bit | length as u8),
            length if length <= usize::from(u16::MAX) => {
                frame.push(mask_bit | 126);
                frame.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                frame.push(mask_bit | 127);
                frame.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        if self.masked {
            let mask: [u8; 4] = rand::random();
            frame.extend_from_slice(&mask);
            frame.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(index, byte)| byte ^ mask[index % 4]),
            );
        } else {
            frame.extend_from_slice(payload);
        }
        let io = |error: std::io::Error| format!("send: {error}");
        self.stream.write_all(&frame).await.map_err(io)?;
        self.stream.flush().await.map_err(io)
    }

    async fn read(&mut self, length: usize) -> Result<Vec<u8>, String> {
        let buffered = length.min(self.pending.len());
        let mut bytes: Vec<u8> = self.pending.drain(..buffered).collect();
        if bytes.len() < length {
            let mut rest = vec![0; length - bytes.len()];
            self.stream
                .read_exact(&mut rest)
                .await
                .map_err(|error| format!("receive: {error}"))?;
            bytes.extend(rest);
        }
        Ok(bytes)
    }

    /// The next text message. Answers pings and skips binary messages.
    async fn receive(&mut self) -> Result<String, String> {
        let mut message = Vec::new();
        let mut message_opcode = None;
        loop {
            let header = self.read(2).await?;
            let fin = header[0] & 0x80 != 0;
            let opcode = header[0] & 0x0F;
            let length = match header[1] & 0x7F {
                126 => {
                    let bytes = self.read(2).await?;
                    u64::from(u16::from_be_bytes([bytes[0], bytes[1]]))
                }
                127 => {
                    let bytes = self.read(8).await?;
                    u64::from_be_bytes(bytes.try_into().unwrap_or([0xFF; 8]))
                }
                length => u64::from(length),
            };
            let length = usize::try_from(length)
                .ok()
                .filter(|length| message.len() + length <= MAX_MESSAGE_BYTES)
                .ok_or("relay message too large")?;
            let mask = if header[1] & 0x80 != 0 {
                Some(self.read(4).await?)
            } else {
                None
            };
            let mut payload = self.read(length).await?;
            if let Some(mask) = mask {
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % 4];
                }
            }
            match opcode {
                CLOSE => return Err("relay closed the connection".to_owned()),
                PING => self.send(PONG, &payload).await?,
                PONG => {}
                TEXT | BINARY | CONTINUATION => {
                    if opcode != CONTINUATION {
                        message_opcode = Some(opcode);
                        message.clear();
                    }
                    message.extend(payload);
                    if fin {
                        if message_opcode == Some(TEXT) {
                            return String::from_utf8(std::mem::take(&mut message))
                                .map_err(|_| "relay sent text that is not UTF-8".to_owned());
                        }
                        message.clear();
                        message_opcode = None;
                    }
                }
                other => return Err(format!("relay sent unknown opcode {other}")),
            }
        }
    }

    /// Waits for the relay's `OK` for `event_id`, skipping other messages.
    /// A refusal is `Ok(Err(reason))`; a lost connection is `Err`. NIP-01
    /// relays answer a duplicate with `false` and a `duplicate:` prefix
    /// when they already hold the event, which counts as accepted.
    async fn accepted(&mut self, event_id: &str) -> Result<Result<(), String>, String> {
        loop {
            let text = self.receive().await?;
            let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if parts.first().and_then(Value::as_str) != Some("OK")
                || parts.get(1).and_then(Value::as_str) != Some(event_id)
            {
                continue;
            }
            let accepted = parts.get(2).and_then(Value::as_bool).unwrap_or(false);
            let reason = parts.get(3).and_then(Value::as_str).unwrap_or_default();
            return Ok(if accepted || reason.starts_with("duplicate:") {
                Ok(())
            } else {
                Err(format!("relay refused the event: {reason}"))
            });
        }
    }
}

/// Reads an HTTP head up to the blank line. Returns it and the bytes after.
async fn read_head<S: AsyncRead + Unpin + ?Sized>(
    stream: &mut S,
) -> Result<(String, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            return Ok((head, buffer[end + 4..].to_vec()));
        }
        if buffer.len() > MAX_HANDSHAKE_BYTES {
            return Err("handshake reply too large".to_owned());
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("handshake: {error}"))?;
        if read == 0 {
            return Err("relay closed the connection during the handshake".to_owned());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// `Sec-WebSocket-Accept` for a client `key` (RFC 6455 section 4.2.2).
fn accept_key(key: &str) -> String {
    let digest = Sha1::new()
        .chain_update(key.as_bytes())
        .chain_update(WEBSOCKET_GUID.as_bytes())
        .finalize();
    general_purpose::STANDARD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{event::EventBuilder, event::FinalizeEvent, event::Kind, key::Keys};
    use tokio::net::TcpListener;

    #[test]
    fn accept_key_matches_rfc_6455() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRxw0BBA="
        );
    }

    /// A relay that accepts the first event and refuses the second, after
    /// a ping and a notice, so the client's framing is exercised both ways.
    async fn relay(listener: TcpListener) {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut stream: Box<dyn Socket> = Box::new(tcp);
        let (head, pending) = read_head(&mut stream).await.unwrap();
        let key = head
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap()
            .to_owned();
        let reply = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            accept_key(&key)
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
        let mut socket = WebSocket {
            stream,
            pending,
            masked: false,
        };
        for accepted in [true, false] {
            let message: Value = serde_json::from_str(&socket.receive().await.unwrap()).unwrap();
            assert_eq!(message[0], "EVENT");
            let event = Event::from_json(message[1].to_string()).unwrap();
            event.verify().unwrap();
            socket.send(PING, b"hi").await.unwrap();
            socket
                .send(TEXT, br#"["NOTICE","slow down"]"#)
                .await
                .unwrap();
            let reply = serde_json::json!(["OK", event.id.to_hex(), accepted, "blocked: test"]);
            socket
                .send(TEXT, reply.to_string().as_bytes())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn events_are_sent_and_each_reply_is_read() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(relay(listener));
        let keys = Keys::generate();
        let events: Vec<Event> = ["first", "second"]
            .into_iter()
            .map(|content| {
                EventBuilder::new(Kind::Custom(30078), content.repeat(100))
                    .finalize(&keys)
                    .unwrap()
            })
            .collect();
        let transport = WebSocketTransport::new().unwrap();
        let results = transport
            .publish(&format!("ws://{address}/"), &events)
            .await
            .unwrap();
        assert_eq!(results[0], Ok(()));
        assert_eq!(
            results[1],
            Err("relay refused the event: blocked: test".to_owned())
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_relay_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let transport = WebSocketTransport::new().unwrap();
        assert!(
            transport
                .publish(&format!("ws://{address}"), &[])
                .await
                .is_err()
        );
    }
}
