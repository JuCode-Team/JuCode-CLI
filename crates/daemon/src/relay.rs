//! The host side of the JuCode relay (`docs/relay-protocol.md`): one
//! outbound WebSocket to the relay, authenticated with the daemon's Ed25519
//! identity, carrying client streams. Each stream runs a Noise session
//! (`noise.rs`) and then serves a paired device exactly like a local
//! WebSocket client. The relay only sees ciphertext.

use crate::{
    hub::Hub,
    noise,
    store::{bytes_hash, write_private, Device, Store},
    Link, Received,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    net::TcpStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

const IDENTITY_FILE: &str = "relay-identity.json";
/// Signed together with the relay's nonce.
const AUTH_CONTEXT: &[u8] = b"jucode-relay-v1:";
/// The daemon setting that turns the relay on.
const SETTING: &str = "relay";
/// How long the relay's challenge and a client's first Noise message may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Short reads let one thread both receive and flush, as for local clients.
const POLL: Duration = Duration::from_millis(20);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The relay pings the host every 30 s. A connection this long without a
/// single frame is dead even if the socket never said so: a proxy or a
/// network change can drop it without the close ever reaching us.
const SILENCE: Duration = Duration::from_secs(75);

const OPEN: u8 = 1;
const DATA: u8 = 2;
const CLOSE: u8 = 3;

/// A binary frame on the host connection (§3.1).
#[derive(Debug, PartialEq)]
enum Frame {
    Open(u32),
    Data(u32, Vec<u8>),
    Close(u32),
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let (kind, stream, payload) = match self {
            Frame::Open(stream) => (OPEN, stream, &[][..]),
            Frame::Data(stream, payload) => (DATA, stream, payload.as_slice()),
            Frame::Close(stream) => (CLOSE, stream, &[][..]),
        };
        let mut bytes = Vec::with_capacity(5 + payload.len());
        bytes.push(kind);
        bytes.extend_from_slice(&stream.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (&kind, rest) = bytes.split_first()?;
        let stream = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
        if stream == 0 {
            return None;
        }
        match kind {
            OPEN => Some(Frame::Open(stream)),
            DATA => Some(Frame::Data(stream, rest[4..].to_vec())),
            CLOSE => Some(Frame::Close(stream)),
            _ => None,
        }
    }
}

/// The daemon's relay keys (§1), kept in `relay-identity.json`.
pub struct Identity {
    signing: SigningKey,
    static_private: Vec<u8>,
    static_public: Vec<u8>,
}

impl Identity {
    fn load_or_create(path: &std::path::Path) -> Result<Self, String> {
        if let Ok(text) = fs::read_to_string(path) {
            let saved: Value = serde_json::from_str(&text)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let key = |name: &str| {
                saved[name]
                    .as_str()
                    .and_then(|text| URL_SAFE_NO_PAD.decode(text).ok())
                    .ok_or_else(|| format!("{}: missing {name}", path.display()))
            };
            let seed: [u8; 32] = key("identity")?
                .try_into()
                .map_err(|_| format!("{}: bad identity key", path.display()))?;
            return Ok(Self {
                signing: SigningKey::from_bytes(&seed),
                static_private: key("static")?,
                static_public: key("static_pub")?,
            });
        }
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|error| error.to_string())?;
        let (static_private, static_public) = noise::generate_keypair()?;
        let identity = Self {
            signing: SigningKey::from_bytes(&seed),
            static_private,
            static_public,
        };
        let saved = json!({
            "identity": URL_SAFE_NO_PAD.encode(seed),
            "static": URL_SAFE_NO_PAD.encode(&identity.static_private),
            "static_pub": URL_SAFE_NO_PAD.encode(&identity.static_public),
        });
        write_private(path, format!("{saved:#}\n").as_bytes())
            .map_err(|error| error.to_string())?;
        Ok(identity)
    }

    /// First 16 bytes of SHA-256 of the Ed25519 public key, base64url.
    pub fn host_id(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.signing.verifying_key().as_bytes());
        URL_SAFE_NO_PAD.encode(&digest[..16])
    }
}

/// Relay configuration and state, shared by the relay thread and the ops
/// that report or change it.
pub struct Relay {
    /// Base URL; None when the relay is disabled for this process.
    url: Option<String>,
    enabled: AtomicBool,
    connected: AtomicBool,
    dir: PathBuf,
    identity: Mutex<Option<Arc<Identity>>>,
}

impl Relay {
    pub fn new(url: Option<String>, store: &Store) -> Self {
        Self {
            url: url.map(|url| url.trim_end_matches('/').to_string()),
            enabled: AtomicBool::new(store.setting(SETTING).as_bool().unwrap_or(false)),
            connected: AtomicBool::new(false),
            dir: store.dir().to_path_buf(),
            identity: Mutex::new(None),
        }
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    fn enabled(&self) -> bool {
        self.url.is_some() && self.enabled.load(Ordering::SeqCst)
    }

    /// The identity, created on first use.
    fn identity(&self) -> Result<Arc<Identity>, String> {
        let mut slot = self
            .identity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(identity) = slot.as_ref() {
            return Ok(Arc::clone(identity));
        }
        let identity = Arc::new(Identity::load_or_create(&self.dir.join(IDENTITY_FILE))?);
        *slot = Some(Arc::clone(&identity));
        Ok(identity)
    }

    /// Turns the relay on or off; persisted across restarts.
    pub fn set_enabled(&self, store: &Store, enabled: bool) -> Result<(), String> {
        if self.url.is_none() {
            return Err("the relay is disabled (--no-relay)".to_string());
        }
        store
            .set_setting(SETTING, json!(enabled))
            .map_err(|error| error.to_string())?;
        self.enabled.store(enabled, Ordering::SeqCst);
        Ok(())
    }

    pub fn status_json(&self) -> Value {
        json!({
            "type": "relay_status",
            "enabled": self.enabled(),
            "connected": self.connected.load(Ordering::SeqCst),
            "host": self.identity().ok().map(|identity| identity.host_id()),
            "url": self.url,
        })
    }

    /// A pairing code plus the link a phone opens to pair over the relay
    /// (§2). The page is served by the relay's host.
    pub fn pair_link(&self, hub: &Hub) -> Result<Value, String> {
        if !self.enabled() {
            return Err("the relay is off".to_string());
        }
        let identity = self.identity()?;
        let (code, expires_at) = hub.start_pairing()?;
        let link = format!(
            "{}/remote#pair={}.{}.{code}",
            app_origin(self.url.as_deref().unwrap_or_default()),
            identity.host_id(),
            URL_SAFE_NO_PAD.encode(&identity.static_public),
        );
        Ok(json!({ "type": "pair_link", "link": link, "code": code, "expires_at": expires_at }))
    }
}

/// `wss://host[:port]/relay/v1` → `https://host[:port]`.
fn app_origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("wss", url));
    let authority = rest.split('/').next().unwrap_or_default();
    let scheme = if scheme == "ws" { "http" } else { "https" };
    format!("{scheme}://{authority}")
}

/// The relay thread: keeps a host connection while the relay is enabled,
/// reconnecting with backoff (1 s doubling to 60 s, plus jitter).
pub fn run(hub: &Arc<Hub>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if !hub.relay.enabled() {
            backoff = Duration::from_secs(1);
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let result = hub
            .relay
            .identity()
            .and_then(|identity| host_connection(hub, &identity, &mut backoff));
        hub.relay.connected.store(false, Ordering::SeqCst);
        if let Err(error) = result {
            jucode_agent_core::log_warn!("daemon", "relay connection ended", error = error);
        }
        if !hub.relay.enabled() {
            continue;
        }
        let mut jitter = [0u8; 2];
        let _ = getrandom::getrandom(&mut jitter);
        let jitter = Duration::from_millis(u64::from(u16::from_le_bytes(jitter)) % 1000);
        thread::sleep(backoff + jitter);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

type HostSocket = WebSocket<MaybeTlsStream<TcpStream>>;

/// One host connection: authenticate, then route stream frames until the
/// socket drops or the relay is turned off. Dropping the stream table ends
/// every stream thread.
fn host_connection(
    hub: &Arc<Hub>,
    identity: &Arc<Identity>,
    backoff: &mut Duration,
) -> Result<(), String> {
    let base = hub.relay.url().unwrap_or_default();
    let mut socket = connect_host(&format!("{base}/host"))?;
    let challenge = read_json(&mut socket)?;
    if challenge["t"] != "challenge" {
        return Err(format!("expected a challenge, got {challenge}"));
    }
    let nonce = challenge["nonce"]
        .as_str()
        .and_then(|nonce| URL_SAFE_NO_PAD.decode(nonce).ok())
        .ok_or("challenge without a nonce")?;
    let signature = identity
        .signing
        .sign(&[AUTH_CONTEXT, nonce.as_slice()].concat());
    let auth = json!({
        "t": "auth",
        "pub": URL_SAFE_NO_PAD.encode(identity.signing.verifying_key().as_bytes()),
        "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        "v": hub.version,
    });
    socket
        .send(Message::text(auth.to_string()))
        .map_err(|error| error.to_string())?;
    let ready = read_json(&mut socket)?;
    if ready["t"] != "ready" {
        return Err(format!("relay refused the host: {ready}"));
    }
    set_read_timeout(&socket, POLL)?;
    hub.relay.connected.store(true, Ordering::SeqCst);
    // Back to the short backoff only once this connection proved stable: a
    // relay that accepts the host and drops it right away (another daemon
    // with this identity, say) would otherwise be retried every second.
    let _reset = ResetWhenStable {
        backoff,
        since: Instant::now(),
    };
    jucode_agent_core::log_info!("daemon", "relay connected", host = identity.host_id());

    // Stream threads send finished frames here; this thread writes them.
    let (out, outgoing) = mpsc::channel::<Vec<u8>>();
    let mut streams: HashMap<u32, Sender<Vec<u8>>> = HashMap::new();
    let mut heard = Instant::now();
    loop {
        if !hub.relay.enabled() {
            let _ = socket.close(None);
            let _ = socket.flush();
            return Ok(());
        }
        if heard.elapsed() > SILENCE {
            return Err(format!(
                "no frame from the relay for {} s",
                SILENCE.as_secs()
            ));
        }
        let read = socket.read();
        if read.is_ok() {
            heard = Instant::now();
        }
        match read {
            Ok(Message::Binary(bytes)) => match Frame::decode(&bytes) {
                Some(Frame::Open(id)) => {
                    let (incoming, receiver) = mpsc::channel();
                    streams.insert(id, incoming);
                    let hub = Arc::clone(hub);
                    let identity = Arc::clone(identity);
                    let out = out.clone();
                    thread::spawn(move || {
                        if let Err(error) = stream(&hub, &identity, id, receiver, out) {
                            jucode_agent_core::log_warn!(
                                "daemon",
                                "relay stream ended",
                                error = error
                            );
                        }
                    });
                }
                Some(Frame::Data(id, payload)) => {
                    if streams
                        .get(&id)
                        .is_some_and(|stream| stream.send(payload).is_err())
                    {
                        streams.remove(&id);
                    }
                }
                Some(Frame::Close(id)) => {
                    streams.remove(&id);
                }
                None => return Err("malformed relay frame".to_string()),
            },
            Ok(Message::Close(_)) => return Err("the relay closed the connection".to_string()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
        while let Ok(frame) = outgoing.try_recv() {
            if let [CLOSE, a, b, c, d] = frame[..] {
                streams.remove(&u32::from_be_bytes([a, b, c, d]));
            }
            socket
                .send(Message::binary(frame))
                .map_err(|error| error.to_string())?;
        }
    }
}

/// How long a host connection must last before the backoff starts over.
const STABLE: Duration = Duration::from_secs(30);

struct ResetWhenStable<'a> {
    backoff: &'a mut Duration,
    since: Instant,
}

impl Drop for ResetWhenStable<'_> {
    fn drop(&mut self) {
        if self.since.elapsed() >= STABLE {
            *self.backoff = Duration::from_secs(1);
        }
    }
}

/// Opens the host WebSocket with every step bounded: TCP connect, TLS and the
/// HTTP upgrade (a proxy that accepts the connection and then says nothing
/// would otherwise block this thread for good, `relay_set(false)` included).
fn connect_host(url: &str) -> Result<HostSocket, String> {
    use std::net::ToSocketAddrs;
    use tungstenite::client::IntoClientRequest;
    let request = url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    let uri = request.uri();
    let host = uri.host().ok_or("relay url without a host")?.to_string();
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let mut last = format!("could not resolve {host}");
    let mut tcp = None;
    for addr in (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
    {
        match TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(error) => last = error.to_string(),
        }
    }
    let tcp = tcp.ok_or(last)?;
    tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .and_then(|_| tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT)))
        .map_err(|error| error.to_string())?;
    let (socket, _) = tungstenite::client_tls(request, tcp).map_err(|error| error.to_string())?;
    Ok(socket)
}

fn set_read_timeout(socket: &HostSocket, timeout: Duration) -> Result<(), String> {
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => tls.get_ref(),
        _ => return Err("unsupported relay stream".to_string()),
    };
    tcp.set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())
}

/// The next text frame as JSON, during the host handshake.
fn read_json(socket: &mut HostSocket) -> Result<Value, String> {
    loop {
        match socket.read().map_err(|error| error.to_string())? {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str()).map_err(|error| error.to_string())
            }
            Message::Close(frame) => return Err(format!("relay closed: {frame:?}")),
            _ => {}
        }
    }
}

/// One client stream: the Noise handshake, then the client protocol. A
/// refused client gets its reason in msg 2 and then `close`.
fn stream(
    hub: &Arc<Hub>,
    identity: &Identity,
    id: u32,
    incoming: Receiver<Vec<u8>>,
    out: Sender<Vec<u8>>,
) -> Result<(), String> {
    let first = match incoming.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(message) => message,
        Err(RecvTimeoutError::Disconnected) => return Ok(()),
        Err(RecvTimeoutError::Timeout) => {
            let _ = out.send(Frame::Close(id).encode());
            return Err("no handshake from the client".to_string());
        }
    };
    let (transport, device) = match handshake(hub, identity, id, &first, &out) {
        Ok(accepted) => accepted,
        Err(error) => {
            let _ = out.send(Frame::Close(id).encode());
            return Err(error);
        }
    };
    let mut link = Stream {
        id,
        incoming,
        out,
        transport,
        open: true,
    };
    crate::attach(hub, &mut link, Some(device.id))
}

/// Reads msg 1, authorizes the client and sends msg 2.
fn handshake(
    hub: &Hub,
    identity: &Identity,
    id: u32,
    first: &[u8],
    out: &Sender<Vec<u8>>,
) -> Result<(noise::Transport, Device), String> {
    let mut noise = noise::responder(&identity.static_private)?;
    let mut buffer = vec![0u8; noise::MAX_MESSAGE];
    let read = noise
        .read_message(first, &mut buffer)
        .map_err(|error| format!("bad handshake: {error}"))?;
    let hello: Value = serde_json::from_slice(&buffer[..read]).unwrap_or(Value::Null);
    let key = noise
        .get_remote_static()
        .ok_or("handshake without a client key")?
        .to_vec();
    let verdict = authorize(hub, &key, &hello);
    let reply = match &verdict {
        Ok(device) => json!({ "ok": true, "device": device.id, "name": device.name }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    let written = noise
        .write_message(reply.to_string().as_bytes(), &mut buffer)
        .map_err(|error| error.to_string())?;
    out.send(Frame::Data(id, buffer[..written].to_vec()).encode())
        .map_err(|_| "relay connection closed".to_string())?;
    let device = verdict?;
    Ok((noise::Transport::new(noise)?, device))
}

/// The device a client static key belongs to: an already paired one, or a
/// new one when msg 1 carries a valid pairing code (§4 step 2).
fn authorize(hub: &Hub, key: &[u8], hello: &Value) -> Result<Device, String> {
    let hash = bytes_hash(key);
    if let Some(device) = hub.store.device_for_hash(&hash) {
        return Ok(device);
    }
    match hello["pair"].as_str() {
        Some(code) => hub.pair_hash(code, hello["name"].as_str().unwrap_or_default(), hash),
        None => Err("this device is not paired".to_string()),
    }
}

/// A relay stream after its handshake, as a client link. Frames travel as
/// chunked Noise transport messages.
struct Stream {
    id: u32,
    incoming: Receiver<Vec<u8>>,
    out: Sender<Vec<u8>>,
    transport: noise::Transport,
    /// False once either side closed, so `close` goes out at most once.
    open: bool,
}

impl Link for Stream {
    fn send(&mut self, frame: &str) -> Result<(), String> {
        for message in self.transport.seal(frame.as_bytes())? {
            self.out
                .send(Frame::Data(self.id, message).encode())
                .map_err(|_| "relay connection closed".to_string())?;
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<String>, Received> {
        match self.incoming.recv_timeout(POLL) {
            Ok(message) => match self.transport.open(&message) {
                Ok(Some(frame)) => String::from_utf8(frame)
                    .map(Some)
                    .map_err(|_| Received::Failed("frame is not UTF-8".to_string())),
                Ok(None) => Ok(None),
                Err(error) => Err(Received::Failed(error)),
            },
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.open = false;
                Err(Received::Closed)
            }
        }
    }

    fn close(&mut self) {
        if std::mem::replace(&mut self.open, false) {
            let _ = self.out.send(Frame::Close(self.id).encode());
        }
    }
}

/// A stream that ends on the daemon's side (an error, a revoked device)
/// tells the relay.
impl Drop for Stream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{store::now, Agents};

    fn hub(label: &str) -> Arc<Hub> {
        let dir = std::env::temp_dir().join(format!(
            "jucode-relay-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            Some("wss://relay.example/relay/v1/".to_string()),
        )
    }

    #[test]
    fn frames_encode_and_decode() {
        for frame in [
            Frame::Open(1),
            Frame::Data(0xdead_beef, b"payload".to_vec()),
            Frame::Data(7, Vec::new()),
            Frame::Close(u32::MAX),
        ] {
            assert_eq!(Frame::decode(&frame.encode()), Some(frame));
        }
        assert_eq!(Frame::Data(258, vec![9]).encode(), [2, 0, 0, 1, 2, 9]);
        assert_eq!(Frame::decode(&[1, 0, 0, 0, 0]), None, "stream 0");
        assert_eq!(Frame::decode(&[4, 0, 0, 0, 1]), None, "unknown kind");
        assert_eq!(Frame::decode(&[2, 0, 0]), None, "short");
    }

    #[test]
    fn authorization_pairs_with_a_code_and_honours_revocation() {
        let hub = hub("authorize");
        let (_, phone) = noise::generate_keypair().unwrap();
        let (_, stranger) = noise::generate_keypair().unwrap();

        // Unknown key without a code, or with a wrong one.
        assert!(authorize(&hub, &phone, &json!({ "name": "phone" })).is_err());
        assert!(authorize(&hub, &phone, &json!({ "pair": "WRONG123" })).is_err());

        // A valid code pairs the key; the code is single use.
        let (code, _) = hub.start_pairing().unwrap();
        let device = authorize(&hub, &phone, &json!({ "name": "phone", "pair": code })).unwrap();
        assert_eq!(device.name, "phone");
        assert_eq!(device.token_hash, bytes_hash(&phone));
        assert!(authorize(&hub, &stranger, &json!({ "pair": code })).is_err());

        // The paired key comes back without a code, as the same device.
        assert_eq!(authorize(&hub, &phone, &json!({})).unwrap().id, device.id);
        assert_eq!(hub.devices_json()["devices"][0]["id"], device.id.as_str());

        hub.revoke_device(&device.id).unwrap();
        assert!(authorize(&hub, &phone, &json!({})).is_err());
    }

    #[test]
    fn the_identity_is_kept_and_private() {
        let hub = hub("identity");
        let first = hub.relay.identity().unwrap();
        let path = hub.store.dir().join(IDENTITY_FILE);
        let again = Identity::load_or_create(&path).unwrap();
        assert_eq!(again.host_id(), first.host_id());
        assert_eq!(again.static_public, first.static_public);
        assert_eq!(first.host_id().len(), 22);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_pair_link_needs_the_relay_on_and_names_the_app() {
        let hub = hub("link");
        assert!(hub.relay.pair_link(&hub).is_err());
        hub.relay.set_enabled(&hub.store, true).unwrap();
        assert_eq!(hub.store.setting(SETTING), json!(true));
        let reply = hub.relay.pair_link(&hub).unwrap();
        let identity = hub.relay.identity().unwrap();
        assert_eq!(
            reply["link"],
            format!(
                "https://relay.example/remote#pair={}.{}.{}",
                identity.host_id(),
                URL_SAFE_NO_PAD.encode(&identity.static_public),
                reply["code"].as_str().unwrap(),
            )
        );
        assert_eq!(
            app_origin("ws://127.0.0.1:9/relay/v1"),
            "http://127.0.0.1:9"
        );
    }
}
