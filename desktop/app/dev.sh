#!/bin/bash
# 独立开发实例；与日常 profile、数据、端口和宿主全局入口分离。
set -euo pipefail
cd "$(dirname "$0")"
export VPNMGR_DEV_MODE=1
export VPNMGR_MANAGED_VM=1
export VPNMGR_VM_PROFILE=vpnmgr-dev
export DATA_DIR="${VPNMGR_DEV_DATA_DIR:-$HOME/Library/Application Support/vpnmgr-dev}"
export DOCKER_HOST="unix://$HOME/.colima/$VPNMGR_VM_PROFILE/docker.sock"
export VPN_NET=vpnmgr_dev_vpnnet
export UI_PORT=48878
export MIHOMO_HOST_PORT=48879
export MIHOMO_CTRL_PORT=48880
export MIHOMO_CTRL_URL="http://127.0.0.1:$MIHOMO_CTRL_PORT"
export MIHOMO_CONFIG_PATH="$DATA_DIR/config.yaml"
unset MIHOMO_SECRET HELPER_RES_DIR
mkdir -p "$DATA_DIR"
chmod 700 "$DATA_DIR"
if [[ "${1:-}" == --core ]]; then
    exec cargo run --manifest-path ../core/Cargo.toml
fi
exec cargo run --manifest-path Cargo.toml
