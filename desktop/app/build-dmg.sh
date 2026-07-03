#!/bin/sh
# 一键出可分发的自签 .dmg:暂存自带运行时(colima/limactl/lima/docker + lima share)+ 内置镜像
# (oss-vpn tarball)→ cargo tauri build --bundles dmg。
# 产物:target/release/bundle/dmg/vpnmgr_<ver>_aarch64.dmg —— 内含 ad-hoc 自签 .app + /Applications
# 拖拽符号链接;双击 .app 即起自带 colima VM、首启 docker-load oss 镜像、进 6 屏 UI(用户机无需装 colima/docker)。
#
# ⚠️ v1 自用/内测,未公证:本机构建无 quarantine 可直接双击;经下载/AirDrop 传到他机会染 quarantine,
#    Gatekeeper 拦,需右键→打开 或 `xattr -dr com.apple.quarantine /path/to/vpnmgr.app`。公证为后续阶段。
set -eu
cd "$(dirname "$0")"
./stage-runtime.sh   # 自带运行时二进制(从 Homebrew Cellar 暂存 + 重签 vz entitlement)
./stage-images.sh    # 内置镜像 tarball(docker save vpnmgr/oss-vpn | gzip)
./stage-helper.sh    # 层3 TUN 入口:vpnmgr-helper(构建)+ mihomo darwin 二进制 → runtime/helper
cargo tauri build --bundles dmg
echo "✓ dmg: $(ls -1 target/release/bundle/dmg/*.dmg | tail -1)"
