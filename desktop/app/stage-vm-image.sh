#!/bin/sh
# 内置 colima VM 磁盘镜像(ubuntu minimal cloudimg,~317MB):首启免连 GitHub。
# colima 下载缓存的 key = sha256(下载 URL),文件放 ~/Library/Caches/colima/caches/<key>;
# 原生 core 在显式连接时把 bundle 里这份预置进对方机器的同一路径,
# colima start 命中缓存即跳过下载 —— 国内网络连不上 GitHub 也能完成首启。
# URL 与 Colima 版本共用 vm-image-source.json；版本不匹配时拒绝暂存。
# vm-image/ 已 gitignore(构建前跑本脚本从本机缓存取)。
set -eu
cd "$(dirname "$0")"
exec python3 - <<'PY'
import hashlib
import json
from pathlib import Path
import shutil

source = json.loads(Path('vm-image-source.json').read_text())
lock = json.loads(Path('runtime-sources.lock.json').read_text())
if source['colima_version'] != lock['colima']['version']:
    raise SystemExit('VM 镜像来源与锁定 Colima 版本不一致')
key = hashlib.sha256(source['url'].encode()).hexdigest()
cached = Path.home() / 'Library/Caches/colima/caches' / key
if not cached.is_file():
    raise SystemExit('本机缺少对应的 VM 镜像缓存，请先准备运行环境资源')
destination = Path('vm-image')
destination.mkdir(exist_ok=True)
shutil.copy2(cached, destination / key)
print('staged vm-image: ' + key)
PY
