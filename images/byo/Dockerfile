FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    DISPLAY=:0 \
    GEOMETRY=1280x800x24

# 拆成多个小 RUN 层:不稳定的 apt 镜像/代理下,buildkit 缓存已成功的层,
# 重试只重跑失败层、逐层收敛(单层大 apt 任一包耗尽重试即整层失败、且不缓存)。
# 一次 update(索引随层文件系统下传给后续 install 层),最后统一清 lists。
RUN apt-get -o Acquire::Retries=8 update
# 桌面 + noVNC 栈
RUN apt-get -o Acquire::Retries=8 install -y --no-install-recommends xvfb fluxbox x11vnc
# noVNC 静态站 + WS 桥 + SOCKS5(bookworm 一等包)
RUN apt-get -o Acquire::Retries=8 install -y --no-install-recommends novnc websockify microsocks
# tun/网络工具 + ensure_novnc_bridge 自愈用的 pgrep(procps)/ss(iproute2)
RUN apt-get -o Acquire::Retries=8 install -y --no-install-recommends \
      iproute2 iptables iputils-ping net-tools dnsutils procps
# 下载/安装客户端 + 终端 + 文件管理器 + GUI 客户端常见运行时依赖
RUN apt-get -o Acquire::Retries=8 install -y --no-install-recommends \
      wget curl ca-certificates xterm pcmanfm libgtk-3-0 libnss3 libxss1
RUN rm -rf /var/lib/apt/lists/*

# 兼容 manager.ensure_novnc_bridge 的自愈路径(它硬编码 /usr/local/share/novnc)
RUN mkdir -p /usr/local/share && ln -sf /usr/share/novnc /usr/local/share/novnc

# 上传安装器落点(put_archive 的 dest_dir 须已存在)
RUN mkdir -p /root/installers
WORKDIR /root

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# 8080 = noVNC web+ws(manager 映射到 127.0.0.1 随机高位端口)
# 1080 = SOCKS5(仅 docker 内网,永不 host-map — 命门 #4)
EXPOSE 8080 1080

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
