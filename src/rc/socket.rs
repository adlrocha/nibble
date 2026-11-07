//! Slack Socket Mode: a WebSocket the daemon opens to Slack, so no public
//! URL is needed.
//!
//! Slack sends JSON envelopes (`hello`, `events_api`, `interactive`,
//! `slash_commands`, `disconnect`); each envelope with an `envelope_id` must
//! be acknowledged within 3 seconds, optionally with a response payload
//! (modal validation errors travel that way).
//!
//! The socket is read with a short timeout so one thread can both serve
//! Slack and poll nibble's tasks between frames. TLS goes through rustls with
//! the system trust store, the same roots ureq uses.

use anyhow::{bail, Context, Result};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use tungstenite::{Message, WebSocket};

/// One Socket Mode envelope.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Envelope {
    /// Connection is ready.
    Hello,
    /// Slack asks us to reconnect (routine refresh, or shutting down).
    Disconnect { reason: String },
    /// An Events API event (`payload.event`).
    Event { envelope_id: String, payload: Value },
    /// A button click, modal submission, … (`payload.type`).
    Interactive { envelope_id: String, payload: Value },
    /// A slash command invocation.
    SlashCommand { envelope_id: String, payload: Value },
    /// Anything else; acknowledged and ignored.
    Other {
        envelope_id: Option<String>,
        kind: String,
    },
}

impl Envelope {
    /// The id to acknowledge, if the envelope needs one.
    pub(crate) fn envelope_id(&self) -> Option<&str> {
        match self {
            Envelope::Event { envelope_id, .. }
            | Envelope::Interactive { envelope_id, .. }
            | Envelope::SlashCommand { envelope_id, .. } => Some(envelope_id),
            Envelope::Other { envelope_id, .. } => envelope_id.as_deref(),
            Envelope::Hello | Envelope::Disconnect { .. } => None,
        }
    }
}

/// Parse one text frame.
pub(crate) fn parse_envelope(text: &str) -> Result<Envelope> {
    let v: Value = serde_json::from_str(text).context("Socket Mode frame is not JSON")?;
    let kind = v["type"].as_str().unwrap_or_default().to_string();
    let id = v["envelope_id"].as_str().map(str::to_string);
    let payload = v["payload"].clone();
    let need_id =
        |id: Option<String>| id.with_context(|| format!("{kind} envelope without envelope_id"));
    Ok(match kind.as_str() {
        "hello" => Envelope::Hello,
        "disconnect" => Envelope::Disconnect {
            reason: v["reason"].as_str().unwrap_or("unknown").to_string(),
        },
        "events_api" => Envelope::Event {
            envelope_id: need_id(id)?,
            payload,
        },
        "interactive" => Envelope::Interactive {
            envelope_id: need_id(id)?,
            payload,
        },
        "slash_commands" => Envelope::SlashCommand {
            envelope_id: need_id(id)?,
            payload,
        },
        _ => Envelope::Other {
            envelope_id: id,
            kind,
        },
    })
}

/// The acknowledgement frame for `envelope_id`, with an optional response
/// payload.
pub(crate) fn ack_frame(envelope_id: &str, payload: Option<&Value>) -> String {
    match payload {
        Some(p) => json!({ "envelope_id": envelope_id, "payload": p }).to_string(),
        None => json!({ "envelope_id": envelope_id }).to_string(),
    }
}

type Tls = StreamOwned<ClientConnection, TcpStream>;

/// An open Socket Mode connection.
pub(crate) struct Connection {
    ws: WebSocket<Tls>,
}

/// What one read produced.
pub(crate) enum Read {
    /// A text frame.
    Text(String),
    /// A ping, pong or other non-text frame: the connection is alive.
    Control,
    /// Nothing arrived within the read timeout.
    Idle,
}

/// How long a read waits before returning [`Read::Idle`].
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// How long connecting and the WebSocket handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

fn tls_config() -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    let certs =
        rustls_native_certs::load_native_certs().context("failed to load system TLS roots")?;
    let (added, _) = roots.add_parsable_certificates(certs);
    if added == 0 {
        bail!("no usable system TLS root certificates");
    }
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("TLS configuration")?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Connect to a `wss://` URL from `apps.connections.open`.
pub(crate) fn connect(url: &str) -> Result<Connection> {
    let uri: tungstenite::http::Uri = url.parse().context("invalid Socket Mode URL")?;
    if uri.scheme_str() != Some("wss") {
        bail!("Socket Mode URL must be wss://");
    }
    let host = uri
        .host()
        .context("Socket Mode URL has no host")?
        .to_string();
    let port = uri.port_u16().unwrap_or(443);

    let addrs: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))
        .with_context(|| format!("cannot resolve {host}"))?
        .collect();
    // Dual-stack hosts: an unusable first record (IPv6 without IPv6 egress)
    // must not hide a working one.
    let mut last_err = None;
    let tcp = addrs
        .iter()
        .find_map(
            |addr| match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
                Ok(tcp) => Some(tcp),
                Err(e) => {
                    last_err = Some(e);
                    None
                }
            },
        )
        .with_context(|| match &last_err {
            Some(e) => format!("cannot connect to {host}: {e}"),
            None => format!("no address for {host}"),
        })?;
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let name = ServerName::try_from(host.clone()).context("invalid TLS server name")?;
    let conn = ClientConnection::new(tls_config()?, name).context("TLS setup")?;
    let tls = StreamOwned::new(conn, tcp);

    let (ws, _resp) = tungstenite::client(url, tls)
        .map_err(|e| anyhow::anyhow!("WebSocket handshake failed: {e}"))?;
    ws.get_ref().sock.set_read_timeout(Some(READ_TIMEOUT))?;
    Ok(Connection { ws })
}

impl Connection {
    /// Read the next frame, or report idleness after the read timeout.
    pub(crate) fn read(&mut self) -> Result<Read> {
        match self.ws.read() {
            Ok(Message::Text(t)) => Ok(Read::Text(t.to_string())),
            Ok(Message::Close(frame)) => bail!("Slack closed the socket ({frame:?})"),
            Ok(_) => Ok(Read::Control), // ping/pong/binary; pongs are queued by tungstenite
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // Push out any queued pong.
                match self.ws.flush() {
                    Ok(()) => Ok(Read::Idle),
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                    {
                        Ok(Read::Idle)
                    }
                    Err(e) => Err(anyhow::anyhow!("Socket Mode write failed: {e}")),
                }
            }
            Err(e) => Err(anyhow::anyhow!("Socket Mode read failed: {e}")),
        }
    }

    /// Send a text frame (acknowledgements).
    pub(crate) fn send(&mut self, text: String) -> Result<()> {
        self.ws
            .send(Message::text(text))
            .map_err(|e| anyhow::anyhow!("Socket Mode send failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_envelope_kind() {
        assert_eq!(
            parse_envelope(r#"{"type":"hello","num_connections":1}"#).unwrap(),
            Envelope::Hello
        );
        assert_eq!(
            parse_envelope(r#"{"type":"disconnect","reason":"refresh_requested"}"#).unwrap(),
            Envelope::Disconnect {
                reason: "refresh_requested".into()
            }
        );
        let ev = parse_envelope(
            r#"{"type":"events_api","envelope_id":"e1","payload":{"event":{"type":"message"}}}"#,
        )
        .unwrap();
        assert_eq!(ev.envelope_id(), Some("e1"));
        assert!(
            matches!(ev, Envelope::Event { ref payload, .. } if payload["event"]["type"] == "message")
        );
        let it = parse_envelope(
            r#"{"type":"interactive","envelope_id":"e2","payload":{"type":"block_actions"}}"#,
        )
        .unwrap();
        assert!(matches!(it, Envelope::Interactive { .. }));
        let sc = parse_envelope(
            r#"{"type":"slash_commands","envelope_id":"e3","payload":{"command":"/nibble"}}"#,
        )
        .unwrap();
        assert!(matches!(sc, Envelope::SlashCommand { .. }));
        let other = parse_envelope(r#"{"type":"future_thing","envelope_id":"e4"}"#).unwrap();
        assert_eq!(other.envelope_id(), Some("e4"));
    }

    #[test]
    fn rejects_bad_frames() {
        assert!(parse_envelope("not json").is_err());
        assert!(parse_envelope(r#"{"type":"events_api","payload":{}}"#).is_err());
    }

    #[test]
    fn ack_frames_carry_optional_payload() {
        assert_eq!(ack_frame("e1", None), r#"{"envelope_id":"e1"}"#);
        let with = ack_frame("e2", Some(&json!({"response_action": "clear"})));
        let v: Value = serde_json::from_str(&with).unwrap();
        assert_eq!(v["payload"]["response_action"], "clear");
    }

    #[test]
    fn connect_rejects_non_wss_urls() {
        let err = connect("https://example.com/x").err().unwrap().to_string();
        assert!(err.contains("wss://"), "{err}");
    }

    /// TLS + WebSocket against a public echo server:
    /// `cargo test -- --ignored websocket_echo`.
    #[test]
    #[ignore = "needs network"]
    fn websocket_echo() {
        let mut conn = connect("wss://ws.postman-echo.com/raw").unwrap();
        conn.send("nibble-probe".to_string()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Read::Text(t) = conn.read().unwrap() {
                assert_eq!(t, "nibble-probe");
                return;
            }
        }
        panic!("no echo within 10s");
    }
}
