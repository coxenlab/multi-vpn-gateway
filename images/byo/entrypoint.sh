#!/bin/sh
set -e

# --- /dev/net/tun 兜底(host 应 --device 传入;mknod 是安全网,需 MKNOD cap)---
if [ ! -c /dev/net/tun ]; then
    mkdir -p /dev/net
    mknod /dev/net/tun c 10 200 || echo "WARN: mknod /dev/net/tun 失败 — VPN 客户端可能拿不到隧道(需 --cap-add NET_ADMIN,MKNOD)"
    chmod 600 /dev/net/tun 2>/dev/null || true
fi

# --- 无头 X ---
rm -f /tmp/.X0-lock
Xvfb :0 -screen 0 "${GEOMETRY:-1280x800x24}" -nolisten tcp &
for i in $(seq 1 50); do [ -e /tmp/.X11-unix/X0 ] && break; sleep 0.1; done

# --- 窗口管理器 ---
DISPLAY=:0 fluxbox >/tmp/fluxbox.log 2>&1 &

# --- VNC server 绑到 Xvfb 显示,密码取自 PASSWORD env(同 hagb 合约)---
# RFB 5901 对齐 manager.ensure_novnc_bridge 的自愈目标(127.0.0.1:5901)。
PW="${PASSWORD:-changeme}"
x11vnc -storepasswd "$PW" /tmp/.vncpass >/dev/null 2>&1
x11vnc -display :0 -rfbport 5901 -rfbauth /tmp/.vncpass \
       -forever -shared -noxdamage -repeat -bg -o /tmp/x11vnc.log

# --- noVNC:websockify 同端口既服务静态站点又桥接 WS->VNC(8080)---
# 与 vnc.html?path=websockify/&password=PW 兼容(websockify 升级任意路径)。
websockify --web /usr/share/novnc 0.0.0.0:8080 127.0.0.1:5901 >/tmp/websockify.log 2>&1 &

# --- SOCKS5 占 0.0.0.0:1080(仅 docker 内网,命门 #4 永不 host-map):
#     microsocks 跟随 OS 路由表,自动走用户客户端装起的 tun 路由(无 egress pin,
#     因为任意厂商客户端的隧道接口名事先未知)---
exec microsocks -i 0.0.0.0 -p 1080
