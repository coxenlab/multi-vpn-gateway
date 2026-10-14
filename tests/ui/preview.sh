#!/usr/bin/env bash
# 界面联调用的隔离核心：编译 → 塞示例通道 → 后台起核心伺服 app/ui → 导入示例备份 → 打印地址。
# 用法（仓库根执行）：bash tests/ui/preview.sh [--self-test]
# 自带隔离，不依赖调用方：HOME、DATA_DIR 都是本脚本 mktemp 出来的目录，开发模式、非托管、
# 档名 tongdao-verify-*、控制口 http://127.0.0.1:1、DOCKER_HOST 指向不存在的套接字；调用方传进来的同名变量一律不用。
# 不起运行环境，登录、探活、流量看不到；运行态的通道会显示成出错。结束时只停本脚本起的那个进程。
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET_DIR=/tmp/tongdao-target/core

if [ "${1:-}" != "__inner" ]; then
  case "${1:-}" in ""|--self-test) ;; *) echo "用法: bash tests/ui/preview.sh [--self-test]" >&2; exit 2 ;; esac
  CARGO_BIN="$(command -v cargo || true)"
  NODE_BIN="$(command -v node || true)"
  [ -n "$CARGO_BIN" ] || { echo "[preview] 找不到 cargo" >&2; exit 1; }
  [ -n "$NODE_BIN" ] || { echo "[preview] 找不到 node" >&2; exit 1; }
  ROOT="$(mktemp -d /tmp/tongdao-preview.XXXXXX)"
  mkdir -p "$ROOT/home" "$ROOT/data" "$ROOT/tmp"
  exec env -i \
    PATH="$(dirname "$CARGO_BIN"):$(dirname "$NODE_BIN"):/usr/bin:/bin:/usr/sbin:/sbin" \
    HOME="$ROOT/home" USER="${USER:-}" LANG="${LANG:-en_US.UTF-8}" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    TMPDIR="$ROOT/tmp" PREVIEW_ROOT="$ROOT" \
    DATA_DIR="$ROOT/data" MIHOMO_CONFIG_PATH="$ROOT/mihomo.yaml" \
    VPNMGR_DEV_MODE=1 VPNMGR_MANAGED_VM=0 VPNMGR_VM_PROFILE="tongdao-verify-${ROOT##*.}" \
    DOCKER_HOST="unix://$ROOT/absent-docker.sock" MIHOMO_CTRL_URL="http://127.0.0.1:1" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
    bash "$0" __inner "${1:-}"
fi

MODE="${2:-}"
ROOT="$PREVIEW_ROOT"
PID=""
cleanup() {
  if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -rf "$ROOT"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

cd "$REPO"
echo "[preview] root=$ROOT" >&2
cargo build --locked --offline --manifest-path desktop/core/Cargo.toml --bin vpnmgr-core >&2
cargo run --locked --offline --manifest-path desktop/core/Cargo.toml --example seed_demo >&2

LOG="$ROOT/core.log"
STATIC_DIR="$REPO/app/ui" UI_PORT=0 "$TARGET_DIR/debug/vpnmgr-core" 2>"$LOG" &
PID=$!
BASE=""
for _ in $(seq 1 100); do
  BASE="$(sed -n 's/.*listening on \(http:\/\/127\.0\.0\.1:[0-9]*\).*/\1/p' "$LOG" | head -n 1)"
  [ -n "$BASE" ] && break
  kill -0 "$PID" 2>/dev/null || { cat "$LOG" >&2; echo "[preview] 核心没起来" >&2; exit 1; }
  sleep 0.1
done
[ -n "$BASE" ] || { cat "$LOG" >&2; echo "[preview] 等不到 listening on" >&2; exit 1; }

IMPORT="$(curl -fsS -X POST -H 'Content-Type: application/json' \
  --data-binary @tests/ui/fixtures/backup-example.json "$BASE/api/config/import")"
echo "[preview] import: $IMPORT" >&2

if [ "$MODE" = "--self-test" ]; then
  fail() { echo "[self-test] 失败: $*" >&2; exit 1; }
  ENVS="$(ps -E -ww -o command= -p "$PID" | tr ' ' '\n')"
  val() { printf '%s\n' "$ENVS" | sed -n "s/^$1=//p" | tail -n 1; }
  for k in HOME DATA_DIR VPNMGR_DEV_MODE VPNMGR_MANAGED_VM VPNMGR_VM_PROFILE MIHOMO_CTRL_URL DOCKER_HOST; do
    echo "[self-test] $k=$(val "$k")"
  done
  case "$(val HOME)" in "$ROOT"/*) ;; *) fail "HOME 不在 $ROOT 下" ;; esac
  case "$(val DATA_DIR)" in "$ROOT"/*) ;; *) fail "DATA_DIR 不在 $ROOT 下" ;; esac
  [ "$(val VPNMGR_DEV_MODE)" = 1 ] || fail "VPNMGR_DEV_MODE"
  [ "$(val VPNMGR_MANAGED_VM)" = 0 ] || fail "VPNMGR_MANAGED_VM"
  case "$(val VPNMGR_VM_PROFILE)" in tongdao-verify-*) ;; *) fail "VPNMGR_VM_PROFILE" ;; esac
  [ "$(val MIHOMO_CTRL_URL)" = "http://127.0.0.1:1" ] || fail "MIHOMO_CTRL_URL"
  sock="$(val DOCKER_HOST)"; sock="${sock#unix://}"
  [ -n "$sock" ] && [ ! -e "$sock" ] || fail "DOCKER_HOST 指向的套接字存在或为空"
  [ -s "$ROOT/data/vpnmgr.db" ] || fail "数据库不在隔离目录里"
  LISTEN="$(lsof -nP -a -p "$PID" -iTCP -sTCP:LISTEN -Fn | sed -n 's/^n//p')"
  echo "[self-test] listen: $(printf '%s ' $LISTEN)"
  [ -n "$LISTEN" ] || fail "没有监听端口"
  printf '%s\n' "$LISTEN" | grep -qv '^127\.0\.0\.1:' && fail "有不在 127.0.0.1 上的监听"
  COUNT="$(printf '%s' "$IMPORT" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).imported.length))')"
  [ "$COUNT" = 5 ] || fail "导入条数 ${COUNT}，应为 5"
  echo "[self-test] 通过"
  exit 0
fi

echo "[preview] 地址: $BASE/#/home"
echo "[preview] 核心进程: ${PID}（Ctrl-C 结束）"
wait "$PID"
