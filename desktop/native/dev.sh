#!/usr/bin/env bash
set -euo pipefail
NATIVE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export VPNMGR_DEV_MODE=1 VPNMGR_MANAGED_VM=1 VPNMGR_VM_PROFILE=vpnmgr-native-dev
export DATA_DIR="${VPNMGR_NATIVE_DATA_DIR:-$HOME/Library/Application Support/vpnmgr-native-dev}"
export VPN_NET=vpnmgr_native_dev_vpnnet
export STATIC_DIR="$NATIVE_ROOT/app/static"
export VPNMGR_CORE_PATH="$NATIVE_ROOT/desktop/core/target/debug/vpnmgr-core"
unset UI_PORT MIHOMO_HOST_PORT MIHOMO_CTRL_PORT MIHOMO_CTRL_URL MIHOMO_SECRET MIHOMO_CONFIG_PATH HELPER_RES_DIR
mkdir -p "$DATA_DIR"
chmod 700 "$DATA_DIR"
CARGO_TARGET_DIR="$NATIVE_ROOT/desktop/core/target" cargo build --locked --offline --manifest-path "$NATIVE_ROOT/desktop/core/Cargo.toml"
exec swift run --package-path "$NATIVE_ROOT/desktop/native" VPNManager
