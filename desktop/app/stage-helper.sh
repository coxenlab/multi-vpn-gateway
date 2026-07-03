#!/bin/sh
# 把层3 TUN 入口的两个二进制暂存进 runtime/helper/,随 tauri.conf.json 的 resources(runtime→runtime)
# 打进 .app 的 Contents/Resources/runtime/helper —— 供 entry.rs 的一次性 sudo 安装脚本取源。
# ⚠️ 必须跑在 stage-runtime.sh 之后(它会 rm -rf runtime)。
#
# - vpnmgr-helper:随仓库源码构建(desktop/helper,root LaunchDaemon:监管 mihomo#2 + 路由对账)。
# - mihomo:宿主 TUN 引擎(darwin-arm64)。优先 env MIHOMO_BIN → 本机已装 → GitHub release 下载。
set -eu
cd "$(dirname "$0")"
MIHOMO_VERSION="${MIHOMO_VERSION:-v1.19.27}"
mkdir -p runtime/helper

cargo build --release --manifest-path ../helper/Cargo.toml
cp ../helper/target/release/vpnmgr-helper runtime/helper/vpnmgr-helper

if [ -n "${MIHOMO_BIN:-}" ] && [ -x "${MIHOMO_BIN:-}" ]; then
  echo "mihomo: $MIHOMO_BIN(env 指定)"
  [ "$MIHOMO_BIN" -ef runtime/helper/mihomo ] || cp -L "$MIHOMO_BIN" runtime/helper/mihomo
elif command -v mihomo >/dev/null 2>&1; then
  echo "mihomo: $(command -v mihomo)(本机已装)"
  cp -L "$(command -v mihomo)" runtime/helper/mihomo
else
  echo "mihomo: 下载 GitHub release ${MIHOMO_VERSION}"
  curl -fL "https://github.com/MetaCubeX/mihomo/releases/download/${MIHOMO_VERSION}/mihomo-darwin-arm64-${MIHOMO_VERSION}.gz" \
    | gunzip > runtime/helper/mihomo
fi
chmod 755 runtime/helper/vpnmgr-helper runtime/helper/mihomo

# Apple Silicon 要求一切二进制至少 ad-hoc 签名才能 exec;重签保证一致(launchd 不校验身份)。
codesign --force --sign - runtime/helper/vpnmgr-helper runtime/helper/mihomo

echo "staged runtime/helper:"; ls -lh runtime/helper | sed 's/^/  /'
