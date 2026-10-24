# Relay protocol (v2)

Lets a client signed in to a JuCode account (the Android and iOS apps, the
web app at `https://app.jucode.net/remote`, a desktop) reach the
`jucode daemon` of a computer signed in to the same account, from anywhere.
Three parties:

- **host**: the daemon. Keeps one outbound WebSocket to the relay while its
  computer is signed in to JuCode.
- **relay**: part of the JuCode backend (`api.jucode.net/v1/relay/*`, same
  binary and deploy as the rest of the API). Checks both ends' OAuth tokens,
  keeps the account's list of computers, and forwards opaque bytes between a
  client and its host. Holds no stream keys and sees no content.
- **client**: opens one WebSocket per connection to the relay, naming the
  host.

The account decides who may reach a computer; end-to-end encryption decides
who can read the stream: every client stream carries a Noise
`Noise_IK_25519_ChaChaPoly_SHA256` session between the client and the
daemon, and the relay only ever sees Noise handshake and transport messages.

Version 2 replaces v1 outright: v1 hosts and clients (no token, the
standalone `jucode-relay` under `/relay/v1`) no longer connect.

All base64 in this document is **base64url without padding**.

## 1. Keys and identities

The daemon keeps two keypairs in `<daemon state dir>/relay-identity.json`
(mode 0600), created on first use:

| Key | Algorithm | Use |
| --- | --- | --- |
| identity key | Ed25519 | proves the host id to the relay |
| static key | X25519 | the Noise responder static key (`s` in IK) |

**Host id** = first 16 bytes of `SHA-256(ed25519 public key)`, base64url →
22 characters. It names the computer at the relay, in the account's computer
list and in pairing links.

**Account owner** of a computer: the user whose access token the daemon
presented on its latest host connection. Signing the daemon in to another
account moves the computer there (§5).

**Client device**: an OAuth authorization of a client app (`jucode-mobile`,
`jucode-web`, `jucode-cli`) plus that install's own X25519 static keypair,
which never leaves the install. The daemon knows a paired device by its
static public key.

## 2. Pairing link

```
https://app.jucode.net/remote#pair=<host_id>.<host_static_pub>.<code>
```

- The origin is fixed (the web app), whatever relay URL the daemon uses.
  Clients always connect to their own configured relay; the link only names
  the computer and a code.
- `host_static_pub`: the daemon's X25519 public key (32 bytes). The client
  checks it against the `static_pub` the account's computer list gives for
  `host_id` and refuses a mismatch.
- `code`: a one-time pairing code from the daemon's `pair_link` op (8
  characters, 5 minutes, single use).
- The host must be in the client's account list (same account only).
- The fragment is never sent to any server.

## 3. Relay endpoints

Base: `wss://api.jucode.net/v1/relay`.

**Authentication on both sockets.** The upgrade request carries a valid
OAuth device access token. Native clients and the daemon send
`Authorization: Bearer <token>`; browsers, which cannot set headers on a
WebSocket, offer the subprotocols `jucode-relay` and
`jucode-bearer.<token>`, and the relay answers with `jucode-relay`. A
missing or invalid token fails the upgrade with HTTP **401**. Personal
access tokens and web session tokens are refused.

### 3.1 Host connection: `GET /v1/relay/host?host=<host_id>`

`host` routes the connection (the load balancer hashes on it). After the
upgrade:

1. relay → host, text: `{"t":"challenge","nonce":"<32 random bytes>"}`
2. host → relay, text:
   ```
   {"t":"auth","pub":"<ed25519 pub>","sig":"<sig over \"jucode-relay-v1:\" || nonce bytes>",
    "v":"<daemon version>","static":"<x25519 static pub>","name":"<hostname>","platform":"macos|windows|linux"}
   ```
3. The relay verifies the signature and that `HostID(pub)` equals the `host`
   parameter (else close 4400), registers the computer under the token's
   user (name only until the user renamed it), and replies
   `{"t":"ready","host":"<host_id>","user":"<user id>"}`. A newer connection
   for the same host replaces the older one (close 4409). A bad signature:
   close 4401.

Then all messages are **binary** frames:

```
byte 0      kind: 1 = open, 2 = data, 3 = close
bytes 1..4  stream id, u32 big-endian (allocated by the relay, never 0)
bytes 5..   payload (open and data)
```

- relay → host `open sid payload`: a client connected. The payload is a JSON
  object naming the caller, from its token:
  `{"user":"<user id>","authz":"<authorization id>","client":"jucode-mobile","name":"<authorization name>"}`.
- `data sid payload`: both directions; payload is one client message.
- `close sid`: both directions; the other side of the stream closes. The host
  sends it to drop a client (bad handshake, refused, revoked device).

Keepalive: the relay pings the host every 30 s. The host treats 75 s
without any frame as a dead connection.

Reconnecting: the host reconnects when its socket drops, with backoff 1 s,
2 s, 4 s … capped at 60 s, plus up to 1 s of jitter, back to 1 s once a
connection lasted 30 s. Close **4503** (`server restarting`, sent when a
backend instance drains) means reconnect after 1 s, without growing the
backoff. HTTP 401 on the upgrade: the host refreshes its token once and
retries; refused again, it stays off until its saved login changes.

### 3.2 Client connection: `GET /v1/relay/connect?host=<host_id>`

- Unknown host, or a host owned by another user → the relay accepts the
  upgrade and closes with **4403** (`not your computer`), so browsers can
  read the code.
- Host not connected → accept, then close **4404** (`host offline`).
- Otherwise the relay allocates a stream id, sends `open` with the caller
  payload to the host, and then forwards: each **binary** client message ⇄
  one `data` frame. Text frames from the client are a protocol error (close
  4400).
- Either side closing ends the stream (`close` to the host / WS close 4410
  `host closed` to the client). Host disconnect closes all its client
  streams with 4404; a draining instance closes them with 4503 (retry soon).

### 3.3 Limits (relay)

| Limit | Value |
| --- | --- |
| max message (client or data payload) | 1 MiB |
| streams per host | 32 (further connects close 4429) |
| new client connections per IP | 20 per minute (close 4429) |
| host auth attempts per IP | 10 per minute |
| challenge answered | 10 s |
| idle client stream (no traffic either way) | 10 min |

Terminal output is flow-controlled end to end by the daemon
(daemon-protocol.md, Terminals).

### 3.4 Account API

The computer list and notifications are ordinary backend API (OAuth access
token, under `/v1/oauth`): `GET /v1/oauth/computers`,
`PATCH`/`DELETE /v1/oauth/computers/:host_id`, `POST /v1/oauth/push/register`,
`GET /v1/oauth/push/vapid-key`, and `POST /v1/oauth/notify`, with which the
daemon notifies the account's other devices (daemon-protocol.md,
Notifications).

## 4. Noise session (client ⇄ daemon, inside one stream)

`Noise_IK_25519_ChaChaPoly_SHA256`, prologue = ASCII `jucode-relay-v1`.
The client is the initiator and knows the daemon static key from the
account's computer list (or the pairing link).

1. **msg 1** (client → daemon, `-> e, es, s, ss`), payload = JSON
   `{"name":"<device name>","pair":"<code>"?,"request":true?}`. `pair` only on
   the first connection after scanning a link; `request` only when the user
   asked to connect to a computer this install is not paired with. The
   daemon waits 10 s for msg 1.
2. The daemon authorizes the client static key `rs`, in order:
   1. the `open` payload's `user` is not the account the computer is
      registered under → refused, `wrong account`;
   2. `rs` belongs to a paired, unrevoked device → allowed as that device;
   3. `pair` is a valid unexpired code → paired now, allowed;
   4. `request` is true → a **connect request**: with no desktop (local
      client) connected, refused at once with `no_desktop`; with 3 requests
      already open, `busy`; otherwise the desktop is asked and the daemon
      waits up to 120 s: allowed → paired now; `denied`; `expired`; the
      client closing first cancels it;
   5. otherwise → refused, `not paired`.

   A device paired in step 3 or 4 is recorded keyed by `hex(SHA-256(rs))`,
   with the `open` payload's `authz`, `client` and `user`; its name is msg
   1's `name`, else the payload's `name`.
3. **msg 2** (daemon → client, `<- e, ee, se`), payload
   `{"ok":true,"device":"<device id>","name":"<device name>"}`, or
   `{"ok":false,"error":"<reason>"}` followed by `close`. In request mode msg 2
   may come up to 120 s after msg 1.
4. **Transport**: after msg 2 both sides split into cipher states. Each WS
   message is one Noise transport message. Noise messages are ≤ 65535 bytes,
   so daemon protocol frames (one JSON document each, as on the local
   WebSocket) are chunked:

   ```
   plaintext = flag (1 byte: 0 = last chunk, 1 = more follows) || chunk
   ```

   chunks of up to 65000 bytes; the receiver concatenates until flag 0 and
   parses the result as one daemon protocol frame (UTF-8 JSON).

After the handshake the stream behaves like a local client with the device
restrictions (`docs/daemon-protocol.md`): `hello` first, the same ops and
events. Revoking a device closes its relay streams.

## 5. Daemon

- `--relay <wss url>` (default `wss://api.jucode.net/v1/relay`) overrides the
  relay for development; `--no-relay` disables it.
- The host connection is made only while the computer is signed in to JuCode
  (the token comes from the JuCode login and is refreshed as needed) and the
  `relay` setting is not `false`. Signed out, nothing connects, and a
  connected host leaves the relay.
- `ready.user` is kept as `relay_user` in `settings.json`. When it differs
  from the stored one, every paired device is revoked before any stream is
  served.
- Local-client ops: `relay_status`
  (`{enabled, connected, signed_in, host, url, user}`), `relay_set {enabled}`,
  `pair_link`, `connect_requests`, `connect_decide`, `device_list`,
  `device_revoke` (daemon-protocol.md, Remote access).

## 6. Test vectors / interop

`crates/daemon` ships `examples/noise_peer.rs`: a responder that reads Noise
messages as hex lines on stdin and writes replies as hex lines on stdout, with
a fixed static key given on the command line. The JS side has a Node test that
runs its initiator against it (handshake, one chunked frame each way). Keep
both implementations in step with this document.
