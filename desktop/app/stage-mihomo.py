#!/usr/bin/env python3
"""Stage only the pinned host engine. No PATH fallback, installation, or replacement."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.request

SOURCE = Path(__file__).resolve().parents[2] / 'app/mihomo-source.json'


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def stage(output, spec, archive=None, binary=None):
    require(not output.is_symlink(), 'mihomo 输出不能是链接')
    if output.exists():
        require(output.is_file() and digest(output) == spec['binary_sha256'], '已有 mihomo 与锁定版本不同，请先移走旧暂存文件')
        require(os.access(output, os.X_OK), '已有 mihomo 缺少执行权限')
        return False
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.mihomo-source-', dir=output.parent) as temporary:
        work = Path(temporary)
        if binary is None:
            if archive is None:
                archive = work / 'download.gz'
                with urllib.request.urlopen(spec['url'], timeout=45) as response, archive.open('wb') as stream:
                    size = 0
                    for block in iter(lambda: response.read(1024 * 1024), b''):
                        size += len(block)
                        require(size <= spec['size'], 'mihomo 下载大小超过锁定值')
                        stream.write(block)
            require(archive.stat().st_size == spec['size'] and digest(archive) == spec['sha256'], 'mihomo 下载归档校验失败')
        target = work / 'mihomo'
        with (binary.open('rb') if binary is not None else gzip.open(archive, 'rb')) as source, target.open('wb') as stream:
            size = 0
            for block in iter(lambda: source.read(1024 * 1024), b''):
                size += len(block)
                require(size <= spec['binary_size'], 'mihomo 展开大小超过锁定值')
                stream.write(block)
            stream.flush(); os.fsync(stream.fileno())
        require(size == spec['binary_size'] and digest(target) == spec['binary_sha256'], 'mihomo 二进制与锁定版本不同')
        target.chmod(0o755)
        # The upstream Go binary is already ad-hoc signed. Preserve the checked bytes.
        subprocess.run(['/usr/bin/lipo', str(target), '-verify_arch', 'arm64'], check=True)
        subprocess.run(['/usr/bin/codesign', '--verify', '--strict', str(target)], check=True)
        os.link(target, output)  # Atomic and no-clobber, including a racing writer.
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    inputs = parser.add_mutually_exclusive_group()
    inputs.add_argument('--archive', type=Path, help='离线使用已下载的官方 gzip，仍核对大小与 SHA256')
    inputs.add_argument('--binary', type=Path, help='只接受与官方解压文件完全一致的二进制')
    args = parser.parse_args()
    lock = json.loads(SOURCE.read_text())
    require(lock['schema'] == 1, '未知 mihomo 来源清单版本')
    changed = stage(args.output, lock['darwin_arm64'], args.archive, args.binary)
    print('mihomo ' + lock['version'] + (' 已暂存并核对' if changed else ' 已存在且校验一致'))


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError, ValueError, KeyError, EOFError, subprocess.CalledProcessError) as error:
        raise SystemExit('mihomo 暂存失败: ' + str(error))
