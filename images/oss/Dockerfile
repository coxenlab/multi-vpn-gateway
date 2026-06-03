FROM debian:bookworm-slim

# Acquire::Retries:对不稳定镜像源/代理的瞬时失败自动重试(通用硬化)
RUN apt-get -o Acquire::Retries=3 update \
 && apt-get -o Acquire::Retries=3 install -y --no-install-recommends \
      openconnect vpnc-scripts openfortivpn openvpn \
      wireguard-tools wireguard-go \
      ppp dante-server \
      iproute2 iptables iputils-ping ca-certificates dnsutils curl gettext-base \
 && rm -rf /var/lib/apt/lists/*

COPY sockd.conf.tmpl /etc/sockd.conf.tmpl
COPY entrypoint.sh   /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# 无内核模块时强制 wireguard-go 用户态
ENV WG_QUICK_USERSPACE_IMPLEMENTATION=wireguard-go WG_SUDO=1

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
