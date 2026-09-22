# Hillstone Secure Connect 镜像

[下载镜像（约 369 MiB）](https://github.com/coxenlab/multi-vpn-gateway/releases/download/containers-20260922/hillstone-desktop-20260922-linux-arm64.tar.gz) · [下载 SHA-256 文件](https://github.com/coxenlab/multi-vpn-gateway/releases/download/containers-20260922/hillstone-desktop-20260922-linux-arm64.tar.gz.sha256) · [版本清单](./20260922-linux-arm64.json)

- 平台：Linux arm64；Apple Silicon Mac 使用应用内 Linux VM，VM 需支持 x86_64 客户端转译。
- 镜像标签：`vpnmgr/hillstone-desktop:latest`。
- 打包日期：2026-09-22；基础镜像构建于 2026-09-07。日期是镜像包版本，不是厂商客户端版本。
- 内容：预装 Hillstone 客户端、桌面、noVNC 和 SOCKS5 服务；使用者自行登录。

## 校验下载

把镜像包和 SHA-256 文件放在同一文件夹，在该文件夹打开终端执行：

```sh
shasum -a 256 -c hillstone-desktop-20260922-linux-arm64.tar.gz.sha256
```

应显示 `OK`。发布的 SHA-256 为：

```text
072bf92eee46f8b57d382dd774a25a020594a57383d55e73103fbbc87e6210e5
```

## macOS App 导入

适用于安装在 `/Applications/vpnmgr.app` 的 Apple Silicon 版本。先在 App 中让运行环境进入已就绪状态。当前 1.3.1 的主界面没有镜像包导入入口，可使用 App 自带的 Docker 命令导入；无需另装 Docker Desktop。

在下载文件夹执行：

```sh
"/Applications/vpnmgr.app/Contents/Resources/runtime/bin/docker" \
  --host "unix://$HOME/.colima/vpnmgr/docker.sock" \
  image load --input hillstone-desktop-20260922-linux-arm64.tar.gz
```

然后回到 App 新建通道，选择 **Hillstone Secure Connect**，在登录窗口中填写服务器、账号及验证信息。内网连通检测通过后再配置分流规则。

如果提示无法连接 Docker，请先检查 App 的运行环境是否已就绪；不要切换到其他项目的 Docker 环境导入。

## 自行部署的 Docker 环境

确认当前 Docker 引擎是要运行 VPN 管理网关的 Linux arm64 引擎，并已配置 x86_64 转译，再执行：

```sh
docker image load --input hillstone-desktop-20260922-linux-arm64.tar.gz
docker image inspect vpnmgr/hillstone-desktop:latest
```

导入只准备镜像模板，不创建或重启现有通道，不导入任何账号或通道配置。之后在管理界面创建 Hillstone 通道并登录。

## 发布核验

本次导出来自已有基础镜像；所有镜像层的内容摘要已核对，并检查了镜像构建历史、环境配置及各层中的用户目录。没有包含运行容器、挂载卷或登录配置。当前源码的归档预览与规范化导入校验通过；本轮没有重新进行厂商登录、从空白 VM 载入或长期稳定性测试。

镜像构建定义见 [`images/hillstone/`](../../../images/hillstone/)，厂商客户端条款见 [第三方声明](../../../NOTICE)。
