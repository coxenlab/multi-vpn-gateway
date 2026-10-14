ARG BASE_IMAGE=debian:bookworm-slim
FROM ${BASE_IMAGE}
ARG DEBIAN_MIRROR=http://deb.debian.org/debian
ARG DEBIAN_SECURITY_MIRROR=http://deb.debian.org/debian-security

ENV DEBIAN_FRONTEND=noninteractive DISPLAY=:0 GEOMETRY=1280x800x24 container=docker
# One vendor-neutral Linux desktop. Official package maintainer scripts run
# under real systemd; no extracted vendor binaries or substitute service shims.
RUN sed -i "s|http://deb.debian.org/debian-security|${DEBIAN_SECURITY_MIRROR}|g; s|http://deb.debian.org/debian |${DEBIAN_MIRROR} |g; s|http://deb.debian.org/debian$|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
 && apt-get -o Acquire::Retries=3 update \
 && apt-get -o Acquire::Retries=3 install -y --no-install-recommends \
      systemd systemd-sysv dbus dbus-x11 xvfb fluxbox x11vnc novnc websockify \
      microsocks iproute2 iptables iputils-ping net-tools dnsutils procps \
      wget curl ca-certificates xterm pcmanfm libgtk-3-0 libwebkit2gtk-4.1-0 \
      desktop-file-utils libglib2.0-bin python3 file sudo wmctrl x11-utils \
      gnome-keyring libpam-gnome-keyring \
 && if [ "$(dpkg --print-architecture)" = arm64 ]; then \
      dpkg --add-architecture amd64; apt-get -o Acquire::Retries=3 update; \
      apt-get -o Acquire::Retries=3 install -y --no-install-recommends \
        libgtk-3-0:amd64 libwebkit2gtk-4.1-0:amd64; fi \
 && apt-get clean

RUN apt-get -o Acquire::Retries=3 install -y --no-install-recommends fonts-wqy-microhei \
 && apt-get clean \
 && systemctl mask dev-hugepages.mount sys-fs-fuse-connections.mount danted.service

RUN useradd --create-home --uid 1000 --shell /bin/bash desktop \
 && mkdir -p /usr/local/share /root/installers /etc/systemd/system/multi-user.target.wants \
 && ln -sfn /usr/share/novnc /usr/local/share/novnc \
 && truncate -s 0 /etc/machine-id
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
COPY tongdao-desktop tongdao-install /usr/local/bin/
COPY units/ /etc/systemd/system/
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/tongdao-desktop /usr/local/bin/tongdao-install \
 && for service in tongdao-display tongdao-desktop tongdao-vnc tongdao-websockify tongdao-socks; do \
      ln -sfn ../${service}.service /etc/systemd/system/multi-user.target.wants/${service}.service; done
WORKDIR /root
EXPOSE 8080 1080
STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
