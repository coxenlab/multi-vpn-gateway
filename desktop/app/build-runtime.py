#!/usr/bin/env python3
"""固定来源构建 macOS arm64 runtime；候选补丁必须显式选择，不替换已安装 runtime。"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request

HERE = Path(__file__).resolve().parent


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def download(cache, spec):
    path = cache / spec['sha256']
    if path.exists():
        require(digest(path) == spec['sha256'], '下载缓存校验失败，请移走损坏文件后重试')
        return path
    print('下载 ' + spec['url'], flush=True)
    temporary = path.with_suffix('.part')
    try:
        with urllib.request.urlopen(spec['url'], timeout=45) as response, temporary.open('wb') as output:
            total = 0
            for block in iter(lambda: response.read(1024 * 1024), b''):
                total += len(block)
                require(total <= 512 * 1024 * 1024, '下载大小超过限制')
                output.write(block)
        require(digest(temporary) == spec['sha256'], '下载内容 SHA256 不匹配')
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)
    return path


def extract(archive_path, destination):
    """固定归档仍校验路径/类型；仅允许归档根目录内的相对软链接。"""
    with tarfile.open(archive_path) as archive:
        entries = []
        total = 0
        prefix = None
        for entry in archive:
            parts = Path(entry.name).parts
            require(parts and not Path(entry.name).is_absolute() and '..' not in parts, '归档路径无效')
            prefix = prefix or parts[0]
            require(parts[0] == prefix, '归档必须有单一根目录')
            if len(parts) == 1:
                continue
            entry.name = str(Path(*parts[1:]))
            require(entry.isfile() or entry.isdir() or entry.issym(), '归档文件类型无效')
            if entry.issym():
                target = (destination / entry.name).parent / entry.linkname
                require(not Path(entry.linkname).is_absolute() and target.resolve().is_relative_to(destination.resolve()), '归档链接越界')
            entry.mode &= 0o777
            total += entry.size
            require(total <= 1024 * 1024 * 1024, '归档展开大小超过限制')
            entries.append(entry)
        for entry in entries:
            archive.extract(entry, destination)


def build(cache, lock, variant, build_id, environment, toolchain):
    output = cache / build_id
    if output.exists():
        verify(output, build_id)
        return output
    with tempfile.TemporaryDirectory(prefix='runtime-build-', dir=cache) as temporary:
        work = Path(temporary)
        source = work / 'lima'
        source.mkdir()
        extract(download(cache, lock['lima']), source)
        original = {name: digest(source / name) for name in ('go.mod', 'go.sum')}
        version = lock['lima']['version']
        if variant == 'gvisor698':
            patch = lock['gvisor698']
            patch_path = HERE / patch['patch']
            require(digest(patch_path) == patch['patch_sha256'], '补丁 SHA256 不匹配')
            subprocess.run(['go', 'mod', 'vendor'], cwd=source, env=environment, check=True)
            vendor = source / 'vendor/github.com/containers/gvisor-tap-vsock'
            for entry in patch['files']:
                require(digest(vendor / entry['path']) == entry['before'], '补丁原文件版本不匹配')
            subprocess.run(['patch', '--fuzz=0', '-p1', '-i', str(patch_path)], cwd=vendor, check=True)
            for entry in patch['files']:
                require(digest(vendor / entry['path']) == entry['after'], '补丁结果校验失败')
            version = patch['lima_version']
            environment = dict(environment, GOFLAGS='-mod=vendor -trimpath -buildvcs=false')
        tags = 'vpnmgr_lima_2_1_2' + (',vpnmgr_gvisor_698_8b4db4a' if variant == 'gvisor698' else '')
        subprocess.run(['make', 'VERSION=' + version, 'GO_BUILDTAGS=' + tags, 'limactl', 'native-guestagent', 'templates'], cwd=source, env=environment, check=True)
        require(all(digest(source / name) == value for name, value in original.items()), '构建改变了依赖锁定文件')
        result = work / 'runtime'
        (result / 'bin').mkdir(parents=True)
        (result / 'share/lima').mkdir(parents=True)
        for name in ('limactl', 'lima'):
            shutil.copy2(source / '_output/bin' / name, result / 'bin' / name)
        guest = source / '_output/share/lima/lima-guestagent.Linux-aarch64.gz'
        require(guest.is_file(), '缺少配套 Linux guestagent')
        shutil.copy2(guest, result / 'share/lima' / guest.name)
        shutil.copytree(source / '_output/share/lima/templates', result / 'share/lima/templates')
        shutil.copy2(download(cache, lock['colima']), result / 'bin/colima')
        docker = work / 'docker'
        docker.mkdir()
        extract(download(cache, lock['docker']), docker)
        shutil.copy2(docker / 'docker', result / 'bin/docker')
        for name in ('limactl', 'lima', 'colima', 'docker'):
            (result / 'bin' / name).chmod(0o755)
        for name in ('limactl', 'colima'):
            subprocess.run(['codesign', '--force', '--sign', '-', '--entitlements', str(HERE / 'entitlements.plist'),
                            '--options', 'runtime', str(result / 'bin' / name)], check=True)
        info = subprocess.check_output(['go', 'version', '-m', str(result / 'bin/limactl')], text=True)
        require(subprocess.check_output([str(result / 'bin/limactl'), '--version'], text=True).strip() == 'limactl version ' + version.removeprefix('v'), 'Lima 构建版本不匹配')
        require('\t-tags=' + tags in info, 'Lima 构建标记缺失')
        require('\tgithub.com/containers/gvisor-tap-vsock\tv0.8.9\t' in info, 'gvisor 基础版本不匹配')
        require('colima version ' + lock['colima']['version'] in subprocess.check_output([str(result / 'bin/colima'), 'version'], text=True), 'Colima 版本不匹配')
        require('Docker version ' + lock['docker']['version'] + ',' in subprocess.check_output([str(result / 'bin/docker'), '--version'], text=True), 'Docker CLI 版本不匹配')
        (result / 'buildinfo.txt').write_text(info)
        manifest = {'schema': 1, 'build_id': build_id, 'variant': variant, 'lima_version': version,
                    'toolchain': toolchain,
                    'sources': lock, 'files': {str(p.relative_to(result)): digest(p) for p in sorted(result.rglob('*')) if p.is_file()}}
        (result / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        result.rename(output)
    verify(output, build_id)
    return output


def verify(directory, build_id):
    manifest = json.loads((directory / 'manifest.json').read_text())
    require(manifest['build_id'] == build_id, 'runtime 构建身份不匹配')
    for name in ('bin/limactl', 'bin/lima', 'bin/colima', 'bin/docker', 'share/lima/lima-guestagent.Linux-aarch64.gz'):
        require(name in manifest['files'], 'runtime 必需文件未记录: ' + name)
    for name, expected in manifest['files'].items():
        path = directory / name
        require(path.resolve().is_relative_to(directory.resolve()) and not path.is_symlink(), 'runtime 文件路径无效')
        require(path.is_file() and digest(path) == expected, 'runtime 文件校验失败: ' + name)
    for name in ('limactl', 'lima', 'colima', 'docker'):
        require(os.access(directory / 'bin' / name, os.X_OK), 'runtime 执行权限缺失: ' + name)
    for name in ('limactl', 'colima'):
        subprocess.run(['codesign', '--verify', '--strict', str(directory / 'bin' / name)], check=True)


def stage(source, destination, build_id):
    require(not destination.is_symlink() and destination.parent.is_dir(), '暂存目标无效')
    if destination.exists() and any(destination.iterdir()):
        require(destination == HERE / 'runtime' or (destination / 'manifest.json').is_file(), '拒绝覆盖非 runtime 目录')
    temporary = Path(tempfile.mkdtemp(prefix='.' + destination.name + '-', dir=destination.parent))
    backup = destination.with_name('.' + destination.name + '-previous')
    require(not backup.exists(), '存在上次暂存备份，请先检查恢复')
    try:
        shutil.copytree(source, temporary, dirs_exist_ok=True)
        verify(temporary, build_id)
        if destination.exists():
            destination.rename(backup)
        try:
            temporary.rename(destination)
        except OSError:
            if backup.exists():
                backup.rename(destination)
            raise
        if backup.exists():
            shutil.rmtree(backup)
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--variant', choices=['baseline', 'gvisor698'], default='baseline')
    parser.add_argument('--output', type=Path, default=HERE / 'runtime')
    parser.add_argument('--cache', type=Path, default=Path.home() / 'Library/Caches/vpnmgr-build/runtime')
    args = parser.parse_args()
    lock_path = HERE / 'runtime-sources.lock.json'
    lock = json.loads(lock_path.read_text())
    if args.variant == 'gvisor698':
        require(digest(HERE / lock['gvisor698']['patch']) == lock['gvisor698']['patch_sha256'], '补丁 SHA256 不匹配')
    require(platform.system() == 'Darwin' and platform.machine() == 'arm64', '当前 runtime 构建仅支持 macOS arm64')
    tool_env = dict(os.environ, GOTOOLCHAIN='local', GOENV='off', GOWORK='off')
    require(subprocess.check_output(['go', 'env', 'GOVERSION'], text=True, env=tool_env).strip() == lock['go'], '需要锁定的 Go 编译器 ' + lock['go'])
    sdk = subprocess.check_output(['xcrun', '--show-sdk-version'], text=True).strip()
    compiler = subprocess.check_output(['xcrun', 'clang', '--version'], text=True).strip()
    identity = '\n'.join([digest(Path(__file__)), digest(lock_path), digest(HERE / 'entitlements.plist'), args.variant, sdk, compiler])
    build_id = hashlib.sha256(identity.encode()).hexdigest()
    environment = dict(os.environ, GOTOOLCHAIN='local', GOENV='off', GOWORK='off', GOMAXPROCS='4',
                       GOOS='darwin', GOARCH='arm64', CGO_ENABLED='1',
                       CC=subprocess.check_output(['xcrun', '--find', 'clang'], text=True).strip(),
                       GOFLAGS='-mod=readonly -trimpath -buildvcs=false', GOPROXY='https://proxy.golang.org,direct', GOSUMDB='sum.golang.org')
    for key in ('CGO_CFLAGS', 'CGO_CPPFLAGS', 'CGO_CXXFLAGS', 'CGO_LDFLAGS', 'GOEXPERIMENT', 'MACOSX_DEPLOYMENT_TARGET'):
        environment.pop(key, None)
    for scheme, value in urllib.request.getproxies().items():
        if scheme in ('http', 'https'):
            environment.setdefault(scheme.upper() + '_PROXY', value if '://' in value else 'http://' + value)
    args.cache.mkdir(parents=True, exist_ok=True)
    with (args.cache / 'build.lock').open('a') as guard:
        fcntl.flock(guard, fcntl.LOCK_EX)
        result = build(args.cache, lock, args.variant, build_id, environment,
                       {'go': lock['go'], 'sdk': sdk, 'clang': compiler, 'target': lock['platform']})
        stage(result, args.output.absolute(), build_id)
    print('runtime 已暂存: ' + str(args.output) + ' (' + args.variant + ')')


if __name__ == '__main__':
    main()
