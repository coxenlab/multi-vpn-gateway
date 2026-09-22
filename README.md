# VPN Management Gateway

**English** | [中文](./README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](./LICENSE)
[![Platform](https://img.shields.io/badge/platform-Docker%20%7C%20macOS%2014%2B-lightgrey?style=flat-square)](#getting-started)
[![LINUX DO](https://img.shields.io/badge/LINUX%20DO-community-FFB003?style=flat-square)](https://linux.do)

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

### macOS desktop app (optional)

Besides the Compose stack, the repository contains a native macOS app
(`desktop/`): a SwiftUI shell hosting the same web UI, a Rust core that owns the
local HTTP API, a privileged helper for the route-level TUN entry, and a bundled
Colima / Lima Linux VM so the host needs no Docker installation. Build it from
source:

```bash
./desktop/app/build-dmg.sh --check                            # verify staged build inputs
./desktop/app/build-dmg.sh --output "$HOME/Downloads/vpnmgr"  # build the .app + .dmg
```

Requires macOS 14+ on arm64. Builds are **ad-hoc signed only** — Developer ID
signing and notarization are not provided, so a downloaded build is blocked by
Gatekeeper until you clear its quarantine attribute. Prebuilt `.dmg` files are
attached to the [latest release](https://github.com/coxenlab/multi-vpn-gateway/releases/latest)
with SHA-256 checksums and a bundled `THIRD-PARTY-LICENSES.txt`; note that the
app installs a root-level privileged helper to manage the TUN entry. See
[`desktop/native/README.md`](./desktop/native/README.md) for the staged inputs,
what the build verifies, and what still needs on-device acceptance.

### Prebuilt VPN container images

[Container downloads](./downloads/containers/README.md) provides the Hillstone
Secure Connect Linux arm64 image, SHA-256 checksums and import instructions.
Container archives are separate Release assets; ZLink will be listed after its
container login and connectivity checks pass.

### Frontend-only development

To iterate on the UI without the backend or Docker:

```bash
cd app/static && python3 -m http.server 8080
```

### Tests

Run the host-side suite (FastAPI not required; deps are separate from the app
image):

```bash
pip install --require-hashes --only-binary=:all: -r tests/requirements-dev.txt
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
├── desktop/                      # macOS app: SwiftUI shell, Rust core, privileged helper
├── tests/                        # pytest unit tests + smoke.sh
└── docs/
    ├── design.md                 # full design intent (start here for the "why")
    └── development.md            # architecture, invariants, contributor notes
```

## HTTP API

Web mode's source of truth is `app/main.py`; desktop mode additionally exposes runtime-event routes registered in `desktop/core/src/server.rs`.

| Method | Path | Notes |
|---|---|---|
| GET | `/api/vpn-types` | adapter list (drives the wizard's type grid) |
| GET | `/api/vpn-types/{type}/versions` | `{versions:[{tag, arch, usable_here}]}`, live from Docker Hub; empty list for non-versioned adapters |
| GET | `/api/channels` | channel list (each with `domains[]`, `ips[]`, `socks_endpoint`, `uptime`, …) |
| POST | `/api/channels` | create channel + start container (`name, vpn_type, server, ec_ver, login_method, username, password, probe_url, config{}`) |
| PATCH | `/api/channels/{cid}` | update channel fields; changed connection parameters recreate the target container, while metadata does not; desktop also accepts `routing_enabled` |
| GET | `/api/channels/{cid}/login` | `{url}` (noVNC) — or `{login_mode:"headless"}` for headless adapters |
| PUT / DELETE | `/api/channels/{cid}/login/viewers/{viewer}` | desktop only: renew / release a view acquired by `login?viewer=<UUID>`; response includes `viewer_id` and a 60-second lease; no-viewer clients retain legacy lifetime |
| POST | `/api/channels/{cid}/upload` | Nonempty multipart installer, ≤1 GiB → `{ok, package}`; disk-spooled and streamed to the data volume. Partial metadata failure returns `uploaded:true`; check the channel before retrying. |
| GET | `/api/channels/{cid}/status` | **runs a SOCKS5 probe** → `{status, connected, latency_ms}` |
| GET | `/api/channels/{cid}/health` | shared probe cache for automatic refresh, with `checked_at` / `stale`; stale results refresh in the background; manual checks still use `/status` |
| POST | `/api/channels/{cid}/rules` | add routing rules (`patterns[]` or `pattern`, optional `kind: domain\|ip`; bare IPs auto-get `/32` or `/128`) → `{reload_status, domains, ips, added, rejected}` |
| PATCH | `/api/channels/{cid}/rules/{rid}` | update one rule (`enabled`, or desktop-only `note` / `locked`) → `{ok, rule, reload_status?}` |
| PATCH | `/api/rules` | batch enable / disable with `ids[]` and `enabled`; desktop additionally skips locked rules and returns `skipped_locked` |
| DELETE | `/api/channels/{cid}/rules/{rid}` | delete one rule → `{ok, reload_status}` |
| POST | `/api/channels/{cid}/start` \| `/stop` | start / stop container → `{ok}` |
| POST | `/api/channels/{cid}/restore` | restore the previous settings while a replacement awaits verification; saved memos are retained |
| DELETE | `/api/channels/{cid}` | delete channel → `{ok}` |
| GET | `/api/channels/{cid}/logs?tail=200` | container logs → `{lines}` |
| GET \| PUT | `/api/channels/{cid}/note` | encrypted channel memo `{note}`, read back only through this endpoint |
| GET | `/api/config/export` | export channels and rules, retaining routing switches and rule enabled/note/locked fields; includes automatic-login credentials, so keep the file private |
| POST | `/api/config/import` | JSON backup up to 16 MiB; import stopped channels, preserving rule metadata; returns imported and skipped items |
| POST | `/api/config/retry` | retry saved routing configuration; confirm managed rules/proxies and persist the boot config |
| GET | `/api/preflight` | environment checks; optional `vpn_type`, `version`, and `scope=full` for a complete diagnostic; may run a temporary TUN probe |
| POST | `/api/preflight/fix/{action}` | `create_network` or `pull_image`; a pull returns `task_id`, or 429 when workers are occupied |
| GET | `/api/preflight/fix/{task_id}` | image-download progress/result; 404 means the task record is unavailable, so inspect images before starting another pull |
| GET | `/api/images` | image inventory and available versions for the current architecture |
| GET / POST | `/api/mirrors` | list download mirrors / add one with `{host}` |
| PATCH / DELETE | `/api/mirrors/{mid}` | update mirror `priority` / `enabled`, or remove it |
| POST | `/api/mirrors/test` | test a mirror's HTTPS `/v2/` endpoint with `{host}`; returns reachability and latency |
| GET | `/api/system` | mihomo status / ports / controller and `config_application` (pending, confirmed generation/time); desktop also returns cached `usernet` diagnostics and the last egress-guard check |
| POST | `/api/system/self-heal` | desktop-only in-memory watchdog action toggle (`enabled`); app restart restores it |
| GET \| POST | `/api/routing` | desktop-only persistent global direct-routing switch (`{off}`); preserves raw rule states |
| GET | `/api/connections` | mihomo live connections |
| GET | `/api/proxies` | mihomo proxies → `{proxies}` |
| GET | `/clash/vpn-rules.yaml` | rule-provider payload for Clash to subscribe (`text/plain`) |
| GET | `/api/clash-snippet` | node + rules to paste into your Clash (`text/plain`) |
| GET | `/entry/proxy.pac` | PAC file for the no-Clash entry mode |
| GET | `/api/entry/setup-commands` | per-platform proxy on/off commands |
| GET | `/api/events` | desktop runtime events; filters include `since_seq`, `level`, `src`, `event`, `q`, and `limit` |
| GET | `/api/events/export?days=2` | export the desktop's last 1–14 days of runtime events as JSONL |
| GET \| POST | `/api/events/enabled` | desktop runtime event recording toggle `{enabled}` |

> `GET /` and a catch-all static mount serve the shared Web pages. Desktop-only TUN, system-proxy, and container-diagnostic routes are registered in `desktop/core/src/server.rs`.

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
- SOCKS5 (1080) is exposed on the Docker network only. In desktop mode noVNC
  container port 8080 is not published; the app owns a per-channel SSH forward
  on a random `127.0.0.1` high port. Web mode keeps its Docker loopback mapping.

## Documentation

- [`docs/design.md`](./docs/design.md) — the original design rationale (a pre-implementation snapshot; some tech choices differ from the as-built code — see development.md for the current architecture).
- [`docs/development.md`](./docs/development.md) — architecture, the invariants that must not break, and contributor notes.
- [`desktop/native/README.md`](./desktop/native/README.md) — the macOS app: lifecycle, release build, and the limits of its verification.
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — how to run, test, and contribute.

## Acknowledgements

This project is developed in the open, and endorses the
[LINUX DO](https://linux.do) community, where it is shared and discussed.

It also stands on:

- [mihomo](https://github.com/MetaCubeX/mihomo) — the engine behind the second-layer routing
- [noVNC](https://github.com/novnc/noVNC) — the browser VNC client used for interactive logins
- [hagb/docker-easyconnect](https://github.com/Hagb/docker-easyconnect) — upstream containers for the EasyConnect / aTrust adapters
- [Colima](https://github.com/abiosoft/colima) and [Lima](https://github.com/lima-vm/lima) — the Linux VM bundled with the macOS app
- [Dante](https://www.inet.no/dante/), [microsocks](https://github.com/rofl0r/microsocks), and the OpenConnect / OpenVPN / WireGuard projects — the SOCKS5 exits and tunnels inside the containers

## License

MIT — see [`LICENSE`](./LICENSE). The MIT grant covers this project's own code
only; see [`NOTICE`](./NOTICE) for the third-party / proprietary-software
disclaimer.
