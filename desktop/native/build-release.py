#!/usr/bin/env python3
"""原生 macOS 发布入口。--check 只核对输入；构建须指定不存在的仓库外输出目录。"""
import argparse
import ctypes
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import stat
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[2]
MINIMUM_MACOS = '14.0'
TOOLS = ('limactl', 'lima', 'colima', 'docker')
MACHO_TOOLS = ('limactl', 'colima', 'docker')
STATIC_REQUIRED = ('index.html', 'native-login.html', 'css/app.css', 'js/pages/native-login.js',
                   'js/api.js', 'js/app.js', 'js/vncText.js', 'js/vnc-lifecycle.js', 'vendor/novnc/core/rfb.js')
PACKAGE_MODES = ('lite', 'with-vm')


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def verify_gzip(path):
    expanded = 0
    with gzip.open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            expanded += len(block)
            require(expanded <= 32 * 1024 * 1024 * 1024, '内置镜像展开大小超过 32 GiB')
    require(expanded > 0, '内置镜像为空: ' + str(path))


def regular(path, root, executable=False):
    require(not path.is_symlink() and path.resolve().is_relative_to(root.resolve()), '资源路径越界或为链接: ' + str(path))
    require(path.is_file() and path.stat().st_size > 0, '资源缺失或为空: ' + str(path))
    require(not executable or os.access(path, os.X_OK), '资源缺少执行权限: ' + str(path))
    return path


def tree_files(root):
    require(root.is_dir() and not root.is_symlink(), '资源目录无效: ' + str(root))
    files = []
    for path in sorted(root.rglob('*')):
        require(not path.is_symlink(), '资源目录含链接: ' + str(path))
        if path.is_dir():
            continue
        require(stat.S_ISREG(path.stat().st_mode), '资源不是普通文件: ' + str(path))
        files.append(path)
    return files


def product_info(repo):
    spec = json.loads((repo / 'desktop/app/tauri.conf.json').read_text())
    require(re.fullmatch(r'\d{1,4}\.\d{1,2}\.\d{1,2}', spec['version']), '应用版本必须为三个数字段')
    require(spec['identifier'] == 'com.vpnmgr.desktop' and spec['productName'] == 'vpnmgr', '应用身份改变，请先确认数据目录与升级兼容')
    require('.macOS(.v14)' in (repo / 'desktop/native/Package.swift').read_text(), '最低 macOS 版本与原生 Package 不一致')
    return {'CFBundleIdentifier': spec['identifier'], 'CFBundleName': spec['productName'],
            'CFBundleDisplayName': 'VPN 管理网关', 'CFBundleExecutable': 'VPNManager',
            'CFBundlePackageType': 'APPL', 'CFBundleInfoDictionaryVersion': '6.0',
            'CFBundleShortVersionString': spec['version'], 'CFBundleVersion': spec['version'],
            'CFBundleIconFile': 'icon.icns', 'LSMinimumSystemVersion': MINIMUM_MACOS,
            'LSApplicationCategoryType': 'public.app-category.utilities', 'NSPrincipalClass': 'NSApplication',
            'NSHighResolutionCapable': True, 'CFBundleDevelopmentRegion': 'zh_CN',
            'CFBundleLocalizations': ['zh-Hans', 'en']}


def inspect_inputs(runtime, images, vm_images, engine, variant, repo=REPO, package_mode='both'):
    require(package_mode in (*PACKAGE_MODES, 'both'), '安装包模式无效')
    modes = PACKAGE_MODES if package_mode == 'both' else (package_mode,)
    info = product_info(repo)
    lock_file = repo / 'desktop/app/runtime-sources.lock.json'
    lock = json.loads(lock_file.read_text())
    manifest_path = regular(runtime / 'manifest.json', runtime)
    manifest = json.loads(manifest_path.read_text())
    require(manifest.get('schema') == 1 and manifest.get('sources') == lock, 'runtime 来源清单与当前锁文件不一致')
    require(manifest.get('variant') == variant, 'runtime 候选不匹配，选择候选必须显式指定 --runtime-variant')
    toolchain = manifest.get('toolchain', {})
    require(toolchain.get('target') == 'darwin-arm64' and toolchain.get('go') == lock['go'], 'runtime 工具链身份不匹配')
    identity = '\n'.join([digest(repo / 'desktop/app/build-runtime.py'), digest(lock_file),
                          digest(repo / 'desktop/app/entitlements.plist'), variant, toolchain['sdk'], toolchain['clang']])
    require(manifest.get('build_id') == hashlib.sha256(identity.encode()).hexdigest(), 'runtime 构建身份与当前脚本不一致')
    expected_lima = lock['lima']['version'] if variant == 'baseline' else lock['gvisor698']['lima_version']
    require(manifest.get('lima_version') == expected_lima, 'runtime Lima 版本不匹配')
    required = ['bin/' + name for name in TOOLS] + ['share/lima/lima-guestagent.Linux-aarch64.gz']
    entries = manifest.get('files', {})
    require(isinstance(entries, dict) and all(name in entries for name in required)
            and any(name.startswith('share/lima/templates/') for name in entries), 'runtime 清单缺少工具、guestagent 或模板')
    actual = {str(path.relative_to(runtime)) for path in tree_files(runtime) if 'helper' not in path.relative_to(runtime).parts[:1]}
    require(actual == set(entries) | {'manifest.json'}, 'runtime 存在清单外文件或缺失文件')
    sources = {'runtime/manifest.json': manifest_path}
    for name, expected in entries.items():
        relative = Path(name)
        require(relative.parts and not relative.is_absolute() and '..' not in relative.parts and relative.parts[0] != 'helper', 'runtime 清单路径无效')
        path = regular(runtime / relative, runtime, name in ['bin/' + tool for tool in TOOLS])
        require(digest(path) == expected, 'runtime 文件校验失败: ' + name)
        sources['runtime/' + name] = path
    static = repo / 'app/static'
    for name in STATIC_REQUIRED:
        regular(static / name, static)
    for path in tree_files(static):
        sources['static/' + str(path.relative_to(static))] = path
    sources['icon.icns'] = regular(repo / 'desktop/app/icons/icon.icns', repo)
    sources['runtime/helper/mihomo'] = regular(engine, engine.parent, executable=True)
    vm_source = json.loads((repo / 'desktop/app/vm-image-source.json').read_text())
    require(vm_source.get('schema') == 1 and vm_source['colima_version'] == lock['colima']['version'], 'VM 镜像与 Colima 来源版本不一致')
    key = hashlib.sha256(vm_source['url'].encode()).hexdigest()
    archives = [('images/mihomo.tar.gz', images / 'mihomo.tar.gz'), ('images/oss-vpn.tar.gz', images / 'oss-vpn.tar.gz')]
    if 'with-vm' in modes:
        archives.append(('vm-image/' + key, vm_images / key))
    for destination, path in archives:
        regular(path, path.parent)
        verify_gzip(path)
        sources[destination] = path
    hashes = {name: digest(path) for name, path in sources.items()}
    return {'info': info, 'sources': sources, 'hashes': hashes, 'runtime_build_id': manifest['build_id'],
            'runtime_variant': variant, 'bytes': sum(path.stat().st_size for path in sources.values()),
            'package_modes': modes, 'vm_cache_key': key}


def package_sources(inputs, mode):
    require(mode in inputs['package_modes'], '安装包模式未经过输入校验')
    return {name: path for name, path in inputs['sources'].items()
            if mode == 'with-vm' or not name.startswith('vm-image/')}


def verify_binary(path):
    subprocess.run(['/usr/bin/lipo', str(path), '-verify_arch', 'arm64'], check=True)
    subprocess.run(['/usr/bin/codesign', '--verify', '--strict', str(path)], check=True)


def verify_runtime_files(sources):
    for name in [*('runtime/bin/' + tool for tool in MACHO_TOOLS), 'runtime/helper/mihomo']:
        verify_binary(sources[name])
    # Upstream lima is a POSIX shell wrapper, not a Mach-O binary. Its bytes are
    # covered by the runtime manifest; parse only, never run the wrapper here.
    subprocess.run(['/bin/sh', '-n', str(sources['runtime/bin/lima'])], check=True)


def validate_destination(path, repo=REPO):
    path = path.expanduser().absolute()
    require(not os.path.lexists(path), '输出目录已存在，拒绝覆盖: ' + str(path))
    require(path.parent.is_dir(), '输出目录的父目录不存在')
    resolved = path.resolve()
    require(not resolved.is_relative_to(repo.resolve()), '输出目录须位于项目仓库之外')
    require(not resolved.is_relative_to(Path('/Applications')) and not resolved.is_relative_to(Path.home() / 'Applications')
            and not any(parent.suffix.lower() == '.app' for parent in (resolved, *resolved.parents)), '不能向应用安装目录输出')
    return resolved


def publish_directory(source, destination):
    # macOS SDK sys/stdio.h: RENAME_EXCL=4; refuse even an empty destination
    # created by another process during a long build.
    library = ctypes.CDLL(None, use_errno=True)
    rename = library.renamex_np
    rename.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    if rename(os.fsencode(source), os.fsencode(destination), 4) != 0:
        raise OSError(ctypes.get_errno(), '输出目录发布失败，未覆盖现有目标', str(destination))


def git_revision(repo):
    status = subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=normal'], cwd=repo, text=True)
    require(not status.strip(), '发布构建需要已提交且干净的工作区')
    return subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip()


def build_release(inputs, output, repo=REPO):
    output = validate_destination(output, repo)
    revision = git_revision(repo)
    environment = {key: value for key, value in os.environ.items() if key not in
                   ('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'SWIFTFLAGS')}
    environment['MACOSX_DEPLOYMENT_TARGET'] = MINIMUM_MACOS
    compiler_info = {tool: subprocess.check_output([tool, '--version'], cwd=repo, env=environment, text=True).strip()
                     for tool in ('swift', 'rustc', 'cargo')}
    with tempfile.TemporaryDirectory(prefix='.vpnmgr-release-', dir=output.parent) as temporary:
        stage = Path(temporary)
        products = stage / 'products'; products.mkdir()
        work = stage / 'compile'
        for name in ('core', 'helper'):
            subprocess.run(['cargo', 'build', '--locked', '--offline', '--release', '--manifest-path', str(repo / 'desktop' / name / 'Cargo.toml'),
                            '--target-dir', str(work / name)], cwd=repo, env=environment, check=True)
        subprocess.run(['swift', 'build', '--configuration', 'release', '--arch', 'arm64', '--package-path', str(repo / 'desktop/native'),
                        '--scratch-path', str(work / 'swift'), '-Xswiftc', '-warnings-as-errors'], cwd=repo, env=environment, check=True)
        compiled = {'vpnmgr-core': work / 'core/release/vpnmgr-core', 'runtime/helper/vpnmgr-helper': work / 'helper/release/vpnmgr-helper'}
        reports = {}
        for mode in inputs['package_modes']:
            # Each mode starts empty; neither app nor disk staging is reused across modes.
            sources = package_sources(inputs, mode)
            app = products / mode / 'vpnmgr.app'
            resources = app / 'Contents/Resources'; resources.mkdir(parents=True)
            executable = app / 'Contents/MacOS/VPNManager'; executable.parent.mkdir()
            shutil.copy2(work / 'swift/arm64-apple-macosx/release/VPNManager', executable)
            for name, source in sources.items():
                target = resources / name; target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)
                require(digest(target) == inputs['hashes'][name], '构建期间资源变化: ' + name)
            for name, source in compiled.items():
                target = resources / name; target.parent.mkdir(parents=True, exist_ok=True); shutil.copy2(source, target)
            with (app / 'Contents/Info.plist').open('wb') as stream:
                plistlib.dump(inputs['info'], stream)
            bundle_mode = {'schema': 1, 'mode': mode, 'vm_cache_key': inputs['vm_cache_key']}
            (resources / 'bundle-mode.json').write_text(json.dumps(bundle_mode, indent=2) + '\n')
            for binary in [executable, *(resources / name for name in compiled)]:
                subprocess.run(['/usr/bin/codesign', '--force', '--sign', '-', str(binary)], check=True)
                verify_binary(binary)
            verify_runtime_files({name: resources / name for name in sources})
            report = {'schema': 1, 'ui': 'SwiftUI', 'source_commit': revision, 'version': inputs['info']['CFBundleShortVersionString'],
                      'identifier': inputs['info']['CFBundleIdentifier'], 'minimum_macos': MINIMUM_MACOS, 'architecture': 'arm64',
                      'package_mode': mode, 'runtime_variant': inputs['runtime_variant'], 'runtime_build_id': inputs['runtime_build_id'],
                      'signing': 'ad-hoc', 'notarized': False, 'compiler_versions': compiler_info,
                      'input_sha256': {name: inputs['hashes'][name] for name in sources}}
            (resources / 'build-info.json').write_text(json.dumps(report, indent=2) + '\n')
            subprocess.run(['/usr/bin/codesign', '--force', '--sign', '-', str(app)], check=True)
            subprocess.run(['/usr/bin/codesign', '--verify', '--deep', '--strict', str(app)], check=True)
            disk_source = stage / ('disk-' + mode); disk_source.mkdir()
            shutil.copytree(app, disk_source / app.name, copy_function=os.link)
            (disk_source / 'Applications').symlink_to('/Applications')
            dmg = products / ('vpnmgr_' + report['version'] + '_arm64_' + mode + '.dmg')
            subprocess.run(['/usr/bin/hdiutil', 'create', '-volname', 'vpnmgr ' + mode, '-srcfolder', str(disk_source), '-format', 'UDZO', str(dmg)], check=True)
            subprocess.run(['/usr/bin/hdiutil', 'verify', str(dmg)], check=True)
            (products / (dmg.name + '.sha256')).write_text(digest(dmg) + '  ' + dmg.name + '\n')
            (products / mode / 'build-info.json').write_text(json.dumps(report, indent=2) + '\n')
            reports[mode] = report
        require(git_revision(repo) == revision, '构建期间源码改变，取消发布')
        (products / 'build-info.json').write_text(json.dumps({'schema': 1, 'packages': reports}, indent=2) + '\n')
        publish_directory(products, output)
    print('原生产物已保存: ' + str(output))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='只读取、校验来源与资源，不编译或打包')
    parser.add_argument('--output', type=Path, help='不存在的仓库外输出目录，不允许安装目录')
    parser.add_argument('--runtime-dir', type=Path, default=REPO / 'desktop/app/runtime')
    parser.add_argument('--images-dir', type=Path, default=REPO / 'desktop/app/bundled-images')
    parser.add_argument('--vm-image-dir', type=Path, default=REPO / 'desktop/app/vm-image')
    parser.add_argument('--mihomo', type=Path, default=REPO / 'desktop/app/runtime/helper/mihomo')
    parser.add_argument('--runtime-variant', choices=['baseline', 'gvisor698'], default='baseline')
    parser.add_argument('--package-mode', choices=['both', *PACKAGE_MODES], default='both',
                        help='默认同时输出轻量版和带 VM 镜像版；lite 不读取或携带 VM 镜像')
    args = parser.parse_args()
    if not args.check and args.output is None:
        parser.error('请使用 --check 核对资源，或用 --output 显式指定新的输出目录')
    require(platform.system() == 'Darwin' and platform.machine() == 'arm64', '当前原生发布仅支持 macOS arm64')
    if args.output is not None:
        validate_destination(args.output)
    inputs = inspect_inputs(args.runtime_dir, args.images_dir, args.vm_image_dir, args.mihomo, args.runtime_variant, package_mode=args.package_mode)
    verify_runtime_files(inputs['sources'])
    if args.check:
        print(json.dumps({'resources_ready': True, 'version': inputs['info']['CFBundleShortVersionString'],
                          'runtime_variant': inputs['runtime_variant'], 'runtime_build_id': inputs['runtime_build_id'],
                          'resource_files': len(inputs['sources']), 'resource_bytes': inputs['bytes'],
                          'packages': {mode: {'resource_files': len(package_sources(inputs, mode)),
                                              'resource_bytes': sum(path.stat().st_size for path in package_sources(inputs, mode).values())}
                                       for mode in inputs['package_modes']},
                          'built': False, 'installed': False}, ensure_ascii=False, indent=2))
        return
    build_release(inputs, args.output)


if __name__ == '__main__':
    try:
        main()
    except (RuntimeError, OSError, EOFError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        raise SystemExit('原生发布检查未通过: ' + str(error))
