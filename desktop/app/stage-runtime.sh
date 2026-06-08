#!/bin/sh
# 把自带运行时(colima/limactl/lima + lima share + docker CLI)暂存进 runtime/,供 `cargo tauri build`
# 经 tauri.conf.json 的 resources 打进 .app 的 Contents/Resources/runtime —— 真零安装的前提
# (用户机无需装 colima/docker)。runtime/ 已 gitignore(87MB 二进制不入库),构建前跑本脚本生成。
#
# 当前从本机 Homebrew Cellar 复制(开发机已装 colima 0.10.3 / lima 2.1.2 / Docker Desktop docker)。
# ⚠️ 发布级应改为下载官方 Homebrew-free 制品(lima/colima GitHub release + download.docker.com 的开源
#    静态 docker 客户端,Apache-2.0),以脱离 Homebrew 并厘清 docker 客户端授权来源。见 spec §5 改造B。
set -eu
cd "$(dirname "$0")"
BREW="${HOMEBREW_PREFIX:-/opt/homebrew}"
COLIMA_BIN="$(ls "$BREW"/Cellar/colima/*/bin/colima | head -1)"
LIMA_DIR="$(ls -d "$BREW"/Cellar/lima/*/ | head -1)"
DOCKER_BIN="$(command -v docker)"
[ -x "$COLIMA_BIN" ] || { echo "未找到 colima(brew install colima)"; exit 1; }
[ -d "$LIMA_DIR" ]    || { echo "未找到 lima(brew install lima)"; exit 1; }
[ -x "$DOCKER_BIN" ]  || { echo "未找到 docker CLI"; exit 1; }

echo "colima: $COLIMA_BIN"; echo "lima:   $LIMA_DIR"; echo "docker: $DOCKER_BIN"
rm -rf runtime && mkdir -p runtime/bin runtime/share/lima

cp -L "$COLIMA_BIN"            runtime/bin/colima
cp -L "$LIMA_DIR/bin/limactl"  runtime/bin/limactl
cp -L "$LIMA_DIR/bin/lima"     runtime/bin/lima          # colima 依赖检查要 `lima`(limactl shell 的 shell 包装)
cp -L "$(readlink -f "$DOCKER_BIN" 2>/dev/null || echo "$DOCKER_BIN")" runtime/bin/docker  # colima runtime=docker 的宿主依赖检查要它
# lima 经二进制相对路径 ../share/lima 找 guestagent;只带 Linux guestagent + templates
cp -RL "$LIMA_DIR/share/lima/lima-guestagent.Linux-aarch64.gz" runtime/share/lima/
cp -RL "$LIMA_DIR/share/lima/templates"                        runtime/share/lima/
chmod -R u+w runtime

# vz 后端必须的 entitlement;ad-hoc 自签即可授(非受限、不需 provisioning,见 spec §8)。
# Homebrew 的 limactl 本就 adhoc+该 entitlement;重签确保打包后仍带、且与我们一致。
codesign --force --sign - --entitlements entitlements.plist --options runtime runtime/bin/limactl
codesign --force --sign - --entitlements entitlements.plist --options runtime runtime/bin/colima

echo "staged $(du -sh runtime | cut -f1):"; find runtime -type f | sed 's/^/  /'
