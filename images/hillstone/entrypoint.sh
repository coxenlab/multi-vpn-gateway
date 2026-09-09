#!/bin/sh
set -e

# --- /dev/net/tun 兜底(host 应 --device 传入;mknod 是安全网,需 MKNOD cap)---
if [ ! -c /dev/net/tun ]; then
    mkdir -p /dev/net
    mknod /dev/net/tun c 10 200 || echo "WARN: mknod /dev/net/tun 失败 — 客户端拿不到隧道(需 --cap-add NET_ADMIN,MKNOD)"
    chmod 600 /dev/net/tun 2>/dev/null || true
fi

# --- 无头 X ---
rm -f /tmp/.X0-lock
Xvfb :0 -screen 0 "${GEOMETRY:-1280x800x24}" -nolisten tcp &
for i in $(seq 1 50); do [ -e /tmp/.X11-unix/X0 ] && break; sleep 0.1; done

# --- 窗口管理器 ---
DISPLAY=:0 fluxbox >/tmp/fluxbox.log 2>&1 &

# --- VNC server 绑到 Xvfb 显示,密码取自 PASSWORD env(同 hagb/byo 合约)---
# RFB 5901 对齐 manager.ensure_novnc_bridge 的自愈目标(127.0.0.1:5901)。
PW="${PASSWORD:-changeme}"
x11vnc -storepasswd "$PW" /tmp/.vncpass >/dev/null 2>&1
x11vnc -display :0 -rfbport 5901 -rfbauth /tmp/.vncpass \
       -forever -shared -noxdamage -repeat -bg -o /tmp/x11vnc.log

# --- noVNC:websockify 同端口既服务静态站点又桥接 WS->VNC(8080)---
websockify --web /usr/share/novnc 0.0.0.0:8080 127.0.0.1:5901 >/tmp/websockify.log 2>&1 &

# --- Hillstone 守护进程(root;自 fork 后返回;日志在 /tmp/HillstoneSecureConnect/log/)---
# 官方 unit 是 Type=forking + Restart=always,这里用循环守着它:死了就重拉,GUI 会自动重连 IPC。
# pgrep 用 -f + 锚定全路径:进程 comm 被截成 15 字符("HillstoneSecure"),-x 匹配不到;
# 不锚定行首:qemu-user 转译时 cmdline 是 "/usr/bin/qemu-x86_64 <path> <argv0> …"(Rosetta 则保持原样)。
HS=/opt/HillstoneSecureConnect
( while :; do
    if ! pgrep -f "$HS/bin/HillstoneSecureConnectService" >/dev/null; then
        "$HS/bin/HillstoneSecureConnectService" >>/tmp/hs-service.log 2>&1 || true
    fi
    sleep 3
  done ) &

# --- Hillstone GUI(Qt5,自带 lib/plugins;-style fusion 同官方 .desktop)---
# 用户经 noVNC 在此窗口填网关/账号/验证码。配置落 /root/.config(卷,重建保留网关记忆)。
# LANG 必须设:GetCurrentSystemDiaplayLanguage() 对空 locale 直接段错误(实测 5.7.1)。
# GUI 自带单实例检查(ps -ef 找同名进程),循环里同样先 pgrep 再拉,避免自己撞自己。
export LANG=zh_CN.UTF-8 LANGUAGE=zh_CN LC_ALL=zh_CN.UTF-8 XDG_RUNTIME_DIR=/tmp/runtime-root
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
( sleep 1; while :; do
    if ! pgrep -f "$HS/bin/HillstoneSecureConnect " >/dev/null; then
        DISPLAY=:0 HOME=/root "$HS/bin/HillstoneSecureConnect" -style fusion >>/tmp/hs-gui.log 2>&1 || true
    fi
    sleep 2
  done ) &

# --- SOCKS5 占 0.0.0.0:1080(仅 docker 内网,命门 #4 永不 host-map):
#     microsocks 跟随 OS 路由表,自动走守护进程装起的 tun 路由(split 模式只覆盖下发网段)---
exec microsocks -i 0.0.0.0 -p 1080
