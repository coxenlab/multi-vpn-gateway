#!/usr/bin/env python3
"""从 Hillstone Secure Connect 官方 Linux 安装器(Qt Installer Framework 单文件 .run)里
抠出各组件的 7z 数据段并解包到目标目录。

安装器本体是 x86_64 ELF,它的 --dump-binary-data 需要完整 X/Qt 运行时才肯启动,
构建期不值得为此装 X;IFW 把每个组件按 7z 原样嵌在 ELF 尾部,直接扫 7z 魔数
(37 7A BC AF 27 1C)逐段试解即可,与安装器版本无关。
用法:extract-sc.py <installer.run> <dest_dir>
"""
import subprocess
import sys
from pathlib import Path

SIG = b"\x37\x7a\xbc\xaf\x27\x1c"


def main(installer: str, dest: str) -> int:
    data = Path(installer).read_bytes()
    dest_p = Path(dest)
    dest_p.mkdir(parents=True, exist_ok=True)
    offsets = []
    i = data.find(SIG)
    while i != -1:
        offsets.append(i)
        i = data.find(SIG, i + 1)
    if not offsets:
        print("no 7z signature found", file=sys.stderr)
        return 1
    ok = 0
    tmp = Path("/tmp/sc-part.7z")
    for off in offsets:
        tmp.write_bytes(data[off:])
        r = subprocess.run(["7z", "x", "-y", f"-o{dest_p}", str(tmp)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if r.returncode == 0:
            ok += 1
            print(f"extracted segment @{off}")
    tmp.unlink(missing_ok=True)
    print(f"{ok}/{len(offsets)} segments extracted → {dest_p}")
    need = [dest_p / "bin" / "HillstoneSecureConnect", dest_p / "bin" / "HillstoneSecureConnectService"]
    missing = [str(p) for p in need if not p.exists()]
    if missing:
        print("missing after extract: " + ", ".join(missing), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
