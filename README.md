# VPN Management Gateway

**English** | [中文](./README.zh-CN.md)

> A self-hosted VPN management gateway. It runs multiple corporate VPNs side by
> side — each isolated in its own Docker container exposing a SOCKS5 exit — and a
> dedicated second mihomo instance routes traffic to them by domain / IP. Your
> existing Clash is left untouched: add one `vpn-router` node and subscribe to a
> rule set. Everything runs in Docker; zero new dependencies on the host.

## The problem it solves

Connecting to several corporate VPNs at once is painful: official clients fight
over routes and DNS, each installs its own drivers, and most only let you
connect to one gateway at a time. This gateway isolates each VPN into its own
container, unifies them behind SOCKS5 exits, and routes by domain / IP — so
multiple intranets are reachable simultaneously without interfering with each
other.

## Architecture

Three layers, traffic flowing outside-in:

```
your existing Clash ──(matches a routing rule)──▶ vpn-router node
                                                      │
        second mihomo (this tool) ── route by domain / IP ──▶ ch-1 / ch-2 / …
                                                      │
   one container per VPN (ch-{id}) ── EC / aTrust / openconnect / … ──▶ SOCKS5 exit ──▶ corporate intranet
```

- **Login**: GUI clients (EasyConnect / aTrust, or the BYO desktop) run noVNC,
  so you complete the interactive corporate login in your browser (re-login is
  a first-class action, for device re-binding). Headless clients log in via
  injected credentials with no noVNC.
- **Liveness**: the backend probes through SOCKS5 (`socks5h`, remote DNS, to
  your `probe_url`) to decide whether the intranet is truly reachable — *not*
  "the VNC connected".
- **Hot reload**: adding / changing a routing rule hot-reloads the mihomo
  config **without dropping existing connections**.

No Clash? An **entry mode** lets you point your system / browser proxy straight
at this tool's mihomo (`/entry/proxy.pac` or one-liners from
`/api/entry/setup-commands`): matched traffic goes through a VPN, everything
else stays direct.

## Getting started

### Prerequisites

Docker with the Compose v2 plugin (`docker compose`). Nothing else — every
runtime dependency lives inside the containers.

### Start

From the repository root:

```bash
./start.sh
```

`start.sh` is idempotent and does three things:

1. On first run, generates `.env` with random high ports (UI / mihomo proxy /
   mihomo controller) and a mihomo secret — then keeps it stable across runs.
2. On first run, renders `mihomo/config.yaml` from the template. An existing
   config (with the channels you've created) is preserved.
3. Runs `docker compose up -d --build`.

When it finishes it prints the endpoints — all bound to `127.0.0.1`:

| Endpoint | Env var | Purpose |
|---|---|---|
| Console (web UI) | `UI_PORT` | the management interface — open it in your browser |
| mihomo proxy | `MIHOMO_PORT` | point your Clash here (see the in-app "Clash config" button) |
| mihomo controller | `MIHOMO_CTRL_PORT` | mihomo external controller API |

The exact ports are in `.env`. The first run pulls / builds images, so it takes
a while.

### Stop

```bash
docker compose down
```

Delete a single VPN container from the UI, or `docker rm -f vpn-<id>`.

### Frontend-only development

To iterate on the UI without the backend or Docker:

```bash
cd app/static && python3 -m http.server 8080
```

### Tests

Run the host-side suite (FastAPI not required; deps are separate from the app
image):

```bash
pip install -r tests/requirements-dev.txt
pytest
```

## Supported VPNs

Adapters are declarative (`app/adapters.yaml`) and grouped into three families:

| Family | Login | Clients |
|---|---|---|
| **hagb** | interactive, via noVNC | EasyConnect, aTrust (upstream `hagb/docker-easyconnect` / `hagb/docker-atrust` images) |
| **oss** | headless (injected credentials) | Cisco AnyConnect, GlobalProtect, Fortinet, Juniper/Pulse, Ivanti, openfortivpn, OpenVPN, WireGuard — all share the self-built `vpnmgr/oss-vpn` image (`images/oss/`) |
| **byo** | bring-your-own, via noVNC | A `custom` Linux desktop (`vpnmgr/byo-desktop`, `images/byo/`) where you install any VPN GUI by hand — best-effort fallback for the long tail |

> The BYO fallback suits ordinary Linux GUI/CLI clients that bring their own tun
> and authenticate over the network only. It does **not** support
> systemd/dbus daemon clients, clients needing kernel modules absent on the
> host, hardware-token / smartcard / TPM binding, or Windows/macOS-only
> clients. Prefer the hagb / oss adapters for those.

## Repository layout

```
.
├── README.md / README.zh-CN.md   # this file (bilingual)
├── LICENSE                       # MIT (project's own code)
├── NOTICE                        # third-party / proprietary-software disclaimer
├── CONTRIBUTING.md
├── CHANGELOG.md
├── docker-compose.yml            # mihomo + app services; all ports bound to 127.0.0.1
├── start.sh                      # one-shot launcher
├── gen_env.py                    # generates .env (random ports + secret)
├── mihomo/config.template.yaml   # mihomo config template (rendered at first run)
├── images/                       # self-built container images (oss / byo)
├── app/                          # FastAPI backend + static frontend
├── tests/                        # pytest unit tests + smoke.sh
└── docs/
    ├── design.md                 # full design intent (start here for the "why")
    └── development.md            # architecture, invariants, contributor notes
```

## HTTP API

Source of truth is `app/main.py`.

| Method | Path | Notes |
|---|---|---|
| GET | `/api/vpn-types` | adapter list (drives the wizard's type grid) |
| GET | `/api/vpn-types/{type}/versions` | `{versions:[{tag, arch, usable_here}]}`, live from Docker Hub; empty list for non-versioned adapters |
| GET | `/api/channels` | channel list (each with `domains[]`, `ips[]`, `socks_endpoint`, `uptime`, …) |
| POST | `/api/channels` | create channel + start container (`name, vpn_type, server, ec_ver, login_method, username, password, probe_url, config{}`) |
| GET | `/api/channels/{cid}/login` | `{url}` (noVNC) — or `{login_mode:"headless"}` for headless adapters |
| POST | `/api/channels/{cid}/upload` | multipart upload → `{ok, package}` (BYO installer streamed into the data volume via `put_archive`) |
| GET | `/api/channels/{cid}/status` | **runs a SOCKS5 probe** → `{status, connected, latency_ms}` |
| POST | `/api/channels/{cid}/rules` | add routing rules (`patterns[]` or `pattern`, optional `kind: domain\|ip`; bare IPs auto-get `/32` or `/128`) → `{reload_status, domains, ips, added, rejected}` |
| PATCH | `/api/channels/{cid}/rules/{rid}` | enable / disable one rule (`enabled`) → `{ok, reload_status}` |
| DELETE | `/api/channels/{cid}/rules/{rid}` | delete one rule → `{ok, reload_status}` |
| POST | `/api/channels/{cid}/start` \| `/stop` | start / stop container → `{ok}` |
| DELETE | `/api/channels/{cid}` | delete channel → `{ok}` |
| GET | `/api/channels/{cid}/logs?tail=200` | container logs → `{lines}` |
| GET | `/api/system` | mihomo status / ports / controller |
| GET | `/api/connections` | mihomo live connections |
| GET | `/api/proxies` | mihomo proxies → `{proxies}` |
| GET | `/clash/vpn-rules.yaml` | rule-provider payload for Clash to subscribe (`text/plain`) |
| GET | `/api/clash-snippet` | node + rules to paste into your Clash (`text/plain`) |
| GET | `/entry/proxy.pac` | PAC file for the no-Clash entry mode |
| GET | `/api/entry/setup-commands` | per-platform proxy on/off commands |

> Plus `GET /` and a catch-all static mount that serve the single-page frontend.

## Channel state machine

```
creating ──▶ running ──▶ logged_in     (plus stopped, error)
```

- **running** — container is up but not logged in yet (awaiting login)
- **logged_in** — SOCKS5 probe passed (intranet truly reachable)

## Security

- Every host port binds to `127.0.0.1` only — **never** `0.0.0.0`.
- Credentials are Fernet-encrypted at rest; `master.key` is mode `0600` on the
  data volume; the API never returns ciphertext or secret fields. Headless
  adapters inject credentials over stdin, never on the command line; BYO
  installers are streamed into the data volume and never stored in SQLite.
- SOCKS5 (1080) is exposed on the Docker network only; noVNC is mapped to a
  random high port on `127.0.0.1`.

## Documentation

- [`docs/design.md`](./docs/design.md) — the original design rationale (a pre-implementation snapshot; some tech choices differ from the as-built code — see development.md for the current architecture).
- [`docs/development.md`](./docs/development.md) — architecture, the invariants that must not break, and contributor notes.
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — how to run, test, and contribute.

## License

MIT — see [`LICENSE`](./LICENSE). The MIT grant covers this project's own code
only; see [`NOTICE`](./NOTICE) for the third-party / proprietary-software
disclaimer.
