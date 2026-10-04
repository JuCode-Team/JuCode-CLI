# Relay protocol (v1)

Lets a paired phone (the PWA at `https://app.jucode.net`) reach a desktop's
`jucode daemon` from outside the LAN. Three parties:

- **host**: the daemon. Keeps one outbound WebSocket to the relay.
- **relay**: `jucode-relay` (Go, JuCode-backend `cmd/jucode-relay`). Forwards
  opaque bytes between a client and its host. Holds no keys, sees no content,
  stores nothing on disk.
- **client**: the PWA (or any device). Opens one WebSocket per session to the
  relay, naming the host.

End-to-end encryption: every client stream carries a Noise
`Noise_IK_25519_ChaChaPoly_SHA256` session between the client and the daemon.
The relay only ever sees Noise handshake and transport messages.

All base64 in this document is **base64url without padding**.

## 1. Keys and identities

The daemon keeps two keypairs in `<daemon state dir>/relay-identity.json`
(mode 0600), created on first use:

| Key | Algorithm | Use |
| --- | --- | --- |
| identity key | Ed25519 | authenticates the host to the relay |
| static key | X25519 | the Noise responder static key (`s` in IK) |

**Host id** = first 16 bytes of `SHA-256(ed25519 public key)`, base64url →
22 characters. It names the host at the relay and in pairing links.

Each client keeps its own X25519 static keypair (the PWA: localStorage key
`jucode-relay-device`, JSON `{priv, pub}`, base64url). A paired device is
identified to the daemon by its static public key.

## 2. Pairing link

Desktop shows a QR code / link:

```
https://<relay host>/remote#pair=<host_id>.<host_static_pub>.<code>
```

The link is on the relay's own origin (`--relay wss://host/relay/v1` →
`https://host/remote`; `app.jucode.net` by default).

- `host_static_pub`: the daemon's X25519 public key (32 bytes, base64url).
- `code`: a one-time pairing code from the existing `pair_start` op (8 chars,
  5 minutes, single use).
- The fragment is never sent to any server. The PWA reads it, stores
  `{host_id, host_static_pub, relay: "wss://<link origin>/relay/v1"}` under
  localStorage `jucode-relay-host`, then clears the fragment
  (`history.replaceState`).

## 3. Relay endpoints

Base: `wss://app.jucode.net/relay/v1` (Caddy terminates TLS and proxies to the
relay on `127.0.0.1:18095`; the relay itself speaks plain WS/HTTP).

### 3.1 Host connection: `GET /relay/v1/host`

After the WebSocket upgrade:

1. relay → host, text: `{"t":"challenge","nonce":"<32 random bytes, b64>"}`
2. host → relay, text:
   `{"t":"auth","pub":"<ed25519 pub, b64>","sig":"<sig over \"jucode-relay-v1:\" || nonce bytes, b64>","v":"<daemon version>"}`
3. relay verifies the signature, derives the host id from `pub`, and replies
   `{"t":"ready","host":"<host_id>"}`. A newer authenticated connection for the
   same host id replaces the older one (the older one is closed with code 4409).
   Failure: close code 4401.

Then all messages are **binary** frames:

```
byte 0      kind: 1 = open, 2 = data, 3 = close
bytes 1..4  stream id, u32 big-endian (allocated by the relay, never 0)
bytes 5..   payload (data only)
```

- relay → host `open sid`: a client connected. Payload empty.
- `data sid payload`: both directions; payload is one client message.
- `close sid`: both directions; the other side of the stream closes. The host
  sends it to drop a client (bad handshake, revoked device).

Keepalive: the relay pings the host every 30 s (WebSocket ping); the host
reconnects when its socket drops, with backoff 1 s, 2 s, 4 s … capped at 60 s,
plus jitter.

### 3.2 Client connection: `GET /relay/v1/connect?host=<host_id>`

- Host not connected → the relay accepts the upgrade and immediately closes
  with code **4404** (reason `host offline`), so browsers can read the code.
- Otherwise the relay allocates a stream id, sends `open` to the host, and then
  forwards: each **binary** client message ⇄ one `data` frame. Text frames from
  the client are a protocol error (close 4400).
- Either side closing ends the stream (`close` to the host / WS close 4410
  `host closed` to the client). Host disconnect closes all its client streams
  with 4404.

### 3.3 Limits (relay)

| Limit | Value |
| --- | --- |
| max message (client or data payload) | 1 MiB |
| streams per host | 32 (further connects close 4429) |
| new client connections per IP | 20 per minute (close 4429) |
| host auth attempts per IP | 10 per minute |
| handshake (challenge answered) timeout | 10 s |
| idle client stream (no traffic either way) | 10 min |

`GET /relay/v1/healthz` → `200 ok`.
Logs: connects/disconnects with host id and stream counts; never payloads.

### Web Push and Getui

The relay sends Web Push notifications, and 个推 (Getui) notifications to the
JuCode Android app, for connected hosts; it keeps no subscriptions and stores
nothing it passes on.

- `GET /relay/v1/push/key` → `{"key":"<VAPID public key, base64url>"}`: the
  `applicationServerKey` a browser subscribes with.
- `POST /relay/v1/push`, body
  `{"pub":"<host Ed25519 public key>","ts":<ms>,"subscription":{"endpoint","keys":{"p256dh","auth"}},"payload":{…}}`,
  header `X-JuCode-Signature`: base64url Ed25519 signature over
  `"jucode-relay-push-v1:" + body`. Accepted only from a host connected now,
  within 5 minutes of `ts`, at most 60 per host per minute, for endpoints of
  the browser push services, payload at most 3 KB. Replies `204` sent, `410`
  the subscription is gone (forget it), `401`/`403`/`400`/`429`/`502`
  otherwise.
- The same request with `"subscription":{"provider":"getui","client_id":"<cid>"}`
  goes to the Android app through Getui REST API v2 (its own channel while the
  app runs, the phone vendor's channel otherwise). `payload` is
  `{"title","body","session"?}` (title required); tapping the notification
  opens the app with `type=relay`, `host=<host id>` and `session`. Same checks
  and replies; `410` when Getui does not know the client id, `501` when Getui
  (or, for a Web Push subscription, VAPID) is not configured.

The relay serves Web Push with `-vapid-file` (keys created on first start)
and Getui with the environment variables `GETUI_APP_ID`, `GETUI_APP_KEY`,
`GETUI_MASTER_SECRET` (and optionally `GETUI_VENDOR_OPTIONS`, a JSON object
passed as `push_channel.android.ups.options`, e.g.
`{"XM":{"/extra.channel_id":"…"},"OP":{"/channel_id":"…"}}`); with neither,
these endpoints are not served.

## 4. Noise session (client ⇄ daemon, inside one stream)

`Noise_IK_25519_ChaChaPoly_SHA256`, prologue = ASCII `jucode-relay-v1`.
The client is the initiator and knows the daemon static key from pairing.

1. **msg 1** (client → daemon, `-> e, es, s, ss`), payload = JSON
   `{"name":"<device name>","pair":"<code>"}`. `pair` is present only on the
   first connection after scanning a link.
2. The daemon authorizes the client static key `rs`:
   - `rs` belongs to a paired, non-revoked device → allowed as that device;
   - else, `pair` is a valid unexpired code → the device is paired now
     (`devices.jsonl` entry with `token_hash = hex(SHA-256(rs))`, so revocation
     and listing reuse the existing device store), allowed;
   - else → the daemon sends msg 2 with payload `{"ok":false,"error":"..."}`
     and then `close`.
3. **msg 2** (daemon → client, `<- e, ee, se`), payload
   `{"ok":true,"device":"<device id>","name":"<device name>"}`.
4. **Transport**: after msg 2 both sides split into cipher states. Each WS
   message is one Noise transport message. Noise messages are ≤ 65535 bytes,
   so daemon protocol frames (one JSON document each, as on the local
   WebSocket) are chunked:

   ```
   plaintext = flag (1 byte: 0 = last chunk, 1 = more follows) || chunk
   ```

   chunks of up to 65000 bytes; the receiver concatenates until flag 0 and
   parses the result as one daemon protocol frame (UTF-8 JSON).

After the handshake the stream behaves exactly like a device-authenticated
local WebSocket: `hello` first, the same ops and events
(`docs/daemon-protocol.md`, `docs/serve-protocol.md`), the same restrictions
on device clients. Revoking a device closes its relay streams.

## 5. Daemon configuration

`jucode daemon` flags / config:

- `--relay <wss url>` (default `wss://app.jucode.net/relay/v1`),
  `--no-relay` to disable. Desktop turns it on with a setting
  ("允许通过中继远程访问").
- New local-client ops (not allowed for device clients):
  - `relay_status` → `{type:"relay_status", enabled, connected, host, url}`
  - `pair_link` → like `pair_start` but also returns
    `{link:"https://app.jucode.net/remote#pair=..."}`; an error while the
    relay is off.
  - `relay_set {enabled}` → turns the relay on or off (persisted in
    `settings.json`) and replies `relay_status`. Off by default.

## 6. Test vectors / interop

`crates/daemon` ships `examples/noise_peer.rs`: a responder that reads Noise
messages as hex lines on stdin and writes replies as hex lines on stdout, with
a fixed static key given on the command line. The JS side has a Node test that
runs its initiator against it (handshake, one chunked frame each way). Keep
both implementations in step with this document.
