#!/bin/sh
# 把内置镜像 `docker save | gzip` 成 tarball 进 bundled-images/,供 tauri.conf.json 的 resources
# 打进 .app 的 Contents/Resources/images;首启 infra::ensure_bundled_images 经 bollard docker-load 进 VM。
# bundled-images/ 已 gitignore(不入库,构建前跑本脚本生成)。
#
# 当前只内置 vpnmgr/oss-vpn(无头 CLI 家族共用,~291MB→gz ~120MB;自建、registry 拉不到,只能 load)。
# byo-desktop(1.15GB,长尾兜底)暂不内置 —— 用户首次用 byo 通道时再按需取(待分发 host 落地)。
# EC/aTrust(hagb)是 registry 可拉镜像,本就不打包。
set -eu
cd "$(dirname "$0")"
: "${DOCKER_HOST:=unix://$HOME/.colima/vpnmgr/docker.sock}"   # 默认从 app 专属 colima VM 取
export DOCKER_HOST
DOCKER="${DOCKER:-docker}"
mkdir -p bundled-images
MIHOMO_IMAGE=$(python3 -c 'import json; print(json.load(open("../../app/mihomo-source.json"))["image"])')
MIHOMO_IDS=$(python3 -c 'import json; s=json.load(open("../../app/mihomo-source.json")); p=s["platforms"]["arm64"]; print(" ".join([p["config"], p["manifest"], s["index"]]))')
# oss-vpn(自建,registry 拉不到)+ mihomo(分流内核;内置后第 05 步离线,免赖镜像源)
for PAIR in "vpnmgr/oss-vpn:latest oss-vpn.tar.gz" "$MIHOMO_IMAGE mihomo.tar.gz"; do
  IMG=${PAIR% *}; OUT=${PAIR#* }
  "$DOCKER" image inspect "$IMG" >/dev/null 2>&1 \
    || { echo "context $DOCKER_HOST 里没有 $IMG;先 build/load 它再跑本脚本"; exit 1; }
  if [ "$OUT" = mihomo.tar.gz ]; then
    ACTUAL_ID=$("$DOCKER" image inspect --format '{{.Id}}' "$IMG")
    case " $MIHOMO_IDS " in
      *" $ACTUAL_ID "*) ;;
      *) echo 'mihomo 实际镜像与锁定来源不同，拒绝导出。' >&2; exit 1 ;;
    esac
    [ "$("$DOCKER" image inspect --format '{{.Architecture}}' "$IMG")" = arm64 ] \
      || { echo 'mihomo 镜像不是 arm64，拒绝导出。' >&2; exit 1; }
  fi
  echo "docker save $IMG → bundled-images/$OUT  (DOCKER_HOST=$DOCKER_HOST)"
  "$DOCKER" save "$IMG" | gzip > "bundled-images/$OUT"
  ls -lh "bundled-images/$OUT" | awk '{print "  staged:", $5, $NF}'
done
