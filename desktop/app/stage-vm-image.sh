#!/bin/sh
# 内置 colima VM 磁盘镜像(ubuntu minimal cloudimg,~317MB):首启免连 GitHub。
# colima 下载缓存的 key = sha256(下载 URL),文件放 ~/Library/Caches/colima/caches/<key>;
# 壳启动时(main.rs seed_vm_image_cache)把 bundle 里这份预置进对方机器的同一路径,
# colima start 命中缓存即跳过下载 —— 国内网络连不上 GitHub 也能完成首启。
# ⚠️ URL 由 bundle 的 colima 版本决定(colima 0.10.3 → colima-core v0.10.4);升级 colima 时同步改这里。
# vm-image/ 已 gitignore(构建前跑本脚本从本机缓存取)。
set -eu
cd "$(dirname "$0")"
URL="https://github.com/abiosoft/colima-core/releases/download/v0.10.4/ubuntu-24.04-minimal-cloudimg-arm64-docker.raw.gz"
KEY=$(printf %s "$URL" | shasum -a 256 | awk '{print $1}')
SRC="$HOME/Library/Caches/colima/caches/$KEY"
[ -f "$SRC" ] || { echo "本机没有 VM 镜像缓存 $SRC;先在本机跑一次 colima start 生成"; exit 1; }
mkdir -p vm-image
cp "$SRC" "vm-image/$KEY"
ls -lh "vm-image/$KEY" | awk '{print "  staged vm-image:", $5, $NF}'
