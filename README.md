# VPN Management Gateway

**English** | [中文](./README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](./LICENSE)
[![Platform](https://img.shields.io/badge/platform-Docker%20%7C%20macOS%2014%2B-lightgrey?style=flat-square)](#install)
[![LINUX DO](https://img.shields.io/badge/LINUX%20DO-community-FFB003?style=flat-square)](https://linux.do)

> Connect to several corporate VPNs at once. Each VPN runs in its own Docker
> container with a SOCKS5 exit, and a dedicated mihomo instance routes traffic
> to them by domain / IP. Your existing Clash stays untouched: add one
> `vpn-router` node and subscribe to a rule set.

## Why

Official clients fight over routes and DNS, and most let you connect to only
one gateway at a time. This tool isolates each VPN in its own container, so
several intranets are reachable at the same time without interfering.

```
your Clash ──(rule match)──▶ vpn-router
                                 │
      this tool's mihomo ── route by domain / IP ──▶ ch-1 / ch-2 / …
                                 │
      one container per VPN ── SOCKS5 exit ──▶ corporate intranet
```

- **Login**: GUI clients such as EasyConnect / aTrust are logged in by hand in
  a login window (noVNC); headless clients (OpenConnect, OpenVPN,
  WireGuard, …) log in with saved credentials.
- **Connected means reachable**: a channel counts as connected only when a
  probe through its SOCKS5 exit reaches your intranet check address.
- **No Clash?** The app takes over with a virtual interface and the system
  proxy: matched traffic goes through a VPN, everything else stays direct.

## Install

### macOS app "Tongdao" (recommended)

1. Download a `.dmg` from the [latest release](https://github.com/coxenlab/multi-vpn-gateway/releases/latest):
   `with-vm` (recommended) bundles the Linux VM image; `lite` is smaller and
   downloads the VM image on first launch.
2. Open the `.dmg` and drag `Tongdao.app` into Applications.
3. The app is ad-hoc signed and not notarized, so Gatekeeper blocks the first
   launch. Clear the quarantine attribute:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Tongdao.app
   ```

Requires Apple Silicon and macOS 14+. The app bundles its own Linux VM, so no
Docker installation is needed. It installs a privileged helper running as root
to manage the virtual interface and system proxy; verify the SHA-256 from the
release first.

The legacy 1.3.x app `vpnmgr.app` can stay installed alongside Tongdao; move
channels with Settings → Backup → Export in the old app, then import them in
Tongdao. To build from source, see [`desktop/tongdao/README.md`](./desktop/tongdao/README.md).

### Docker Compose (self-hosted)

Requires Docker with Compose v2. From the repository root:

```bash
./start.sh            # first run creates .env (random ports and secret); safe to re-run
docker compose down   # stop
```

All ports bind to `127.0.0.1`; see `.env` for `UI_PORT` (web UI) and
`MIHOMO_PORT` (point your Clash here).

## Supported VPNs

| Type | Login | Notes |
|---|---|---|
| EasyConnect / aTrust | in the login window | upstream [hagb](https://github.com/Hagb/docker-easyconnect) images |
| Open protocols | automatic | AnyConnect, GlobalProtect, Fortinet, Juniper / Pulse, openfortivpn, OpenVPN, WireGuard |
| Generic install | in the login window | drag in the vendor's official Linux package (`.deb`, or a script / AppImage); it is installed and launched inside the container — e.g. Hillstone Secure Connect |

Generic install suits Linux clients that bring their own tun device and use
network-only authentication. RPM packages, hardware tokens / smart cards, and
clients that exist only for Windows / macOS are not supported.

## Security

- Every port binds to `127.0.0.1` only.
- Credentials are encrypted at rest and never returned by the API; they are
  passed to containers over stdin, never on the command line.
- Exported backups contain auto-login credentials — keep them private.

## Development

- [`docs/development.md`](./docs/development.md) — architecture, invariants, API notes.
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — how to run and test.
- [`docs/design.md`](./docs/design.md) — the original design (a pre-implementation snapshot; some choices differ from the current code).

## Acknowledgements

Shared and discussed in the [LINUX DO](https://linux.do) community. Built on
[mihomo](https://github.com/MetaCubeX/mihomo),
[noVNC](https://github.com/novnc/noVNC),
[hagb/docker-easyconnect](https://github.com/Hagb/docker-easyconnect),
[Colima](https://github.com/abiosoft/colima) / [Lima](https://github.com/lima-vm/lima),
[Dante](https://www.inet.no/dante/), [microsocks](https://github.com/rofl0r/microsocks),
and the OpenConnect / OpenVPN / WireGuard projects.

## License

MIT — see [`LICENSE`](./LICENSE). It covers this project's own code only; see
[`NOTICE`](./NOTICE) for third-party and proprietary software.
