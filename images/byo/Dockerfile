ARG BASE_IMAGE=debian:bookworm-slim

# Rosetta compatibility for translated x86_64 clients (see rosetta-eintr.c).
# Only arm64 hosts translate x86_64; elsewhere this stage outputs nothing.
FROM ${BASE_IMAGE} AS rosetta
ARG DEBIAN_MIRROR=http://deb.debian.org/debian
ARG DEBIAN_SECURITY_MIRROR=http://deb.debian.org/debian-security
COPY rosetta-eintr.c /src/
RUN mkdir -p /out \
 && if [ "$(dpkg --print-architecture)" = arm64 ]; then \
      sed -i "s|http://deb.debian.org/debian-security|${DEBIAN_SECURITY_MIRROR}|g; s|http://deb.debian.org/debian |${DEBIAN_MIRROR} |g; s|http://deb.debian.org/debian$|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
      && apt-get -o Acquire::Retries=3 update \
      && apt-get -o Acquire::Retries=3 install -y --no-install-recommends gcc libc6-dev gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
      && mkdir -p /out/usr/lib/x86_64-linux-gnu /out/usr/lib/aarch64-linux-gnu /out/etc \
      && x86_64-linux-gnu-gcc -O2 -Wall -Wextra -Werror -shared -fPIC -o /out/usr/lib/x86_64-linux-gnu/libtongdao-rosetta.so /src/rosetta-eintr.c \
      && : > /src/empty.c && gcc -O2 -shared -fPIC -o /out/usr/lib/aarch64-linux-gnu/libtongdao-rosetta.so /src/empty.c \
      && printf '/usr/$LIB/libtongdao-rosetta.so\n' > /out/etc/ld.so.preload; fi

FROM ${BASE_IMAGE}
ARG DEBIAN_MIRROR=http://deb.debian.org/debian
ARG DEBIAN_SECURITY_MIRROR=http://deb.debian.org/debian-security

ENV DEBIAN_FRONTEND=noninteractive DISPLAY=:0 GEOMETRY=1280x800x24 container=docker
# One vendor-neutral Linux desktop. Official package maintainer scripts run
# under real systemd; no extracted vendor binaries or substitute service shims.
# polkit: domestic (UOS/Kylin-targeted) packages often launch via pkexec and ship
# their own policy; gxmessage renders UTF-8 failure dialogs (xmessage cannot);
# hsetroot keeps fluxbox from popping a "no wallpaper setter" dialog.
RUN sed -i "s|http://deb.debian.org/debian-security|${DEBIAN_SECURITY_MIRROR}|g; s|http://deb.debian.org/debian |${DEBIAN_MIRROR} |g; s|http://deb.debian.org/debian$|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
 && apt-get -o Acquire::Retries=3 update \
 && apt-get -o Acquire::Retries=3 install -y --no-install-recommends \
      systemd systemd-sysv dbus dbus-x11 xvfb fluxbox x11vnc novnc websockify \
      microsocks iproute2 iptables iputils-ping net-tools dnsutils procps \
      wget curl ca-certificates xterm pcmanfm libgtk-3-0 libwebkit2gtk-4.1-0 \
      desktop-file-utils libglib2.0-bin python3 file sudo wmctrl x11-utils \
      gnome-keyring libpam-gnome-keyring polkitd pkexec gxmessage libnotify4 hsetroot \
 && if [ "$(dpkg --print-architecture)" = arm64 ]; then \
      dpkg --add-architecture amd64; apt-get -o Acquire::Retries=3 update; \
      apt-get -o Acquire::Retries=3 install -y --no-install-recommends \
        libgtk-3-0:amd64 libwebkit2gtk-4.1-0:amd64; fi \
 && apt-get clean
# Vendor clients that bundle their own Qt5 (UniVPN) still need the system xcb/GL
# libraries for the x86_64 build, translated on arm64 hosts.
RUN qt=""; [ "$(dpkg --print-architecture)" = arm64 ] && qt=":amd64"; \
    apt-get -o Acquire::Retries=3 install -y --no-install-recommends \
      $(for lib in libgl1 libegl1 libx11-xcb1 libxkbcommon-x11-0 libxcb-glx0 libxcb-icccm4 libxcb-image0 \
          libxcb-keysyms1 libxcb-randr0 libxcb-render-util0 libxcb-shape0 libxcb-sync1 libxcb-xfixes0 \
          libxcb-xkb1 libxcb-xinerama0 libfontconfig1 libudev1 libzstd1; do printf '%s%s ' "$lib" "$qt"; done) \
 && apt-get clean

RUN apt-get -o Acquire::Retries=3 install -y --no-install-recommends fonts-wqy-microhei \
 && apt-get clean \
 && systemctl mask dev-hugepages.mount sys-fs-fuse-connections.mount danted.service

RUN useradd --create-home --uid 1000 --shell /bin/bash desktop \
 && useradd --system --no-create-home --shell /usr/sbin/nologin tongdao-socks \
 && mkdir -p /usr/local/share /root/installers /etc/systemd/system/multi-user.target.wants \
 && ln -sfn /usr/share/novnc /usr/local/share/novnc \
 && truncate -s 0 /etc/machine-id
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
COPY tongdao-desktop tongdao-install tongdao-egress tongdao-root-terminal /usr/local/bin/
COPY units/ /etc/systemd/system/
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/tongdao-desktop /usr/local/bin/tongdao-install /usr/local/bin/tongdao-egress /usr/local/bin/tongdao-root-terminal \
 && for service in tongdao-display tongdao-desktop tongdao-vnc tongdao-websockify tongdao-socks tongdao-egress; do \
      ln -sfn ../${service}.service /etc/systemd/system/multi-user.target.wants/${service}.service; done
# The channel desktop is a single-purpose box: give its user full root without
# prompts (sudo, polkit) and make every terminal a root terminal, so vendor
# installers and clients never stall on permissions. The session itself stays
# non-root because Electron/Chromium clients refuse to start as root.
RUN printf 'desktop ALL=(ALL:ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/tongdao-desktop \
 && chmod 0440 /etc/sudoers.d/tongdao-desktop && visudo -cf /etc/sudoers.d/tongdao-desktop \
 && mkdir -p /etc/polkit-1/rules.d \
 && printf 'polkit.addRule(function(action, subject) {\n  if (subject.user == "desktop") return polkit.Result.YES;\n});\n' > /etc/polkit-1/rules.d/49-tongdao-desktop.rules \
 && update-alternatives --install /usr/bin/x-terminal-emulator x-terminal-emulator /usr/local/bin/tongdao-root-terminal 100 \
 && update-alternatives --set x-terminal-emulator /usr/local/bin/tongdao-root-terminal
# Native programs load an empty library; translated x86_64 ones the EINTR fix.
COPY --from=rosetta /out/ /
WORKDIR /root
EXPOSE 8080 1080
STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
