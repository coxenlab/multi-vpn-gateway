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
#
# 熔断上限:VPN 客户端进程由 manager 经 exec 注入,容器自发重启后没人重新拉起隧道;
# 远端一死,unless-stopped 会造出「等 120s → 退出 → 拉起」的无限循环(实测 331 轮)。
# Docker 自带 on-failure:N 对此无效(容器存活 >10s 计数即重置),所以在这里自己计数:
# 计数文件在容器可写层,跨 docker restart 存活、重建容器(UI「启动」)即清零;
# 连续 FAIL_MAX 次起不来 → 打明确日志后驻停(sleep infinity),不再空转。
FAIL_F=/var/tmp/vpnmgr-tunnel-fails
FAIL_MAX=5
i=0
while [ $i -lt 120 ]; do
  ip -4 addr show "$IFACE" 2>/dev/null | grep -q "inet " && break
  i=$((i+1)); sleep 1
done
if ! ip -4 addr show "$IFACE" 2>/dev/null | grep -q "inet "; then
  n=$(cat "$FAIL_F" 2>/dev/null || echo 0)
  n=$((n+1)); echo "$n" > "$FAIL_F"
  echo "tunnel iface $IFACE has no IPv4 after 120s (attempt $n/$FAIL_MAX)" >&2
  if [ "$n" -ge "$FAIL_MAX" ]; then
    echo "circuit open: $n consecutive boots without tunnel; parking (no more retries)." >&2
    echo "fix the VPN config / remote endpoint, then press 启动 (rebuild) to retry." >&2
    exec sleep infinity
  fi
  exit 3
fi
rm -f "$FAIL_F"   # 隧道就绪:清零连续失败计数

# 3. 渲染 dante egress 到隧道接口,exec 成 PID1(debian dante-server 的二进制是 danted)
sed "s/__VPN_IFACE__/$IFACE/" /etc/sockd.conf.tmpl > /etc/sockd.conf
exec danted -f /etc/sockd.conf
