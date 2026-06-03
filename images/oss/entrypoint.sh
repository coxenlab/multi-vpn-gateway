#!/bin/sh
set -e
VPN_PROTOCOL="${VPN_PROTOCOL:?need VPN_PROTOCOL}"

# 0. 确保 /dev/net/tun 存在(device-map 兜底)
[ -c /dev/net/tun ] || { mkdir -p /dev/net; mknod /dev/net/tun c 10 200; chmod 600 /dev/net/tun; }

# 0b. openfortivpn 用 pppd,需 /dev/ppp(major 108);MKNOD 权限 + 设备放行由 manifest 给到
[ "$VPN_PROTOCOL" = openfortivpn ] && { [ -c /dev/ppp ] || mknod /dev/ppp c 108 0; }

# 1. 选隧道接口名(具体客户端进程由 manager.oss_connect 经 exec_run 注入凭据后启动)
case "$VPN_PROTOCOL" in
  anyconnect|gp|fortinet|nc|pulse|openvpn) IFACE=tun0 ;;
  openfortivpn)                            IFACE=ppp0 ;;
  wireguard)                               IFACE=wg0  ;;
  *) echo "unknown VPN_PROTOCOL=$VPN_PROTOCOL" >&2; exit 2 ;;
esac

# 2. 等隧道接口拿到 IPv4 地址(最多 120s)。注意:ppp0 链路在 IP 协商完成前就出现,
#    必须等地址就绪再起 danted,否则 danted 解析 external 接口拿不到可绑地址即退→容器重启循环。
i=0
while [ $i -lt 120 ]; do
  ip -4 addr show "$IFACE" 2>/dev/null | grep -q "inet " && break
  i=$((i+1)); sleep 1
done
ip -4 addr show "$IFACE" 2>/dev/null | grep -q "inet " || { echo "tunnel iface $IFACE has no IPv4 after 120s" >&2; exit 3; }

# 3. 渲染 dante egress 到隧道接口,exec 成 PID1(debian dante-server 的二进制是 danted)
sed "s/__VPN_IFACE__/$IFACE/" /etc/sockd.conf.tmpl > /etc/sockd.conf
exec danted -f /etc/sockd.conf
