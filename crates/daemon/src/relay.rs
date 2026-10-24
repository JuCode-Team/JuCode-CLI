//! The host side of the JuCode relay (`docs/relay-protocol.md`): one
//! outbound WebSocket to the relay, authenticated with the daemon's JuCode
//! login and its Ed25519 identity, carrying client streams. Each stream runs
//! a Noise session (`noise.rs`) and then serves a paired device exactly like
//! a local WebSocket client. The relay only sees ciphertext.

use crate::{
    hub::{lock, Hub},
    noise,
    store::{bytes_hash, now, write_private, Device, Store},
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
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

const IDENTITY_FILE: &str = "relay-identity.json";
/// Signed together with the relay's nonce.
const AUTH_CONTEXT: &[u8] = b"jucode-relay-v1:";
/// The daemon setting that turns the relay off (missing: on).
const SETTING: &str = "relay";
/// The daemon setting naming the account the computer is registered under.
const USER_SETTING: &str = "relay_user";
/// Where pairing links point: the web app, whatever the relay URL.
const APP_ORIGIN: &str = "https://app.jucode.net";
/// How long the relay's challenge and a client's first Noise message may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a connect request waits for the desktop's answer (unit tests:
/// a second).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 120 });
/// Connect requests open at once, at most.
const MAX_REQUESTS: usize = 3;
/// Short reads let one thread both receive and flush, as for local clients.
const POLL: Duration = Duration::from_millis(20);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The relay pings the host every 30 s. A connection this long without a
/// single frame is dead even if the socket never said so: a proxy or a
/// network change can drop it without the close ever reaching us.
const SILENCE: Duration = Duration::from_secs(75);
/// How often a connected host checks that the computer is still signed in.
const LOGIN_CHECK: Duration = Duration::from_millis(500);
/// The relay's close code for a server restarting: reconnect soon.
const RESTARTING: u16 = 4503;

const OPEN: u8 = 1;
const DATA: u8 = 2;
const CLOSE: u8 = 3;

/// A binary frame on the host connection (§3.1). `Open` carries the caller
/// the relay vouches for (§3.2).
#[derive(Debug, PartialEq)]
enum Frame {
    Open(u32, Vec<u8>),
    Data(u32, Vec<u8>),
    Close(u32),
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let (kind, stream, payload) = match self {
            Frame::Open(stream, payload) => (OPEN, stream, payload.as_slice()),
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
            OPEN => Some(Frame::Open(stream, rest[4..].to_vec())),
            DATA => Some(Frame::Data(stream, rest[4..].to_vec())),
            CLOSE => Some(Frame::Close(stream)),
            _ => None,
        }
    }
}

/// Who opened a stream: the client's login, as the relay read it from the
/// client's token (the `open` payload, §3.2).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Caller {
    pub user: String,
    pub authz: String,
    pub client: String,
    pub name: String,
}

impl Caller {
    fn parse(payload: &[u8]) -> Self {
        let value: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
        Self {
            user: text("user"),
            authz: text("authz"),
            client: text("client"),
            name: text("name"),
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

/// Drops the relay identity in `dir`; the next connection makes a new one
/// (and phones must pair again).
pub fn forget_identity(dir: &std::path::Path) {
    let _ = fs::remove_file(dir.join(IDENTITY_FILE));
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
    /// The account the computer is registered under (`relay_user`).
    user: Mutex<Option<String>>,
    /// An access token the relay refused even right after a refresh: the
    /// computer counts as signed out until auth.json holds another one.
    refused: Mutex<Option<String>>,
    /// Open connect requests by id.
    requests: Mutex<HashMap<String, Request>>,
}

/// A client asking to connect, waiting for the desktop (§3.2 step 4).
struct Request {
    name: String,
    client: String,
    at: u64,
    decide: Sender<bool>,
}

impl Request {
    fn to_json(&self, id: &str) -> Value {
        json!({ "id": id, "name": self.name, "client": self.client, "at": self.at })
    }
}

/// How a connect request ended.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Allowed,
    Denied,
    Expired,
    Cancelled,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Allowed => "allowed",
            Outcome::Denied => "denied",
            Outcome::Expired => "expired",
            Outcome::Cancelled => "cancelled",
        }
    }
}

impl Relay {
    pub fn new(url: Option<String>, store: &Store) -> Self {
        Self {
            url: url.map(|url| url.trim_end_matches('/').to_string()),
            enabled: AtomicBool::new(store.setting(SETTING).as_bool().unwrap_or(true)),
            connected: AtomicBool::new(false),
            dir: store.dir().to_path_buf(),
            identity: Mutex::new(None),
            user: Mutex::new(store.setting(USER_SETTING).as_str().map(str::to_string)),
            refused: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
        }
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    pub fn enabled(&self) -> bool {
        self.url.is_some() && self.enabled.load(Ordering::SeqCst)
    }

    /// Signed in to JuCode, with a token the relay has not refused.
    fn signed_in(&self) -> bool {
        jucode_agent_core::jucode_saved_access_token()
            .is_some_and(|token| lock(&self.refused).as_deref() != Some(token.as_str()))
    }

    fn user(&self) -> Option<String> {
        lock(&self.user).clone()
    }

    /// The saved login now belongs to another account than the one this
    /// computer registered under (a `/login` over the old one).
    fn login_changed(&self) -> bool {
        let saved =
            jucode_agent_core::jucode_saved_access_token().and_then(|token| token_user(&token));
        saved.is_some() && saved != self.user()
    }

    /// The identity, created on first use.
    fn identity(&self) -> Result<Arc<Identity>, String> {
        let mut slot = lock(&self.identity);
        if let Some(identity) = slot.as_ref() {
            return Ok(Arc::clone(identity));
        }
        let identity = Arc::new(Identity::load_or_create(&self.dir.join(IDENTITY_FILE))?);
        *slot = Some(Arc::clone(&identity));
        Ok(identity)
    }

    pub fn host_id(&self) -> Option<String> {
        self.identity().ok().map(|identity| identity.host_id())
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
            "signed_in": self.signed_in(),
            "host": self.host_id(),
            "url": self.url,
            "user": self.user(),
        })
    }

    /// A pairing code plus the link a phone opens to pair over the relay
    /// (§3.4).
    pub fn pair_link(&self, hub: &Hub) -> Result<Value, String> {
        if !self.enabled() {
            return Err("the relay is off".to_string());
        }
        let identity = self.identity()?;
        let (code, expires_at) = hub.start_pairing()?;
        let link = format!(
            "{APP_ORIGIN}/remote#pair={}.{}.{code}",
            identity.host_id(),
            URL_SAFE_NO_PAD.encode(&identity.static_public),
        );
        Ok(json!({ "type": "pair_link", "link": link, "code": code, "expires_at": expires_at }))
    }

    /// The relay registered the computer under `user`. Another account than
    /// before (or the first one) unpairs every device first.
    fn register(&self, hub: &Hub, user: &str) -> Result<(), String> {
        let mut current = lock(&self.user);
        if current.as_deref() == Some(user) {
            return Ok(());
        }
        let revoked = hub.revoke_all_devices()?;
        hub.store
            .set_setting(USER_SETTING, json!(user))
            .map_err(|error| error.to_string())?;
        *current = Some(user.to_string());
        jucode_agent_core::log_info!(
            "daemon",
            "relay account changed; devices unpaired",
            revoked = revoked
        );
        Ok(())
    }

    /// `connect_requests`: the open requests.
    pub fn requests_json(&self) -> Value {
        let requests: Vec<Value> = lock(&self.requests)
            .iter()
            .map(|(id, request)| request.to_json(id))
            .collect();
        json!({ "type": "connect_requests", "requests": requests })
    }

    /// `connect_decide`: answers an open request.
    pub fn decide(&self, id: &str, allow: bool) -> Result<(), String> {
        // Sent while the request is taken out under the lock, so a waiting
        // stream that finds it gone knows the answer is in its channel.
        let mut requests = lock(&self.requests);
        let request = requests
            .remove(id)
            .ok_or_else(|| format!("no open connect request {id}"))?;
        let _ = request.decide.send(allow);
        Ok(())
    }

    /// Asks the desktop whether a client may connect and waits for the
    /// answer (`REQUEST_TIMEOUT`). `cancelled` tells whether the client has
    /// gone.
    fn request_connect(
        &self,
        hub: &Hub,
        name: &str,
        client: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Outcome, String> {
        if !hub.has_local_client() {
            return Err("no_desktop".to_string());
        }
        let id = hub.new_id("cr");
        let (decide, decision) = mpsc::channel();
        let request = Request {
            name: name.to_string(),
            client: client.to_string(),
            at: now(),
            decide,
        };
        let mut announce = request.to_json(&id);
        {
            let mut requests = lock(&self.requests);
            if requests.len() >= MAX_REQUESTS {
                return Err("busy".to_string());
            }
            requests.insert(id.clone(), request);
        }
        announce["type"] = json!("connect_request");
        hub.broadcast_local(&announce);
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let waited = loop {
            match decision.recv_timeout(POLL) {
                Ok(allow) => break Some(allow),
                Err(RecvTimeoutError::Timeout) if cancelled() => break None,
                Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => break None,
                Err(_) => {}
            }
        };
        let decided = |allow| {
            if allow {
                Outcome::Allowed
            } else {
                Outcome::Denied
            }
        };
        let outcome = match waited {
            Some(allow) => decided(allow),
            // Already taken out: decided just after the last look.
            None if lock(&self.requests).remove(&id).is_none() => {
                decided(decision.try_recv().unwrap_or(false))
            }
            None if cancelled() => Outcome::Cancelled,
            None => Outcome::Expired,
        };
        hub.broadcast_local(
            &json!({ "type": "connect_request_closed", "id": id, "outcome": outcome.as_str() }),
        );
        Ok(outcome)
    }
}

/// How a host connection ended, besides a clean stop (relay turned off).
#[derive(Debug)]
enum Ended {
    /// HTTP 401 on the upgrade: the relay refused the access token.
    Refused,
    /// Close 4503: the server is restarting; reconnect soon.
    Restarting,
    Failed(String),
}

impl From<String> for Ended {
    fn from(error: String) -> Self {
        Ended::Failed(error)
    }
}

/// The relay thread: keeps a host connection while the relay is enabled and
/// the computer is signed in to JuCode, reconnecting with backoff (1 s
/// doubling to 60 s, plus jitter; 1 s flat after a server restart).
pub fn run(hub: &Arc<Hub>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if !hub.relay.enabled() || !hub.relay.signed_in() {
            backoff = Duration::from_secs(1);
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let result = hub
            .relay
            .identity()
            .map_err(Ended::Failed)
            .and_then(|identity| signed_in_connection(hub, &identity, &mut backoff));
        hub.relay.connected.store(false, Ordering::SeqCst);
        let wait = match result {
            Ok(()) => continue,
            Err(Ended::Refused) => {
                jucode_agent_core::log_warn!("daemon", "the relay refused the JuCode login");
                continue;
            }
            Err(Ended::Restarting) => {
                jucode_agent_core::log_info!("daemon", "relay server restarting");
                Duration::from_secs(1)
            }
            Err(Ended::Failed(error)) => {
                jucode_agent_core::log_warn!("daemon", "relay connection ended", error = error);
                let mut jitter = [0u8; 2];
                let _ = getrandom::getrandom(&mut jitter);
                let jitter = Duration::from_millis(u64::from(u16::from_le_bytes(jitter)) % 1000);
                let wait = backoff + jitter;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                wait
            }
        };
        if hub.relay.enabled() {
            thread::sleep(wait);
        }
    }
}

/// The account an OAuth access token (a JWT) was issued to: its `uid` claim.
/// Read only to notice a change of login; the relay verifies the token.
fn token_user(token: &str) -> Option<String> {
    let payload = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    claims["uid"].as_str().map(str::to_string)
}

/// A host connection on the login's access token. A refused token is
/// refreshed once; refused again, it is remembered (`Relay::refused`).
fn signed_in_connection(
    hub: &Arc<Hub>,
    identity: &Arc<Identity>,
    backoff: &mut Duration,
) -> Result<(), Ended> {
    let (_, token) = jucode_agent_core::jucode_gateway_token()?;
    match host_connection(hub, identity, &token, backoff) {
        Err(Ended::Refused) => {}
        other => return other,
    }
    let (_, fresh) = jucode_agent_core::jucode_gateway_token_refreshed(&token)?;
    let result = host_connection(hub, identity, &fresh, backoff);
    if matches!(result, Err(Ended::Refused)) {
        *lock(&hub.relay.refused) = Some(fresh);
    }
    result
}

type HostSocket = WebSocket<MaybeTlsStream<TcpStream>>;

/// One host connection: authenticate, then route stream frames until the
/// socket drops or the relay is turned off. Dropping the stream table ends
/// every stream thread.
fn host_connection(
    hub: &Arc<Hub>,
    identity: &Arc<Identity>,
    token: &str,
    backoff: &mut Duration,
) -> Result<(), Ended> {
    let base = hub.relay.url().unwrap_or_default();
    let host_id = identity.host_id();
    let mut socket = connect_host(&format!("{base}/host?host={host_id}"), token)?;
    let challenge = read_json(&mut socket)?;
    if challenge["t"] != "challenge" {
        return Err(Ended::Failed(format!(
            "expected a challenge, got {challenge}"
        )));
    }
    let nonce = challenge["nonce"]
        .as_str()
        .and_then(|nonce| URL_SAFE_NO_PAD.decode(nonce).ok())
        .ok_or("challenge without a nonce".to_string())?;
    let signature = identity
        .signing
        .sign(&[AUTH_CONTEXT, nonce.as_slice()].concat());
    let auth = json!({
        "t": "auth",
        "pub": URL_SAFE_NO_PAD.encode(identity.signing.verifying_key().as_bytes()),
        "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        "v": hub.version,
        "static": URL_SAFE_NO_PAD.encode(&identity.static_public),
        "name": jucode_agent_core::hostname().unwrap_or_default(),
        "platform": std::env::consts::OS,
    });
    socket
        .send(Message::text(auth.to_string()))
        .map_err(|error| error.to_string())?;
    let ready = read_json(&mut socket)?;
    if ready["t"] != "ready" {
        return Err(Ended::Failed(format!("relay refused the host: {ready}")));
    }
    let user = ready["user"]
        .as_str()
        .filter(|user| !user.is_empty())
        .ok_or_else(|| format!("relay ready without a user: {ready}"))?;
    hub.relay.register(hub, user)?;
    set_read_timeout(&socket, POLL)?;
    hub.relay.connected.store(true, Ordering::SeqCst);
    // Back to the short backoff only once this connection proved stable: a
    // relay that accepts the host and drops it right away (another daemon
    // with this identity, say) would otherwise be retried every second.
    let _reset = ResetWhenStable {
        backoff,
        since: Instant::now(),
    };
    jucode_agent_core::log_info!("daemon", "relay connected", host = host_id);

    // Stream threads send finished frames here; this thread writes them.
    let (out, outgoing) = mpsc::channel::<Vec<u8>>();
    let mut streams: HashMap<u32, Sender<Vec<u8>>> = HashMap::new();
    let mut heard = Instant::now();
    let mut login_checked = Instant::now();
    loop {
        // Signing out, or in to another account, takes the computer off the
        // relay; the next connection registers it under the new account.
        let signed_out = login_checked.elapsed() >= LOGIN_CHECK && {
            login_checked = Instant::now();
            !hub.relay.signed_in() || hub.relay.login_changed()
        };
        if !hub.relay.enabled() || signed_out {
            let _ = socket.close(None);
            let _ = socket.flush();
            return Ok(());
        }
        if heard.elapsed() > SILENCE {
            return Err(Ended::Failed(format!(
                "no frame from the relay for {} s",
                SILENCE.as_secs()
            )));
        }
        let read = socket.read();
        if read.is_ok() {
            heard = Instant::now();
        }
        match read {
            Ok(Message::Binary(bytes)) => match Frame::decode(&bytes) {
                Some(Frame::Open(id, payload)) => {
                    let (incoming, receiver) = mpsc::channel();
                    streams.insert(id, incoming);
                    let hub = Arc::clone(hub);
                    let identity = Arc::clone(identity);
                    let out = out.clone();
                    let caller = Caller::parse(&payload);
                    thread::spawn(move || {
                        if let Err(error) = stream(&hub, &identity, id, &caller, receiver, out) {
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
                None => return Err(Ended::Failed("malformed relay frame".to_string())),
            },
            Ok(Message::Close(frame)) => return Err(closed(frame)),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(Ended::Failed(error.to_string())),
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

/// What the relay's close frame means for reconnecting.
fn closed(frame: Option<tungstenite::protocol::CloseFrame>) -> Ended {
    match frame {
        Some(frame) if u16::from(frame.code) == RESTARTING => Ended::Restarting,
        frame => Ended::Failed(format!("the relay closed the connection: {frame:?}")),
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
fn connect_host(url: &str, token: &str) -> Result<HostSocket, Ended> {
    use std::net::ToSocketAddrs;
    use tungstenite::client::IntoClientRequest;
    let mut request = url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "the access token is not a header value".to_string())?,
    );
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or("relay url without a host".to_string())?
        .to_string();
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
    match tungstenite::client_tls(request, tcp) {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)))
            if response.status() == tungstenite::http::StatusCode::UNAUTHORIZED =>
        {
            Err(Ended::Refused)
        }
        Err(error) => Err(Ended::Failed(error.to_string())),
    }
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
fn read_json(socket: &mut HostSocket) -> Result<Value, Ended> {
    loop {
        match socket.read().map_err(|error| error.to_string())? {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str())
                    .map_err(|error| Ended::Failed(error.to_string()))
            }
            Message::Close(frame) => return Err(closed(frame)),
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
    caller: &Caller,
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
    // Nothing more comes before msg 2; the channel closing means the client
    // (or the whole host connection) went away.
    let gone = || matches!(incoming.try_recv(), Err(TryRecvError::Disconnected));
    let (transport, device) = match handshake(hub, identity, id, caller, &first, &out, &gone) {
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
    caller: &Caller,
    first: &[u8],
    out: &Sender<Vec<u8>>,
    gone: &dyn Fn() -> bool,
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
    let verdict = authorize(hub, &key, &hello, caller, gone);
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

/// The device a client static key belongs to (§3.2): only for a client
/// signed in to the computer's account; an already paired key, or a new
/// device paired with a code or allowed on the desktop.
fn authorize(
    hub: &Hub,
    key: &[u8],
    hello: &Value,
    caller: &Caller,
    gone: &dyn Fn() -> bool,
) -> Result<Device, String> {
    if caller.user.is_empty() || hub.relay.user().as_deref() != Some(caller.user.as_str()) {
        return Err("wrong account".to_string());
    }
    let key_hash = bytes_hash(key);
    if let Some(device) = hub.store.device_for_key(&key_hash) {
        return Ok(device);
    }
    let name = hello["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&caller.name);
    let device = Device {
        key_hash,
        authz: caller.authz.clone(),
        client: caller.client.clone(),
        user: caller.user.clone(),
        ..Device::default()
    };
    if let Some(code) = hello["pair"].as_str() {
        hub.redeem_pairing(code)?;
        return hub.add_device(name, device);
    }
    if hello["request"] == true {
        return match hub.relay.request_connect(hub, name, &caller.client, gone)? {
            Outcome::Allowed => hub.add_device(name, device),
            outcome => Err(outcome.as_str().to_string()),
        };
    }
    Err("not paired".to_string())
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

    fn hub(label: &str) -> Arc<Hub> {
        let dir = std::env::temp_dir().join(format!(
            "jucode-relay-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            "test",
            Some("wss://relay.example/v1/relay/".to_string()),
        )
    }

    fn caller(user: &str) -> Caller {
        Caller {
            user: user.to_string(),
            authz: "authz-1".to_string(),
            client: "jucode-mobile".to_string(),
            name: "Pixel".to_string(),
        }
    }

    fn never() -> bool {
        false
    }

    /// A hub registered under `u1`, with a desktop connected whose frames
    /// come out of the receiver.
    fn hub_with_desktop(label: &str) -> (Arc<Hub>, Receiver<String>) {
        let hub = hub(label);
        hub.relay.register(&hub, "u1").unwrap();
        let (outbox, frames) = Hub::outbox();
        hub.add_client(outbox, None);
        (hub, frames)
    }

    /// The next frame of `kind` the desktop got.
    fn next(frames: &Receiver<String>, kind: &str) -> Value {
        loop {
            let frame: Value =
                serde_json::from_str(&frames.recv_timeout(Duration::from_secs(5)).unwrap())
                    .unwrap();
            if frame["type"] == kind {
                return frame;
            }
        }
    }

    /// Asks to connect with a new key on another thread.
    fn ask(hub: &Arc<Hub>) -> (Vec<u8>, thread::JoinHandle<Result<Device, String>>) {
        let (_, key) = noise::generate_keypair().unwrap();
        let hub = Arc::clone(hub);
        let asking = key.clone();
        let handle = thread::spawn(move || {
            authorize(
                &hub,
                &asking,
                &json!({ "request": true }),
                &caller("u1"),
                &never,
            )
        });
        (key, handle)
    }

    #[test]
    fn frames_encode_and_decode() {
        for frame in [
            Frame::Open(1, br#"{"user":"u"}"#.to_vec()),
            Frame::Open(2, Vec::new()),
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
    fn the_open_payload_names_the_caller() {
        let parsed =
            Caller::parse(br#"{"user":"u1","authz":"a1","client":"jucode-web","name":"Chrome"}"#);
        assert_eq!(
            parsed,
            Caller {
                user: "u1".to_string(),
                authz: "a1".to_string(),
                client: "jucode-web".to_string(),
                name: "Chrome".to_string(),
            }
        );
        assert_eq!(Caller::parse(b""), Caller::default());
    }

    #[test]
    fn only_the_computers_account_gets_through() {
        let hub = hub("account");
        let (_, phone) = noise::generate_keypair().unwrap();
        let (code, _) = hub.start_pairing().unwrap();
        let pair = json!({ "name": "phone", "pair": code });
        // Not registered yet: no account matches.
        assert_eq!(
            authorize(&hub, &phone, &pair, &caller("u1"), &never).unwrap_err(),
            "wrong account"
        );
        hub.relay.register(&hub, "u1").unwrap();
        for stranger in [caller("u2"), Caller::default()] {
            assert_eq!(
                authorize(&hub, &phone, &pair, &stranger, &never).unwrap_err(),
                "wrong account"
            );
        }
        let device = authorize(&hub, &phone, &pair, &caller("u1"), &never).unwrap();
        // Even a paired key is refused from another account.
        assert_eq!(
            authorize(&hub, &phone, &json!({}), &caller("u2"), &never).unwrap_err(),
            "wrong account"
        );
        assert_eq!(
            authorize(&hub, &phone, &json!({}), &caller("u1"), &never)
                .unwrap()
                .id,
            device.id
        );
    }

    #[test]
    fn authorization_pairs_with_a_code_and_honours_revocation() {
        let hub = hub("authorize");
        hub.relay.register(&hub, "u1").unwrap();
        let (_, phone) = noise::generate_keypair().unwrap();
        let (_, stranger) = noise::generate_keypair().unwrap();
        let me = caller("u1");

        // Unknown key without a code, or with a wrong one.
        assert_eq!(
            authorize(&hub, &phone, &json!({ "name": "phone" }), &me, &never).unwrap_err(),
            "not paired"
        );
        assert!(authorize(&hub, &phone, &json!({ "pair": "WRONG123" }), &me, &never).is_err());

        // A valid code pairs the key; the code is single use.
        let (code, _) = hub.start_pairing().unwrap();
        let device = authorize(
            &hub,
            &phone,
            &json!({ "name": "phone", "pair": code }),
            &me,
            &never,
        )
        .unwrap();
        assert_eq!(device.name, "phone");
        assert_eq!(device.key_hash, bytes_hash(&phone));
        assert_eq!(
            (
                device.authz.as_str(),
                device.client.as_str(),
                device.user.as_str()
            ),
            ("authz-1", "jucode-mobile", "u1")
        );
        assert!(authorize(&hub, &stranger, &json!({ "pair": code }), &me, &never).is_err());

        // Without a name in msg 1 the device takes the authorization's name.
        let (code, _) = hub.start_pairing().unwrap();
        let named = authorize(&hub, &stranger, &json!({ "pair": code }), &me, &never).unwrap();
        assert_eq!(named.name, "Pixel");

        // The paired key comes back without a code, as the same device.
        assert_eq!(
            authorize(&hub, &phone, &json!({}), &me, &never).unwrap().id,
            device.id
        );
        let listed = hub.devices_json();
        assert_eq!(listed["devices"][0]["id"], device.id.as_str());
        assert_eq!(listed["devices"][0]["client"], "jucode-mobile");
        assert_eq!(listed["devices"][0]["authz"], "authz-1");
        assert_eq!(listed["devices"][0]["user"], "u1");

        hub.revoke_device(&device.id).unwrap();
        assert!(authorize(&hub, &phone, &json!({}), &me, &never).is_err());
    }

    #[test]
    fn a_connect_request_needs_a_desktop() {
        let hub = hub("no-desktop");
        hub.relay.register(&hub, "u1").unwrap();
        let (_, phone) = noise::generate_keypair().unwrap();
        let request = json!({ "request": true });
        assert_eq!(
            authorize(&hub, &phone, &request, &caller("u1"), &never).unwrap_err(),
            "no_desktop"
        );
        // A paired device's connection is no desktop.
        let (outbox, _frames) = Hub::outbox();
        hub.add_client(outbox, Some("dev-1".to_string()));
        assert_eq!(
            authorize(&hub, &phone, &request, &caller("u1"), &never).unwrap_err(),
            "no_desktop"
        );
    }

    #[test]
    fn an_allowed_request_pairs_the_device() {
        let (hub, frames) = hub_with_desktop("allowed");
        let (key, asking) = ask(&hub);
        let shown = next(&frames, "connect_request");
        assert_eq!(shown["name"], "Pixel");
        assert_eq!(shown["client"], "jucode-mobile");
        assert!(shown["at"].as_u64().unwrap() > 0);
        let id = shown["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("cr-"));
        assert_eq!(hub.relay.requests_json()["requests"][0]["id"], id.as_str());

        hub.relay.decide(&id, true).unwrap();
        let device = asking.join().unwrap().unwrap();
        assert_eq!(device.key_hash, bytes_hash(&key));
        assert_eq!(device.user, "u1");
        let closed = next(&frames, "connect_request_closed");
        assert_eq!(
            closed,
            json!({ "type": "connect_request_closed", "id": id, "outcome": "allowed" })
        );
        assert_eq!(hub.relay.requests_json()["requests"], json!([]));
        // Decided once; later connections need no request.
        assert!(hub.relay.decide(&id, true).is_err());
        assert_eq!(
            authorize(&hub, &key, &json!({}), &caller("u1"), &never)
                .unwrap()
                .id,
            device.id
        );
    }

    #[test]
    fn token_user_reads_the_uid_claim() {
        let claims = URL_SAFE_NO_PAD.encode(br#"{"uid":"user-b","knd":"oauth_access"}"#);
        assert_eq!(
            token_user(&format!("h.{claims}.sig")).as_deref(),
            Some("user-b")
        );
        assert_eq!(token_user("not-a-jwt"), None);
    }

    #[test]
    fn a_denied_request_is_refused() {
        let (hub, frames) = hub_with_desktop("denied");
        let (key, asking) = ask(&hub);
        let id = next(&frames, "connect_request")["id"]
            .as_str()
            .unwrap()
            .to_string();
        hub.relay.decide(&id, false).unwrap();
        assert_eq!(asking.join().unwrap().unwrap_err(), "denied");
        assert_eq!(next(&frames, "connect_request_closed")["outcome"], "denied");
        assert!(hub.store.device_for_key(&bytes_hash(&key)).is_none());
    }

    #[test]
    fn an_unanswered_request_expires() {
        let (hub, frames) = hub_with_desktop("expired");
        let (_, asking) = ask(&hub);
        let id = next(&frames, "connect_request")["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(asking.join().unwrap().unwrap_err(), "expired");
        assert_eq!(
            next(&frames, "connect_request_closed")["outcome"],
            "expired"
        );
        assert!(hub.relay.decide(&id, true).is_err());
    }

    #[test]
    fn a_client_that_leaves_cancels_its_request() {
        let (hub, frames) = hub_with_desktop("cancelled");
        let (_, phone) = noise::generate_keypair().unwrap();
        let left = Arc::new(AtomicBool::new(false));
        let asking = {
            let hub = Arc::clone(&hub);
            let left = Arc::clone(&left);
            thread::spawn(move || {
                authorize(
                    &hub,
                    &phone,
                    &json!({ "request": true }),
                    &caller("u1"),
                    &|| left.load(Ordering::SeqCst),
                )
            })
        };
        next(&frames, "connect_request");
        left.store(true, Ordering::SeqCst);
        assert_eq!(asking.join().unwrap().unwrap_err(), "cancelled");
        assert_eq!(
            next(&frames, "connect_request_closed")["outcome"],
            "cancelled"
        );
        assert_eq!(hub.relay.requests_json()["requests"], json!([]));
    }

    #[test]
    fn at_most_three_requests_wait_at_once() {
        let (hub, frames) = hub_with_desktop("busy");
        let waiting: Vec<_> = (0..MAX_REQUESTS).map(|_| ask(&hub).1).collect();
        let ids: Vec<String> = (0..MAX_REQUESTS)
            .map(|_| {
                next(&frames, "connect_request")["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let (_, extra) = ask(&hub);
        assert_eq!(extra.join().unwrap().unwrap_err(), "busy");
        for id in &ids {
            hub.relay.decide(id, false).unwrap();
        }
        for asking in waiting {
            assert_eq!(asking.join().unwrap().unwrap_err(), "denied");
        }
    }

    #[test]
    fn another_account_unpairs_every_device() {
        let hub = hub("relay-user");
        hub.relay.register(&hub, "u1").unwrap();
        let (_, phone) = noise::generate_keypair().unwrap();
        let (code, _) = hub.start_pairing().unwrap();
        authorize(
            &hub,
            &phone,
            &json!({ "pair": code }),
            &caller("u1"),
            &never,
        )
        .unwrap();
        let (outbox, _frames) = Hub::outbox();
        let device = hub.devices_json()["devices"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let client = hub.add_client(outbox, Some(device));

        // The same account again keeps them.
        hub.relay.register(&hub, "u1").unwrap();
        assert_eq!(hub.devices_json()["devices"].as_array().unwrap().len(), 1);

        hub.relay.register(&hub, "u2").unwrap();
        assert_eq!(hub.devices_json()["devices"], json!([]));
        assert!(hub.device_of(client).is_none(), "its connection is dropped");
        assert_eq!(hub.store.setting(USER_SETTING), json!("u2"));
        assert_eq!(hub.relay.user().as_deref(), Some("u2"));
        // Kept across restarts.
        assert_eq!(
            Relay::new(hub.relay.url.clone(), &hub.store)
                .user()
                .as_deref(),
            Some("u2")
        );
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
    fn the_relay_is_on_by_default_and_a_pair_link_names_the_web_app() {
        let hub = hub("link");
        assert!(hub.relay.enabled());
        let reply = hub.relay.pair_link(&hub).unwrap();
        let identity = hub.relay.identity().unwrap();
        assert_eq!(
            reply["link"],
            format!(
                "https://app.jucode.net/remote#pair={}.{}.{}",
                identity.host_id(),
                URL_SAFE_NO_PAD.encode(&identity.static_public),
                reply["code"].as_str().unwrap(),
            )
        );
        hub.relay.set_enabled(&hub.store, false).unwrap();
        assert_eq!(hub.store.setting(SETTING), json!(false));
        assert!(hub.relay.pair_link(&hub).is_err());
        assert!(!Relay::new(hub.relay.url.clone(), &hub.store).enabled());
    }

    #[test]
    fn a_restarting_server_is_told_apart() {
        use tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
        let frame = |code: u16| {
            Some(CloseFrame {
                code: CloseCode::from(code),
                reason: "".into(),
            })
        };
        assert!(matches!(closed(frame(4503)), Ended::Restarting));
        assert!(matches!(closed(frame(4409)), Ended::Failed(_)));
        assert!(matches!(closed(None), Ended::Failed(_)));
    }
}
