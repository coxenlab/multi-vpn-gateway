# vpnmgr/hillstone-desktop —— 预装 Hillstone Secure Connect(山石网科 SSL VPN / ZTNA)的桌面容器。
# byo 家族的专用成员。官方只出 x86_64 Linux 客户端,两段式:
#   HillstoneSecureConnectService(root 守护进程,管 tun/路由/resolv.conf,近乎静态链接)
#   + HillstoneSecureConnect(Qt5 GUI,自带全部 Qt 库,经 127.0.0.1 TCP JSON 与守护进程通信)。
# 不跑它的安装器(要 systemctl),构建期直接抠 7z 段落到 /opt/HillstoneSecureConnect(见 extract-sc.py)。
#
# 架构策略:镜像按宿主原生架构构建(Xvfb/x11vnc/noVNC/microsocks 全原生),只有 Hillstone 两个
# x86_64 二进制在 arm64 宿主上经 binfmt(Rosetta/qemu-user)转译;它们的系统侧依赖库经 Debian
# multiarch 装 :amd64 版本。实测整容器 --platform amd64 转译时 x11vnc 抓不到 Qt 窗口区域(黑屏),
# 且 VNC 栈也白白吃转译开销,故不整容器转译。
FROM debian:bookworm-slim
ARG TARGETARCH

ENV DEBIAN_FRONTEND=noninteractive \
    DISPLAY=:0 \
    GEOMETRY=1280x800x24

# arm64 宿主:启用 amd64 multiarch(apt 索引多一份,下面装 :amd64 库);amd64 宿主:库本就原生。
RUN if [ "$TARGETARCH" = "arm64" ]; then dpkg --add-architecture amd64; fi

# 拆小 RUN 层同 images/byo:不稳定 apt 源下逐层收敛。
RUN apt-get -o Acquire::Retries=8 update
# 桌面 + noVNC 栈(原生)
RUN apt-get -o Acquire::Retries=8 install -y --no-install-recommends xvfb fluxbox x11vnc
RUN apt-get -o Acquire::Retries=8 install -y --no-install-recommends novnc websockify microsocks
# tun/网络工具 + ensure_novnc_bridge 自愈用的 pgrep(procps)/ss(iproute2)+ 抠包工具 + 中文字体
RUN apt-get -o Acquire::Retries=8 install -y --no-install-recommends \
      iproute2 iptables iputils-ping net-tools dnsutils procps \
      curl ca-certificates xterm p7zip-full python3 fonts-wqy-microhei
# Hillstone 客户端(x86_64)的系统侧运行时:arm64 宿主装 :amd64 版(含 ld-linux-x86-64 与 libc),
# amd64 宿主装原生版。Qt 库本身随客户端自带,不装系统 Qt。
RUN A=""; if [ "$TARGETARCH" = "arm64" ]; then A=":amd64"; fi; \
    apt-get -o Acquire::Retries=8 install -y --no-install-recommends \
      libc6$A libstdc++6$A libgcc-s1$A \
      libglib2.0-0$A libgl1$A libegl1$A libfontconfig1$A libfreetype6$A libdbus-1-3$A \
      libx11-6$A libx11-xcb1$A libxext6$A libxrender1$A libxi6$A libsm6$A libice6$A \
      libxkbcommon0$A libxkbcommon-x11-0$A libxcb1$A libxcb-icccm4$A libxcb-image0$A \
      libxcb-keysyms1$A libxcb-render-util0$A libxcb-render0$A libxcb-shape0$A \
      libxcb-shm0$A libxcb-sync1$A libxcb-xfixes0$A libxcb-xinerama0$A libxcb-xkb1$A \
      libxcb-randr0$A libxcb-glx0$A libxcb-util1$A libxcursor1$A
RUN rm -rf /var/lib/apt/lists/*

# 兼容 manager.ensure_novnc_bridge 的自愈路径(它硬编码 /usr/local/share/novnc)
RUN mkdir -p /usr/local/share && ln -sf /usr/share/novnc /usr/local/share/novnc

# ---- Hillstone Secure Connect 客户端:官网下载口 302 到带日期的真实 URL;可用 --build-arg 换镜像/本地 http ----
ARG SC_URL="https://swupdate.hillstonenet.com:1338/sslvpn/download?os=linux"
COPY extract-sc.py /usr/local/bin/extract-sc.py
RUN curl -fsSL --retry 5 --retry-delay 3 -o /tmp/sc.run "$SC_URL" \
 && python3 /usr/local/bin/extract-sc.py /tmp/sc.run /opt/HillstoneSecureConnect \
 && rm -f /tmp/sc.run \
 && chmod +x /opt/HillstoneSecureConnect/bin/* \
 && ls -la /opt/HillstoneSecureConnect /opt/HillstoneSecureConnect/bin

# GUI 默认缩进系统托盘,fluxbox 没有托盘 → 主窗口永远看不见;关掉托盘让主窗口常驻。
RUN python3 - <<'PY'
import json
p="/opt/HillstoneSecureConnect/resources/AppCtrl.json"
d=json.load(open(p)); d["DISABLE_TRAY"]=True
json.dump(d, open(p,"w"))
PY

# GUI 的 Qt 运行时指向客户端自带的 lib/plugins(与官方 .desktop 一致)
ENV LD_LIBRARY_PATH=/opt/HillstoneSecureConnect/lib \
    QT_PLUGIN_PATH=/opt/HillstoneSecureConnect/plugins \
    QT_QPA_PLATFORM=xcb \
    QT_X11_NO_MITSHM=1

WORKDIR /root

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# 8080 = noVNC web+ws(manager 映射到 127.0.0.1 随机高位端口 / 桌面栈 SSH 转发)
# 1080 = SOCKS5(仅 docker 内网,永不 host-map — 命门 #4)
EXPOSE 8080 1080

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
