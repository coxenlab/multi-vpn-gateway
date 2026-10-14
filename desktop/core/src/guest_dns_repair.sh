set -eu
root=/
resolver="$root/etc/resolv.conf"
# Never overwrite a working resolver or an unfamiliar symlink/file.
[ ! -e "$resolver" ] || exit 0
[ "$(readlink "$resolver" 2>/dev/null || true)" = '../run/systemd/resolve/stub-resolv.conf' ] || exit 0
network="$root/run/systemd/network/10-netplan-eth0.network"
# Consume the generated networkd setting, not the private DHCP lease format.
dns=$(awk -F= '/^DNS=/ {print $2}' "$network")
printf '%s\n' "$dns" | awk -F. '
    NF != 4 {exit 1}
    {for (i=1;i<=4;i++) if ($i !~ /^[0-9]+$/ || length($i)>3 || $i>255) exit 1}
    END {if (NR != 1) exit 1}'
backup="$resolver.tongdao-original"
if [ ! -e "$backup" ] && [ ! -L "$backup" ]; then
    cp -P "$resolver" "$backup"
fi
tmp=$(mktemp "$root/etc/resolv.conf.tongdao.XXXXXX")
trap 'rm -f "$tmp"' EXIT HUP INT TERM
printf 'nameserver %s\n' "$dns" > "$tmp"
chmod 644 "$tmp"
mv -f "$tmp" "$resolver"
