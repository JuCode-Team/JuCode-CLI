# Cloud agent (groundwork)

Status: groundwork only. Nothing here is offered to users; there is no
cloud compute behind it yet. This document records what exists, how to run
it by hand, and what is deliberately left for later.

## Shape

A cloud agent is the same `jucode daemon` that runs on a user's machine,
in a container of its own: one container per user, the daemon unchanged
(single user, file storage, blocking threads — per-user isolation is what
lets it stay that way). Clients reach it exactly like a local daemon that
is away from home: through the relay (`docs/relay-protocol.md`), end-to-end
encrypted, with no port published. A client only has to choose where a
session runs.

```
 Desktop / PWA ──wss──▶ jucode-relay ◀──wss── daemon (user's machine)
                                  ▲
                                  └──wss── daemon container (per user, cloud)
                                             volumes: ~/.jucode, /workspace
```

## What exists

- **Image** — `docker/Dockerfile`: the jucode binary on Debian slim with
  git, ssh, ripgrep and curl, running as uid 1000 under tini (which reaps
  what tool commands leave behind). Volumes:
  - `/home/jucode/.jucode`: config, `auth.json`, sessions, daemon state,
    relay identity (the host id stays stable across restarts);
  - `/workspace`: the repositories the agent works in.
- **Entrypoint** — `docker/entrypoint.sh`:
  - seeds `config.json` on first start: the JuCode gateway, a model it
    serves (`JUCODE_MODEL`, default `gpt-6-sol`), and
    `"sandbox": "full-access"` — the container is the isolation boundary,
    and the engine's own sandbox (bubblewrap) needs user namespaces a
    container normally lacks;
  - turns the relay on, since it is the only way in;
  - runs `jucode daemon --listen 127.0.0.1:7788`.
- **Headless setup commands** (any server, not only containers):
  - `jucode daemon relay on|off|status`: live on a running daemon, saved
    for the next start on a stopped one;
  - `jucode daemon pair`: prints a pairing link and code for a phone or
    browser (valid 5 minutes, single use).
- **Clean exits** — the daemon ends its tool commands on SIGTERM (what
  `docker stop` sends), so stopping a container leaves nothing running.

## Running one by hand

```sh
docker build -f docker/Dockerfile -t jucode-daemon .
docker volume create jucode-home && docker volume create jucode-work

docker run -d --name jucode-agent --restart unless-stopped \
  -v jucode-home:/home/jucode/.jucode -v jucode-work:/workspace \
  jucode-daemon

# Credentials: model calls go through the JuCode gateway with the OAuth
# tokens of a logged-in machine (they refresh themselves).
docker cp ~/.jucode/auth.json jucode-agent:/home/jucode/.jucode/auth.json

docker exec jucode-agent jucode daemon relay status
docker exec jucode-agent jucode daemon pair   # open the link on the phone
```

Servers run x86_64: build with `docker buildx build --platform linux/amd64`
on the local machine and load the image on the server
(`docker save … | ssh … docker load`); do not build on the server.

## Reserved, not built

Each item needs a decision before it is built.

1. **Controller** — creates a user's volumes and container, starts and
   stops it, and places it on a host. Nothing manages containers yet.
2. **Credentials** — a copied `auth.json` is for manual runs only. A
   controller should inject a token the backend issues for that container,
   scoped to the gateway and revocable. BYOK in the cloud means storing
   users' provider keys encrypted on our side; undecided.
3. **Isolation** — run containers under gVisor (`runsc`) or Firecracker,
   with CPU, memory, pid and disk limits and an egress policy. The image
   assumes this: inside it, the agent has full access.
4. **Sleep and wake** — stop idle containers. Timers live in the daemon's
   store, so a stopped daemon cannot fire them: the controller has to read
   the next due time before stopping it and wake it then. A client that
   finds the host offline (relay close 4404) could also ask for a wake-up.
5. **Ownership** — pairing is code-based today. A cloud host should be
   bound to the JuCode account that owns it, so the account's devices can
   reach it without scanning a code.
6. **Clients** — label hosts as local or cloud, and let a new session pick
   where it runs.
7. **Billing and compliance** — model usage is already billed by the
   gateway; compute is not priced. Offering this publicly in mainland China
   requires generative-AI service filing.
