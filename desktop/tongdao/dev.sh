#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export VPNMGR_DEV_MODE=1 VPNMGR_MANAGED_VM=1 VPNMGR_VM_PROFILE=tongdao-dev
export DATA_DIR="${TONGDAO_DEV_DATA_DIR:-$HOME/Library/Application Support/com.tongdao.desktop-dev}"
export VPN_NET=tongdao_dev_vpnnet
export STATIC_DIR="$ROOT/app/ui"
export VPNMGR_BUNDLED_IMAGES_DIR="$ROOT/desktop/app/bundled-images"
export VPNMGR_CORE_PATH="$ROOT/desktop/core/target/debug/vpnmgr-core"
unset UI_PORT MIHOMO_HOST_PORT MIHOMO_CTRL_PORT MIHOMO_CTRL_URL MIHOMO_SECRET MIHOMO_CONFIG_PATH HELPER_RES_DIR
mkdir -p "$DATA_DIR"
chmod 700 "$DATA_DIR"
CARGO_TARGET_DIR="$ROOT/desktop/core/target" cargo build --locked --offline --manifest-path "$ROOT/desktop/core/Cargo.toml"
exec swift run --package-path "$ROOT/desktop/tongdao" Tongdao
