#!/bin/sh
# 把层3 TUN 入口的两个二进制暂存进 runtime/helper/,随 tauri.conf.json 的 resources(runtime→runtime)
# 打进 .app 的 Contents/Resources/runtime/helper —— 供 entry.rs 的一次性 sudo 安装脚本取源。
# ⚠️ 必须跑在 stage-runtime.sh 之后(它会 rm -rf runtime)。
#
# - vpnmgr-helper:随仓库源码构建(desktop/helper,root LaunchDaemon:监管 mihomo#2 + 路由对账)。
# - mihomo:只接受 app/mihomo-source.json 锁定的官方资源，不从 PATH 任选版本。
set -eu
cd "$(dirname "$0")"
if [ -n "${MIHOMO_VERSION:-}" ]; then
  echo '版本由 app/mihomo-source.json 锁定，请移除 MIHOMO_VERSION 覆盖。' >&2
  exit 1
fi
mkdir -p runtime/helper

if [ -n "${MIHOMO_BIN:-}" ]; then
  python3 stage-mihomo.py --output runtime/helper/mihomo --binary "$MIHOMO_BIN"
else
  python3 stage-mihomo.py --output runtime/helper/mihomo
fi
cargo build --locked --offline --release --manifest-path ../helper/Cargo.toml
cp ../helper/target/release/vpnmgr-helper runtime/helper/vpnmgr-helper
chmod 755 runtime/helper/vpnmgr-helper

# Apple Silicon 要求一切二进制至少 ad-hoc 签名才能 exec;重签保证一致(launchd 不校验身份)。
codesign --force --sign - runtime/helper/vpnmgr-helper

echo "staged runtime/helper:"; ls -lh runtime/helper | sed 's/^/  /'
