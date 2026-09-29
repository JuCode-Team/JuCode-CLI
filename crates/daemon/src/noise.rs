//! The Noise session between a relay client and the daemon
//! (`docs/relay-protocol.md` §4): `Noise_IK_25519_ChaChaPoly_SHA256`, the
//! daemon as responder. After the handshake one daemon protocol frame may
//! span several transport messages: each plaintext is a flag byte (0 = last
//! chunk, 1 = more follows) and up to `CHUNK` bytes of the frame.

use snow::{Builder, HandshakeState, TransportState};

pub const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
pub const PROLOGUE: &[u8] = b"jucode-relay-v1";
/// Frame bytes per transport message.
pub const CHUNK: usize = 65000;
/// The largest Noise message.
pub const MAX_MESSAGE: usize = 65535;
/// A reassembled frame larger than this ends the session, so a client that
/// never sends a last chunk cannot grow the daemon's memory without bound.
const MAX_FRAME: usize = 16 * 1024 * 1024;

fn params() -> snow::params::NoiseParams {
    PATTERN.parse().expect("valid Noise pattern")
}

/// A fresh X25519 keypair: (private, public).
pub fn generate_keypair() -> Result<(Vec<u8>, Vec<u8>), String> {
    let keypair = Builder::new(params())
        .generate_keypair()
        .map_err(|error| error.to_string())?;
    Ok((keypair.private, keypair.public))
}

/// The responder side of a handshake with `static_private` as `s`.
pub fn responder(static_private: &[u8]) -> Result<HandshakeState, String> {
    Builder::new(params())
        .local_private_key(static_private)
        .and_then(|builder| builder.prologue(PROLOGUE))
        .and_then(|builder| builder.build_responder())
        .map_err(|error| error.to_string())
}

/// The initiator side, knowing the responder's static key. The daemon never
/// initiates; tests and tools do.
pub fn initiator(static_private: &[u8], remote_public: &[u8]) -> Result<HandshakeState, String> {
    Builder::new(params())
        .local_private_key(static_private)
        .and_then(|builder| builder.remote_public_key(remote_public))
        .and_then(|builder| builder.prologue(PROLOGUE))
        .and_then(|builder| builder.build_initiator())
        .map_err(|error| error.to_string())
}

/// A finished handshake: seals frames into chunked transport messages and
/// reassembles received ones.
pub struct Transport {
    state: TransportState,
    pending: Vec<u8>,
}

impl Transport {
    pub fn new(handshake: HandshakeState) -> Result<Self, String> {
        Ok(Self {
            state: handshake
                .into_transport_mode()
                .map_err(|error| error.to_string())?,
            pending: Vec::new(),
        })
    }

    /// One frame as one or more transport messages, in order.
    pub fn seal(&mut self, frame: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut chunks: Vec<&[u8]> = frame.chunks(CHUNK).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let last = chunks.len() - 1;
        let mut buffer = vec![0u8; MAX_MESSAGE];
        let mut messages = Vec::with_capacity(chunks.len());
        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut plain = Vec::with_capacity(chunk.len() + 1);
            plain.push(u8::from(index != last));
            plain.extend_from_slice(chunk);
            let written = self
                .state
                .write_message(&plain, &mut buffer)
                .map_err(|error| error.to_string())?;
            messages.push(buffer[..written].to_vec());
        }
        Ok(messages)
    }

    /// Decrypts one transport message; returns the whole frame once its
    /// last chunk has arrived.
    pub fn open(&mut self, message: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let mut buffer = vec![0u8; MAX_MESSAGE];
        let read = self
            .state
            .read_message(message, &mut buffer)
            .map_err(|error| error.to_string())?;
        let (flag, chunk) = buffer[..read]
            .split_first()
            .ok_or_else(|| "empty transport message".to_string())?;
        if self.pending.len() + chunk.len() > MAX_FRAME {
            return Err("frame too large".to_string());
        }
        self.pending.extend_from_slice(chunk);
        match flag {
            0 => Ok(Some(std::mem::take(&mut self.pending))),
            1 => Ok(None),
            other => Err(format!("bad chunk flag {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handshake as the relay carries it: the initiator's payload reaches
    /// the responder, the responder learns the initiator's static key.
    fn handshake() -> (Transport, Transport) {
        let (host_private, host_public) = generate_keypair().unwrap();
        let (client_private, client_public) = generate_keypair().unwrap();
        let mut client = initiator(&client_private, &host_public).unwrap();
        let mut host = responder(&host_private).unwrap();
        let mut message = vec![0u8; MAX_MESSAGE];
        let mut payload = vec![0u8; MAX_MESSAGE];

        let written = client
            .write_message(br#"{"name":"phone"}"#, &mut message)
            .unwrap();
        let read = host
            .read_message(&message[..written], &mut payload)
            .unwrap();
        assert_eq!(&payload[..read], br#"{"name":"phone"}"#);
        assert_eq!(host.get_remote_static().unwrap(), client_public.as_slice());

        let written = host.write_message(br#"{"ok":true}"#, &mut message).unwrap();
        let read = client
            .read_message(&message[..written], &mut payload)
            .unwrap();
        assert_eq!(&payload[..read], br#"{"ok":true}"#);
        (
            Transport::new(client).unwrap(),
            Transport::new(host).unwrap(),
        )
    }

    fn deliver(from: &mut Transport, to: &mut Transport, frame: &[u8]) -> (usize, Vec<u8>) {
        let messages = from.seal(frame).unwrap();
        let count = messages.len();
        let mut result = None;
        for (index, message) in messages.iter().enumerate() {
            assert!(message.len() <= MAX_MESSAGE);
            let opened = to.open(message).unwrap();
            assert_eq!(opened.is_some(), index == count - 1);
            result = opened;
        }
        (count, result.unwrap())
    }

    #[test]
    fn frames_cross_in_chunks_both_ways() {
        let (mut client, mut host) = handshake();
        let small = br#"{"type":"hello"}"#;
        assert_eq!(deliver(&mut host, &mut client, small), (1, small.to_vec()));

        let big: Vec<u8> = (0..CHUNK * 2 + 17).map(|i| b'a' + (i % 26) as u8).collect();
        assert_eq!(deliver(&mut client, &mut host, &big), (3, big.clone()));
        let exact = vec![b'x'; CHUNK];
        assert_eq!(deliver(&mut host, &mut client, &exact), (1, exact.clone()));
        assert_eq!(deliver(&mut host, &mut client, b""), (1, Vec::new()));
    }

    #[test]
    fn a_tampered_message_is_refused() {
        let (mut client, mut host) = handshake();
        let mut message = client.seal(b"{}").unwrap().remove(0);
        message[0] ^= 1;
        assert!(host.open(&message).is_err());
    }

    #[test]
    fn a_wrong_host_key_fails_the_handshake() {
        let (host_private, _) = generate_keypair().unwrap();
        let (_, other_public) = generate_keypair().unwrap();
        let (client_private, _) = generate_keypair().unwrap();
        let mut client = initiator(&client_private, &other_public).unwrap();
        let mut host = responder(&host_private).unwrap();
        let mut message = vec![0u8; MAX_MESSAGE];
        let mut payload = vec![0u8; MAX_MESSAGE];
        let written = client.write_message(b"{}", &mut message).unwrap();
        assert!(host
            .read_message(&message[..written], &mut payload)
            .is_err());
    }
}
