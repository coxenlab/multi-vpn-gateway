# VPN 容器镜像下载

按客户端选择镜像包，下载后导入，再在 VPN 管理网关中创建通道并登录。镜像包包含预装客户端和运行依赖；每位使用者自行填写账号。

| 客户端 | 可用平台 | 下载与导入 |
|---|---|---|
| Hillstone Secure Connect（山石网科） | Linux arm64；Apple Silicon Mac 的应用内 VM | [下载镜像包](https://github.com/coxenlab/multi-vpn-gateway/releases/download/containers-20260922/hillstone-desktop-20260922-linux-arm64.tar.gz) · [校验值](https://github.com/coxenlab/multi-vpn-gateway/releases/download/containers-20260922/hillstone-desktop-20260922-linux-arm64.tar.gz.sha256) · [导入说明](./hillstone/README.md) |
| ZLink | 待验证 | 容器登录、隧道和内网访问验证通过后再提供下载 |

当前 Hillstone 包约 **369 MiB**，打包日期为 **2026-09-22**。它的桌面和网络组件是 arm64，厂商客户端是 x86_64；Apple Silicon 的 Linux VM 需要启用 Rosetta 转译。此包不适用于 Intel / amd64 引擎。

镜像文件存放在本仓库的 [容器资源 Release](https://github.com/coxenlab/multi-vpn-gateway/releases/tag/containers-20260922)，本目录保存下载索引、校验值和导入说明。容器资源独立发布，App 安装包仍从 [最新 App Release](https://github.com/coxenlab/multi-vpn-gateway/releases/latest) 下载。

下载包是 Docker image save 模板，不含运行容器的数据卷。导入模板后仍需自行登录，并以实际内网连通检测为准。厂商客户端适用其自身条款，见 [第三方声明](../../NOTICE)。

## 后续增加镜像

每个客户端在本目录下有一个子目录；每批发布提供 `.tar.gz`、同名 `.sha256` 和版本清单 JSON。清单记录架构、字节数、SHA-256、镜像标签、配置摘要、来源镜像摘要和验证范围。

从干净构建的基础镜像执行 `docker image save`；不使用 `docker commit` 或 `docker export` 打包已登录的运行容器。厂商安装器版本和镜像构建日期分别记录，无法核实的版本不猜填。保留既有下载文件，新版本采用新文件名和 Release 标签。

ZLink 准备完成后，按相同格式新增 `zlink/` 清单及下载入口。
