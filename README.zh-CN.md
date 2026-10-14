# VPN 管理网关

[English](./README.md) | **中文**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](./LICENSE)
[![Platform](https://img.shields.io/badge/platform-Docker%20%7C%20macOS%2014%2B-lightgrey?style=flat-square)](#安装)
[![LINUX DO](https://img.shields.io/badge/LINUX%20DO-%E8%AE%A4%E5%8F%AF%E7%A4%BE%E5%8C%BA-FFB003?style=flat-square)](https://linux.do)

> 同时连多家企业 VPN：每家关进一个 Docker 容器、暴露一个 SOCKS5 出口，再由一个独立的 mihomo 按域名 / IP 分流。现有 Clash 一字不改，只加一个 `vpn-router` 节点并订阅一份规则。

## 解决什么问题

官方客户端会互相抢路由和 DNS，通常一次只能连一家。本工具把每家 VPN 隔离在各自容器里，多家内网同时可达、互不干扰。

```
你现有的 Clash ──(命中规则)──▶ vpn-router
                                  │
        本工具的 mihomo ── 按域名 / IP 分流 ──▶ ch-1 / ch-2 / …
                                  │
        每家 VPN 一个容器 ── SOCKS5 出口 ──▶ 企业内网
```

- **登录**：EasyConnect / aTrust 等有界面的客户端在登录窗口（noVNC）中手动登录；OpenConnect、OpenVPN、WireGuard 等无界面客户端用保存的凭据自动登录。
- **是否连上**：以经 SOCKS5 访问内网验证地址的结果为准，不以「窗口打开了」为准。
- **没有 Clash**：由虚拟网卡和系统代理接管，命中规则的走 VPN，其余直连。

## 安装

### macOS 应用「通道管家」（推荐）

1. 到 [最新 Release](https://github.com/coxenlab/multi-vpn-gateway/releases/latest) 下载 `.dmg`：`with-vm`（推荐）已内置 Linux VM 镜像；`lite` 体积小，首次启动时在线下载 VM 镜像。
2. 打开 `.dmg`，把 `Tongdao.app` 拖进「应用程序」。
3. 安装包只做了 ad-hoc 签名、未经 Apple 公证，首次打开会被拦截，需要先去掉隔离属性：

   ```bash
   xattr -dr com.apple.quarantine /Applications/Tongdao.app
   ```

需要 Apple Silicon、macOS 14 及以上。应用自带 Linux VM，不用另装 Docker。应用会安装一个以 root 运行的助手程序，用于配置虚拟网卡和系统代理，请先核对 Release 里的 SHA-256。

1.3.x 的旧应用 `vpnmgr.app` 可以和通道管家并存；搬通道配置用旧应用「设置 → 备份 → 导出」，再在通道管家里导入。从源码构建见 [`desktop/tongdao/README.md`](./desktop/tongdao/README.md)。

### Docker Compose（自行部署）

需要 Docker 和 Compose v2。在仓库根目录：

```bash
./start.sh            # 首次生成 .env（随机端口与密钥），之后可重复执行
docker compose down   # 停止
```

端口只绑 `127.0.0.1`，具体见 `.env`：`UI_PORT` 是管理界面，`MIHOMO_PORT` 给 Clash 接。

## 支持的 VPN

| 类型 | 登录方式 | 说明 |
|---|---|---|
| EasyConnect / aTrust | 在登录窗口中登录 | 上游 [hagb](https://github.com/Hagb/docker-easyconnect) 镜像 |
| 开源协议 | 自动登录 | AnyConnect、GlobalProtect、Fortinet、Juniper / Pulse、openfortivpn、OpenVPN、WireGuard |
| 通用安装 | 在登录窗口中登录 | 把厂商官方 Linux 安装包（`.deb`，或脚本 / AppImage）拖进来，在容器里安装并打开客户端，例如山石 Secure Connect |

通用安装适合「自带 tun、只做网络认证」的 Linux 客户端；RPM 包、硬件令牌 / 智能卡，以及只有 Windows / macOS 版的客户端不支持。

## 安全

- 所有端口只绑 `127.0.0.1`。
- 密码等凭据加密存储，接口不回传；凭据经标准输入注入容器，不出现在命令行。
- 导出的备份含自动登录所需凭据，请妥善保管。

## 开发

- [`docs/development.md`](./docs/development.md)：架构、不能破坏的约定、接口要点。
- [`CONTRIBUTING.md`](./CONTRIBUTING.md)：怎么跑、怎么测。
- [`docs/design.md`](./docs/design.md)：最初的设计稿（实现前的快照，部分选型与现状不同）。

## 致谢

项目在 [LINUX DO](https://linux.do) 社区分享与讨论，并基于：[mihomo](https://github.com/MetaCubeX/mihomo)、[noVNC](https://github.com/novnc/noVNC)、[hagb/docker-easyconnect](https://github.com/Hagb/docker-easyconnect)、[Colima](https://github.com/abiosoft/colima) / [Lima](https://github.com/lima-vm/lima)、[Dante](https://www.inet.no/dante/)、[microsocks](https://github.com/rofl0r/microsocks)，以及 OpenConnect / OpenVPN / WireGuard。

## 许可

MIT，见 [`LICENSE`](./LICENSE)，只覆盖本项目自有代码；第三方与闭源软件说明见 [`NOTICE`](./NOTICE)。
