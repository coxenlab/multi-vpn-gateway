#!/bin/bash
# 仅在 SOCKS 探活失败后运行；参数不含 URL 路径、账号或密码。
# 无常驻 exec 守护:每次从当前容器读取 PID1 代次,自动重启/同 IP 重登均可重试。
set -eu
host=$1
port=$2
scheme=$3
tun=$4
case "$host" in ''|*[!a-zA-Z0-9.-]*) exit 2;; esac
case "$port" in ''|*[!0-9]*) exit 2;; esac
case "$scheme" in http|https) ;; *) exit 2;; esac
case "$tun" in tun0|utun7) ;; *) exit 2;; esac
command -v curl >/dev/null
command -v flock >/dev/null
test -s /run/danted.conf
exec 9>/run/vpnmgr-dns-recovery.lock
flock -n 9 || exit 0

# 地址只用于排除尚未登录的占位接口,不作为登录成功或重登判据。
ip -4 -o addr show dev "$tun" 2>/dev/null | awk '$4 !~ /^10\.0\.0\.1\// {ok=1} END {exit !ok}' || exit 0
resolved=$(timeout 3 getent ahostsv4 "$host" | awk 'NR==1{print $1}')
test -n "$resolved" || exit 0
generation=$(awk '{print $22}' /proc/1/stat)
now=$(date +%s)
last_generation=0
last_time=0
if test -f /run/vpnmgr-dns-recovery.last; then
    read -r last_generation last_time </run/vpnmgr-dns-recovery.last || true
fi
if test "$last_generation" = "$generation" && test "$((now-last_time))" -lt 20; then
    exit 0
fi

listening() { ss -H -ltn 'sport = :1080' | grep -q .; }
if listening; then
    # 对照同一代理:域名失败,绕过 Dante DNS 的字面 IP 成功,才允许清缓存。
    # 仅测根路径建立 HTTP 通路;真实 probe_url 的成功仍由调用者重试确认。
    url="$scheme://$host:$port/"
    domain_code=$(curl -ks -o /dev/null -w '%{http_code}' --max-time 3 \
        --socks5-hostname 127.0.0.1:1080 "$url" || true)
    test "$domain_code" = 000 || exit 0
    ip_code=$(curl -ks -o /dev/null -w '%{http_code}' --max-time 3 \
        --socks5-hostname 127.0.0.1:1080 --connect-to "$host:$port:$resolved:$port" "$url" || true)
    test "$ip_code" != 000 && test -n "$ip_code" || exit 0
fi

# 先记尝试时间,启动失败也不会每拍杀代理；同一容器代次最多每 20 秒一次。
printf '%s %s\n' "$generation" "$now" >/run/vpnmgr-dns-recovery.last
pkill -TERM -x danted || true
for _ in {1..15}; do
    listening || break
    sleep 0.2
done
if listening; then
    echo VPNMGR_DNS_OLD_LISTENER_BUSY
    exit 1
fi
if ! /usr/sbin/danted -D -f /run/danted.conf; then
    echo VPNMGR_DNS_START_FAILED
    exit 1
fi
for _ in {1..20}; do
    if listening; then
        echo VPNMGR_DNS_RECOVERED
        exit 0
    fi
    sleep 0.2
done
echo VPNMGR_DNS_NOT_READY
exit 1
