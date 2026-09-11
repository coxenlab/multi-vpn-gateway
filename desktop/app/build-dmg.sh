#!/bin/sh
# macOS 分发入口：旧 Web 界面 + 原生生命周期壳 + owned Rust core。
# --check 只核对已准备资源；实际构建必须显式传 --output 新目录。
# 不自动启动 VM、导出 Docker 镜像、安装助手或覆盖已安装应用。
set -eu
cd "$(dirname "$0")"
exec python3 ../native/build-release.py "$@"
