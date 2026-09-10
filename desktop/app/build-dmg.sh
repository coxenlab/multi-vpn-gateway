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
./stage-runtime.sh   # 固定来源与校验值；VPNMGR_RUNTIME_VARIANT 可显式选择候选补丁
./stage-images.sh    # 内置镜像 tarball(docker save vpnmgr/oss-vpn | gzip)
./stage-vm-image.sh  # 内置 colima VM 磁盘镜像(首启免连 GitHub,预置下载缓存)
./stage-helper.sh    # 层3 TUN 入口:vpnmgr-helper(构建)+ mihomo darwin 二进制 → runtime/helper
# 打包完整性自检:内置 VM 镜像 / mihomo tarball 任缺则中止——缺 vm-image 的包在国内
# 网络首启会退回连 GitHub(红队 F5),缺 mihomo 会退回镜像源拉取,都不该静默流出。
[ -s vm-image/"$(ls vm-image 2>/dev/null | head -1)" ] || { echo "✗ vm-image/ 为空,先跑 ./stage-vm-image.sh"; exit 1; }
[ -s bundled-images/mihomo.tar.gz ] || { echo "✗ bundled-images/mihomo.tar.gz 缺失,先跑 ./stage-images.sh"; exit 1; }
cargo tauri build --bundles dmg
DMG=$(ls -1 target/release/bundle/dmg/*.dmg | tail -1)
echo "✓ dmg: $DMG"
# 随包出 sha256:传输(微信/网盘)损坏可先校验,否则只会在 Gatekeeper 处以「已损坏」误导排查
shasum -a 256 "$DMG" | tee "$DMG.sha256"
