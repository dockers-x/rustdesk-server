# Configuration & Environment Variables

This document is the single reference for every option that the open‑source
RustDesk server binaries (`hbbs`, `hbbr`) understand: command‑line flags,
environment variables, and configuration files.

> **TL;DR** — For most people the command‑line flags shown by `hbbs --help` /
> `hbbr --help` are all you need. Environment variables are an alternative way to
> set the same options, plus a handful of extra tuning knobs that have no flag.

---

## How configuration is loaded

Both servers read their configuration from the following sources. For
**`hbbs`** the order of precedence, from highest to lowest, is:

1. **Command‑line flag** (e.g. `-p 21116`, `-k mykey`)
2. **`--config <file>`** — an INI file passed with `-c`/`--config`
3. **`.env`** — an INI file named `.env` in the working directory
4. **Inherited process environment** — variables exported before launch

A value set by a higher source overrides the same value from a lower one. Under
the hood every source is turned into a process environment variable, and the
code then reads that variable — so "flag", "config file" and "env var" are just
three ways to set the same thing.

For **`hbbr`** the precedence is: **flag** (`-b`, `-p`, `-k`) → **`.env`** →
**inherited environment**.

`RUST_LOG` and `LOG_FORMAT` are read from the inherited process environment.
Both binaries initialize logging before loading `.env` or the HBBS config file.

---

## `hbbs` — ID / rendezvous server

| Variable | CLI flag | Default | Description |
|---|---|---|---|
| `KEY` | `-k`, `--key` | `-` | Public key clients must use, a base64 secret key, or `-` / `_` to load or generate a key pair (`id_ed25519`, `id_ed25519.pub`). `-` and `_` have the same behavior, so explicitly passing `-k _` to `hbbs` is unnecessary. An explicitly empty value disables key validation; see [Keys](#keys-and-encryption). |
| `BIND` | `-b`, `--bind` | all interfaces | **Available since 1.1.17.** Local IPv4 or IPv6 address on which all `hbbs` TCP, UDP, and WebSocket listeners bind. This does not change the addresses advertised to clients. Supported by `--config`, `.env`, and the inherited environment. |
| `PORT` | `-p`, `--port` | `21116` | Main TCP/UDP listening port. `hbbs` also binds `PORT-1` (NAT type test) and `PORT+2` (WebSocket). |
| `RELAY-SERVERS` | `-r`, `--relay-servers` | *(empty)* | Optional relay server override handed to clients, as comma-separated `host` or `host:port` values. Leave empty when `hbbr` uses the same address as `hbbs` and the standard port `21117`; clients derive it automatically. Set this only when the relay uses a different IP/hostname or a non-standard port. |
| `RMEM` | `-M`, `--rmem` | `0` (system default) | UDP receive‑buffer size in bytes. Raise the OS limit first: `sudo sysctl -w net.core.rmem_max=52428800`. |
| *(config file)* | `-c`, `--config` | *(none)* | Path to an extra INI config file (see precedence above). |
| `TEST_HBBS` 🅴 | *(none)* | *(auto)* | UDP self‑test target checked at start‑up. Set to `no` to skip the check (useful behind some NATs/proxies), or to an explicit `host:port`. |
| `ALWAYS_USE_RELAY` 🅴 | *(none)* | `N` | `Y` forces every session through a relay (disables direct/hole‑punched connections). At runtime, send `always-use-relay Y` or `always-use-relay N` to the `hbbs` [loopback console](#runtime-console). |
| `DB_URL` 🅴 | *(none)* | `./db_v2.sqlite3` | Path/URL of the SQLite database file. See [Database](#database). |
| `MAX_DATABASE_CONNECTIONS` 🅴 | *(none)* | `1` | Size of the SQLite connection pool. |
| `MAX_PUNCH_REQUESTS` 🅴 | *(none)* | `100000` | Number of punch-request records kept in memory for the `punch-requests` [console](#runtime-console) command; the oldest is dropped when full. `0` disables recording. |
| `MAX_MESSAGE_KB` 🅴 | *(none)* | `64` | Largest message, in KiB, that `hbbs` accepts on its TCP and WebSocket listeners; a connection that sends a bigger one is dropped. Capped at `65536`. The default fits an online-status query for about 5000 standard 9–10 digit IDs, fewer with long custom IDs; a client whose query is too big shows no online status. |

🅴 = set through the inherited process environment.

> `PORT_FOR_API` / `KEY_FOR_API` are only used by RustDesk Server **Pro** and its
> API; they have no effect in the open‑source server.

---

## `hbbr` — relay server

| Variable | CLI flag | Default | Description |
|---|---|---|---|
| `KEY` | `-k`, `--key` | *(empty)* | The empty default intentionally disables relay key validation, avoiding key-pair setup and mismatch failures. To enable relay key validation, use the same non-empty key as `hbbs`; `-` / `_` have the same behavior and load or generate a key pair. An empty key allows clients without a matching key to use the relay, so choose this tradeoff deliberately on an exposed server. |
| `BIND` | `-b`, `--bind` | all interfaces | **Available since 1.1.17.** Local IPv4 or IPv6 address on which the relay TCP and WebSocket listeners bind. Supported by `.env` and the inherited environment; `hbbr` does not support `--config`. |
| `PORT` | `-p`, `--port` | `21117` | Relay listening port. `hbbr` also binds `PORT+2` for WebSocket relay. **Note:** when set via the `PORT` env var (not `-p`), `hbbr` listens on `PORT + 1`, so a shared `PORT=21116` makes `hbbs`=21116 and `hbbr`=21117. |

### Relay bandwidth / QoS

These have no CLI flag and can also be changed through the `hbbr`
[loopback console](#runtime-console) (`tb`, `sb`, `ls`, `dt`, `t`, …; send `h`
for help).

| Variable | Default | Unit | Description |
|---|---|---|---|
| `SINGLE_BANDWIDTH` | `128` | Mb/s | Normal maximum bandwidth for each relay connection. |
| `TOTAL_BANDWIDTH` | `1024` | Mb/s | Aggregate bandwidth cap shared by all relay connections. |
| `LIMIT_SPEED` | `32` | Mb/s | Per-connection cap applied after a connection is downgraded, and to IPs in `blacklist.txt`. |
| `DOWNGRADE_THRESHOLD` | `0.66` | ratio (0–1) | Fraction of `SINGLE_BANDWIDTH` that a connection's lifetime-average throughput must exceed to trigger downgrade. |
| `DOWNGRADE_START_CHECK` | `1800` | seconds | Delay before a connection becomes eligible for the lifetime-average downgrade check. |

Downgrade is decided independently for each connection; it does **not** check
aggregate relay congestion. After `DOWNGRADE_START_CHECK`, a connection is
capped to `LIMIT_SPEED` once its average throughput since it started exceeds
`SINGLE_BANDWIDTH * DOWNGRADE_THRESHOLD`. A lone transfer can therefore be
downgraded even when the relay is otherwise idle. `TOTAL_BANDWIDTH` is a
separate aggregate cap.

These may also be placed in `.env` using the uppercase spellings shown above
(e.g. `SINGLE_BANDWIDTH=256`).

### Blocklists / blacklists (files, not env vars)

`hbbr` reads two optional files from its working directory at start‑up:

* **`blacklist.txt`** — IPs that are **bandwidth‑limited** (one IP per line;
  anything after the first space on a line is ignored).
* **`blocklist.txt`** — IPs that are **refused** outright.

Both can also be edited live through the `hbbr` loopback console (`ba`/`br`,
`Ba`/`Br`).

### Runtime console

The runtime consoles are TCP command transports built into the services; they
are not `rustdesk-utils` commands or interactive standard-input consoles. A
connection from a loopback address is treated as a single console command:

```bash
# hbbs: toggle forced relay on PORT-1 (21115 by default)
printf 'always-use-relay Y' | nc 127.0.0.1 21115

# hbbr: list commands on its relay PORT (21117 by default)
printf 'h' | nc 127.0.0.1 21117
```

Use the corresponding configured ports if you changed `PORT`.

---

## Database

At runtime the database location comes from **`DB_URL`** (default
`./db_v2.sqlite3`). If unset, `hbbs` creates the SQLite file in its working
directory.

> **Do not confuse `DB_URL` with `DATABASE_URL`.** The `DATABASE_URL` entry in
> the repository's `.env` is used **only at compile time** by `sqlx` to check SQL
> queries; it is **not** read by the running server. Setting `DATABASE_URL` on a
> running server has no effect — use `DB_URL`.

---

## Health probes

Set `HBBS_HEALTH_PORT` and `HBBR_HEALTH_PORT` to enable one HTTP listener per
daemon. Standalone binaries leave the endpoint disabled when its setting is unset,
empty or `0`. Each health listener binds to `127.0.0.1` even if the daemon uses
a different protocol listener address.

- `GET /livez` returns HTTP 200 once the local HTTP listener can answer.
- `GET /readyz` returns HTTP 200 after the daemon initializes its protocol
  listeners and services a probe from its main I/O loop. It returns HTTP 503 if
  the loop cannot respond within 500 ms, including while initializing or
  rebinding listeners. A stopped daemon cannot respond to either endpoint.

The combined S6 image enables these on `21120` for HBBS and `21121` for HBBR.
They are available for Console diagnostics and local monitoring. Keep the
health ports unpublished. Docker health uses S6 to check the HBBS and HBBR
services. For Console and its database, query the Console `/health` endpoint
directly.

## Logging

Both binaries use `flexi_logger`, which honours the standard **`RUST_LOG`**
environment variable (default level `info`). Set it in the process environment
before launching the binary. A value in `.env` or `hbbs`'s `--config` file is
loaded too late and has no effect on logging.

```bash
RUST_LOG=debug LOG_FORMAT=json hbbs
```

The default output is text. `LOG_FORMAT=json` emits a JSON record per line
with UTC timestamp, level, component, event, and source location. Static log
messages are included where available. The formatter omits interpolated
values, including credentials, peer data and protocol frames. `RUST_LOG`
controls the level in both formats.

## Official version checks

HBBS normally checks the official RustDesk version service at startup and
once per day. Set `DISABLE_UPDATE_CHECK=Y` to skip starting the check task.

---

## Keys and encryption

The `KEY` / `-k` value can be:

* a **public key** string — clients must present the matching key;
* a **base64‑encoded 64‑byte secret key** — the server derives the public key
  from it;
* **`-` or `_`** — the server loads a key pair from the working directory or
  generates one on first start, writing `id_ed25519` (private) and
  `id_ed25519.pub` (public);
* **empty** — key validation is disabled. `hbbs` still loads or generates key
  files for signing but deliberately leaves its active validation key empty;
  `hbbr` neither loads nor generates a key. Both services then accept clients
  without validating a key. This is the intentional `hbbr` default; use a
  non-empty value when relay key validation is required.

`hbbs` defaults to `-`, so it already loads or generates a key pair without an
explicit `-k _`. `hbbr` intentionally defaults to an empty key to avoid
key-pair setup and mismatch failures. Leave it empty for the default mode
without key validation. To enable relay key validation, give it the same
non-empty key as `hbbs`; both services can reuse key material from a shared
working directory. The `_` value is not a stricter mode than `-` in the current
implementation. The client and transport test matrix is in the
[KX v1 compatibility investigation](kx-v1-compatibility.md).

To supply your own key pair, place `id_ed25519` and `id_ed25519.pub` in the
process's **current working directory** before first start. That directory may
differ from the directory containing the executable. For the supervisor Docker
image, the working directory is `/data`.

---

## Docker image variables

The supervisor image (`rustdesk/rustdesk-server-s6`) starts both binaries with
s6 and adds a few convenience variables handled by its service scripts, **not**
by `hbbs`/`hbbr` directly:

| Variable | Default | Description |
|---|---|---|
| `CONSOLE_ENABLED` | `1` | Set to `0` for separate Console deployments. |
| `RELAY` | `relay.example.com` | Passed to `hbbs` as `-r $RELAY` (your public address). |
| `ENCRYPTED_ONLY` | `0` | `1` adds `-k _` to both servers. This is redundant for `hbbs`, whose default is `-`, and opts `hbbr` into key validation instead of its intentional empty default. |
| `KEY_PUB` | *(unset)* | If set, written to `/data/id_ed25519.pub` on first start. |
| `KEY_PRIV` | *(unset)* | If set, written to `/data/id_ed25519` on first start. Provide **both** `KEY_PUB` and `KEY_PRIV`, or neither. |

Any variable from the tables above can also be passed straight through the
container's environment (e.g. `-e ALWAYS_USE_RELAY=Y`, `-e RUST_LOG=debug`).

The classic scratch image (`rustdesk/rustdesk-server`) contains only the
binaries and does **not** implement `RELAY`, `ENCRYPTED_ONLY`, `KEY_PUB`, or
`KEY_PRIV`; those variables are ignored by that image.

---

## Examples

### Command line — non-standard ports

```bash
# Tell clients where the relay listens because it is not using port 21117.
hbbs -p 22116 -r rustdesk.example.com:22117
hbbr -p 22117
```

### `.env` file (working directory)

```ini
# Non-standard ports shared by both binaries; hbbr listens on PORT+1.
relay-servers=rustdesk.example.com:22117
PORT=22116
```

### docker-compose

```yaml
services:
  rustdesk-server:
    image: rustdesk/rustdesk-server-s6:latest
    environment:
      - RELAY=rustdesk.example.com:21117
      - ALWAYS_USE_RELAY=Y
      - RUST_LOG=info
      - SINGLE_BANDWIDTH=256
    ports:
      - "21115:21115"
      - "21116:21116"
      - "21116:21116/udp"
      - "21117:21117"
      - "21118:21118"
      - "21119:21119"
    volumes: ["./data:/data"]
    restart: unless-stopped
```

### systemd

```ini
[Service]
Environment=ALWAYS_USE_RELAY=Y
Environment=RUST_LOG=info
ExecStart=/usr/bin/hbbs
```

---

## Port reference

| Port | Proto | Server | Purpose |
|---|---|---|---|
| 21115 | TCP | hbbs | NAT type test (`PORT-1`) |
| 21116 | TCP + UDP | hbbs | ID registration / rendezvous / hole punching (`PORT`) |
| 21117 | TCP | hbbr | Relay (`hbbr PORT`) |
| 21118 | TCP | hbbs | WebSocket rendezvous (`PORT+2`) |
| 21119 | TCP | hbbr | WebSocket relay (`hbbr PORT+2`) |

Ports 21118/21119 are only needed for the web client; you can omit them
otherwise.

## Console device authorization (v0.2.44)

With `MUST_LOGIN=Y`, ordinary punch and relay coordination requires both the
existing JWT validation and a live Console decision for the target device.
Set `RUSTDESK_API_RUSTDESK_API_SERVER` to Console v0.2.42 or newer and share its
`RUSTDESK_API_JWT_KEY`. The address must use HTTPS, or HTTP with an explicit
loopback IP in the same network namespace (for example `http://127.0.0.1:21114`).
The base path is preserved and `/api/server/authorize-connection` is appended.
Embedded URL credentials, query strings, fragments and redirects are rejected.

The server sends the stock client token and requested target ID to Console.
Only an explicit `{"allowed":true}` HTTP 200 response allows the ordinary path;
missing configuration, malformed/oversized responses, overload and errors deny.
The lookup has a three-second timeout and at most 32 concurrent requests.
Console checks current sessions, enabled accounts and devices, and device
owner/administrator authority. Address-book sharing does not confer routing
permission. Signed target-bound Switch Sides grants retain their existing path.

`MUST_LOGIN=N` retains legacy unauthenticated routing and does not enforce this
ACL. Direct-IP traffic and already established sessions do not pass this check.
Endpoint passwords/approval and network access controls remain necessary.

## Device admission and control permissions (v0.2.49)

Configure the same `RUSTDESK_API_SECURITY_HBBS_ADMISSION_TOKEN` on Console and
each hbbs that should enforce device admission. Use a random secret of at least
32 characters. Set `RUSTDESK_API_RUSTDESK_API_SERVER` on hbbs to the paired Console
v0.2.49 API as described above. Console enables its device-admission switch
after receiving an authenticated check-in from a paired hbbs.

Admission starts disabled. When enabled, matched approved device keys and
devices with a matching successful Console deployment can register. Operators
can review pending public-key fingerprints in Console. Revoking a device
blocks its next registration or new hbbs-coordinated connection. A paired
hbbs fails closed for these paths while the admission service is unavailable.
Existing sessions and direct-IP access follow the client's local settings.

With `MUST_LOGIN=Y`, Console can apply per-account restrictions to keyboard
and mouse, clipboard, and file transfer. hbbs attaches them to newly created
punch, LAN and relay negotiation messages. Only desktop targets that consume
the control-permissions field apply these restrictions. Android targets and
the signed Switch Sides grant flow follow their existing behavior.

## Console-managed blocklist

Use a Console and paired Server version that support managed blocklists. Set
`RUSTDESK_API_SECURITY_HBBS_ADMISSION_TOKEN` to the same secret on Console,
hbbs, and hbbr. Give both Server processes the Console base URL in
`RUSTDESK_API_RUSTDESK_API_SERVER`. Remote URLs require HTTPS. A cleartext URL
must use a numeric loopback address in the same network namespace.

Console's **Devices & access → Server blocklist** page shows which daemon has
reported applying the current rules. Each daemon polls every 15 seconds and
reports the applied revision on its next poll. A daemon retains its last good
rules in memory if refresh fails. After a restart it needs a successful
snapshot before it can enforce Console rules.

Device ID rules cover registration and new HBBS routing. IP address and CIDR
rules cover HBBS traffic and both legs of new and established HBBR relays.
A relayed leg has no authenticated device ID for terminating an existing relay
by ID. Direct connections that bypass HBBS and HBBR follow endpoint and network
controls. HBBR continues to read its local `blocklist.txt` and `blacklist.txt`.
For proxied WebSockets, restrict direct access to the WebSocket ports and have
the proxy overwrite the client IP header with the observed client address.

## WebClient guest sessions

Use an S6 deployment with Console, HBBS and HBBR in the same network namespace.
On Console, set `RUSTDESK_API_APP_WEB_GUESTS=true` and configure the server
public key in **Client settings**. On HBBS, set `RUSTDESK_GUEST_RELAY_ADDRESS`
to the same local HBBR instance's public address as used by controlled clients.
HBBS and Console share `RUSTDESK_API_SECURITY_HBBS_ADMISSION_TOKEN`. Keep the
standard WebSocket listeners internal to the deployment and expose
`/api/web-guests/relay` through the Console reverse proxy with WebSocket upgrades
and subprotocol negotiation enabled.

A guest link authorizes one registered device. Device approval or password
requirements still apply. Console ends the guest transport when the link
expires, is revoked, or matches a managed blocklist rule.
