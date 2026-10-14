#!/bin/sh
set -eu
# Docker supplies a PRIVATE cgroup namespace. AppArmor permits only this
# remount; never bind the VM's cgroup tree or use privileged/host namespaces.
test "$(cat /proc/1/cgroup)" = '0::/'
mount -o remount,rw /sys/fs/cgroup
mkdir -p /root/installers /root/.local/share/tongdao/apps /run/tongdao
mkdir -p /home/desktop/.local/share/tongdao/apps /home/desktop/Downloads
chown -R desktop:desktop /home/desktop
chmod 700 /run/tongdao
# Keep credentials in a private file, not argv or process environment passed
# to the desktop/session or the VPN installer.
(umask 077; printf '%s\n' "${PASSWORD:?missing desktop password}" > /run/tongdao/vnc-password)
unset PASSWORD
# Generic compatibility for translated Go programs on Apple Silicon.
# This does not replace or edit any vendor unit or installation script.
if [ "$(uname -m)" = aarch64 ]; then
    mkdir -p /etc/systemd/system.conf.d
    printf '[Manager]\nDefaultEnvironment=GODEBUG=cpu.avx2=off\n' > /etc/systemd/system.conf.d/tongdao-translation.conf
fi
exec /sbin/init
