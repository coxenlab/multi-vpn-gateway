"""Private synthetic resources; compilers, signing and DMG commands never execute."""
import contextlib
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import platform
import plistlib
import subprocess
import tempfile
import tarfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'build-release.py'
spec = importlib.util.spec_from_file_location('native_release', SCRIPT)
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='vpnmgr-release-test-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.repo = self.root / 'source'
        self.runtime = self.root / 'runtime'
        self.images = self.root / 'images'
        self.vm = self.root / 'vm'
        self.engine = self.root / 'engine/mihomo'
        app = self.repo / 'desktop/app'
        self.write(app / 'tauri.conf.json', json.dumps({'version':'2.7.3','identifier':'com.vpnmgr.desktop','productName':'vpnmgr'}))
        self.write(self.repo / 'desktop/native/Package.swift', '.macOS(.v14)')
        self.write(app / 'build-runtime.py', 'synthetic runtime builder')
        self.write(app / 'entitlements.plist', 'synthetic entitlements')
        self.write(app / 'icons/icon.icns', 'synthetic icon')
        lock = {'go':'go-fixture', 'lima':{'version':'v2.1.2'}, 'colima':{'version':'v0.10.3'}, 'gvisor698':{'lima_version':'v2.1.2-fixture'}}
        self.write(app / 'runtime-sources.lock.json', json.dumps(lock))
        vm_source = {'schema':1, 'colima_version':'v0.10.3', 'url':'https://fixture.invalid/vm.gz'}
        self.write(app / 'vm-image-source.json', json.dumps(vm_source))
        self.vm_key = hashlib.sha256(vm_source['url'].encode()).hexdigest()
        self.write(self.vm / self.vm_key, gzip.compress(b'synthetic vm image'))
        for name in ['mihomo.tar.gz', 'oss-vpn.tar.gz']:
            self.write(self.images / name, gzip.compress(b'synthetic docker archive'))
        for name in release.STATIC_REQUIRED:
            self.write(self.repo / 'app/static' / name, 'synthetic static resource')
        self.write(self.repo / 'app/static/js/extra-dependency.js', 'preserve the complete static tree')
        for name in release.TOOLS:
            self.write(self.runtime / 'bin' / name, 'synthetic runtime tool', executable=True)
        self.write(self.engine, 'synthetic host engine', executable=True)
        configuration = json.dumps({'os': 'linux', 'architecture': 'arm64', 'rootfs': {'diff_ids': []}}).encode()
        config_hash = hashlib.sha256(configuration).hexdigest()
        self.engine_source = {'schema': 1, 'image': 'metacubex/mihomo:v1.19.27',
                              'platforms': {'arm64': {'config': 'sha256:' + config_hash}},
                              'darwin_arm64': {'binary_sha256': release.digest(self.engine)}}
        self.write(self.repo/'app/mihomo-source.json', json.dumps(self.engine_source))
        with tarfile.open(self.images/'mihomo.tar.gz', 'w:gz') as archive:
            for name, body in [(config_hash+'.json', configuration), ('manifest.json', json.dumps([
                    {'Config': config_hash+'.json', 'RepoTags': [self.engine_source['image']], 'Layers': []}]).encode())]:
                entry = tarfile.TarInfo(name); entry.size = len(body); archive.addfile(entry, io.BytesIO(body))
        self.write(self.runtime / 'share/lima/lima-guestagent.Linux-aarch64.gz', gzip.compress(b'guest'))
        self.write(self.runtime / 'share/lima/templates/default.yaml', 'synthetic template')
        self.manifest = {'schema':1,'sources':lock,'variant':'baseline','lima_version':'v2.1.2',
                         'toolchain':{'go':'go-fixture','target':'darwin-arm64','sdk':'fixture-sdk','clang':'fixture-clang'}}
        identity = '\n'.join([release.digest(app/'build-runtime.py'),release.digest(app/'runtime-sources.lock.json'),
                              release.digest(app/'entitlements.plist'),'baseline','fixture-sdk','fixture-clang'])
        self.manifest['build_id'] = hashlib.sha256(identity.encode()).hexdigest()
        self.manifest['files'] = {str(path.relative_to(self.runtime)):release.digest(path) for path in self.runtime.rglob('*') if path.is_file()}
        self.save_manifest()
        self.write(self.runtime / 'helper/vpnmgr-helper', 'STALE HELPER MUST NOT SHIP', executable=True)

    def write(self, path, value, executable=False):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(value.encode() if isinstance(value, str) else value)
        if executable: path.chmod(0o755)

    def save_manifest(self):
        self.write(self.runtime / 'manifest.json', json.dumps(self.manifest))

    def inspect(self, variant='baseline', package_mode='both'):
        return release.inspect_inputs(self.runtime, self.images, self.vm, self.engine, variant, self.repo, package_mode)

    def test_inspection_is_read_only_and_selects_only_product_resources(self):
        self.write(self.images / 'personal.tar.gz', gzip.compress(b'must not ship'))
        before = {path: (release.digest(path), path.stat().st_mtime_ns) for path in self.root.rglob('*') if path.is_file()}
        with patch.object(subprocess, 'run', side_effect=AssertionError('inspection must not run tools')):
            result = self.inspect()
        after = {path: (release.digest(path), path.stat().st_mtime_ns) for path in self.root.rglob('*') if path.is_file()}
        self.assertEqual(before, after)
        self.assertEqual(result['info']['CFBundleShortVersionString'], '2.7.3')
        self.assertEqual(result['info']['CFBundleExecutable'], 'VPNManager')
        self.assertEqual(result['info']['LSMinimumSystemVersion'], '14.0')
        self.assertIn('static/js/extra-dependency.js', result['sources'])
        self.assertNotIn('images/personal.tar.gz', result['sources'])
        self.assertNotIn('runtime/helper/vpnmgr-helper', result['sources'])

    def test_lite_does_not_read_or_include_stale_or_missing_vm_image(self):
        image = self.vm / self.vm_key
        image.write_bytes(b'invalid stale VM image')
        result = self.inspect(package_mode='lite')
        self.assertEqual(result['package_modes'], ('lite',))
        self.assertFalse(any(name.startswith('vm-image/') for name in result['sources']))
        image.unlink()
        self.assertEqual(self.inspect(package_mode='lite')['hashes'], result['hashes'])
        for mode in ['both', 'with-vm']:
            with self.assertRaisesRegex(RuntimeError, '资源缺失'): self.inspect(package_mode=mode)
        with self.assertRaisesRegex(RuntimeError, '未经过输入校验'): release.package_sources(result, 'with-vm')

    def test_dual_mode_selects_only_the_matching_vm_cache_and_shares_other_resources(self):
        self.write(self.vm / ('b' * 64), gzip.compress(b'stale other version'))
        inputs = self.inspect()
        self.assertEqual(inputs['package_modes'], ('lite', 'with-vm'))
        lite = release.package_sources(inputs, 'lite'); full = release.package_sources(inputs, 'with-vm')
        self.assertEqual(set(full) - set(lite), {'vm-image/' + self.vm_key})
        self.assertEqual(lite, {name: path for name, path in full.items() if not name.startswith('vm-image/')})
        self.assertEqual(self.inspect(package_mode='with-vm')['sources'], full)

    def test_tampered_runtime_and_variant_mismatch_are_rejected(self):
        with self.assertRaisesRegex(RuntimeError, '候选不匹配'): self.inspect('gvisor698')
        self.write(self.runtime / 'bin/colima', 'tampered', executable=True)
        with self.assertRaisesRegex(RuntimeError, '校验失败'): self.inspect()

    def test_host_and_container_mihomo_must_match_source_lock(self):
        self.write(self.engine, 'different host version', executable=True)
        with self.assertRaisesRegex(RuntimeError, '宿主 mihomo'): self.inspect()
        self.write(self.engine, 'synthetic host engine', executable=True)
        self.engine_source['platforms']['arm64']['config'] = 'sha256:' + 'b' * 64
        self.write(self.repo/'app/mihomo-source.json', json.dumps(self.engine_source))
        with self.assertRaisesRegex(RuntimeError, '镜像版本或内容'): self.inspect()

    def test_correct_image_metadata_cannot_hide_a_corrupt_layer(self):
        layer = b'synthetic layer tar'
        config = json.dumps({'os': 'linux', 'architecture': 'arm64',
                             'rootfs': {'diff_ids': ['sha256:' + hashlib.sha256(layer).hexdigest()]}}).encode()
        key = hashlib.sha256(config).hexdigest()
        self.engine_source['platforms']['arm64']['config'] = 'sha256:' + key
        self.write(self.repo/'app/mihomo-source.json', json.dumps(self.engine_source))
        for payload, succeeds in [(layer, True), (gzip.compress(layer), True), (b'corrupted layer', False)]:
            with tarfile.open(self.images/'mihomo.tar.gz', 'w:gz') as archive:
                for name, body in [('layer.tar', payload), (key+'.json', config), ('manifest.json', json.dumps([
                        {'Config': key+'.json', 'RepoTags': [self.engine_source['image']], 'Layers': ['layer.tar']}]).encode())]:
                    entry = tarfile.TarInfo(name); entry.size = len(body); archive.addfile(entry, io.BytesIO(body))
            if succeeds: self.inspect()
            else:
                with self.assertRaisesRegex(RuntimeError, '分层内容'): self.inspect()

    def test_stale_build_identity_and_source_lock_are_rejected(self):
        self.manifest['build_id'] = '0' * 64; self.save_manifest()
        with self.assertRaisesRegex(RuntimeError, '构建身份'): self.inspect()
        self.manifest['sources']['go'] = 'unexpected'; self.save_manifest()
        with self.assertRaisesRegex(RuntimeError, '来源清单'): self.inspect()

    def test_unlisted_and_escaping_files_are_rejected(self):
        extra = self.runtime / 'unlisted-secret'; self.write(extra, 'do not ship')
        with self.assertRaisesRegex(RuntimeError, '清单外'): self.inspect()
        extra.unlink()
        template = self.runtime / 'share/lima/templates/default.yaml'
        template.unlink(); template.symlink_to(self.engine)
        with self.assertRaisesRegex(RuntimeError, '链接'): self.inspect()

    def test_missing_static_vm_version_and_truncated_archives_are_rejected(self):
        login = self.repo / 'app/static/native-login.html'; original = login.read_bytes(); login.unlink()
        with self.assertRaisesRegex(RuntimeError, '资源缺失'): self.inspect()
        self.write(login, original)
        vm_spec = self.repo / 'desktop/app/vm-image-source.json'; old = vm_spec.read_bytes()
        self.write(vm_spec, json.dumps({'schema':1,'colima_version':'wrong','url':'fixture'}))
        with self.assertRaisesRegex(RuntimeError, 'VM 镜像'): self.inspect()
        self.write(vm_spec, old)
        image = self.images / 'mihomo.tar.gz'; image.write_bytes(image.read_bytes()[:-4])
        with self.assertRaises(EOFError): self.inspect()

    def test_destination_cannot_overwrite_or_install(self):
        output = self.root / 'new-output'
        self.assertEqual(release.validate_destination(output, self.repo), output.resolve())
        output.mkdir()
        with self.assertRaisesRegex(RuntimeError, '已存在'): release.validate_destination(output, self.repo)
        with self.assertRaisesRegex(RuntimeError, '仓库之外'): release.validate_destination(self.repo/'release', self.repo)
        app = self.root / 'existing.app'; app.mkdir()
        with self.assertRaisesRegex(RuntimeError, '安装目录'): release.validate_destination(app/'release', self.repo)
        link = self.root / 'dangling'; link.symlink_to(self.root/'absent')
        with self.assertRaisesRegex(RuntimeError, '已存在'): release.validate_destination(link, self.repo)

    @unittest.skipUnless(platform.system() == 'Darwin', 'macOS exclusive directory rename')
    def test_atomic_publish_preserves_a_concurrently_created_destination(self):
        source = self.root/'pending'; source.mkdir(); self.write(source/'payload', 'new')
        destination = self.root/'finished'; destination.mkdir()
        with self.assertRaises(OSError): release.publish_directory(source, destination)
        self.assertTrue((source/'payload').exists()); self.assertEqual(list(destination.iterdir()), [])
        destination.rmdir(); release.publish_directory(source, destination)
        self.assertEqual((destination/'payload').read_text(), 'new'); self.assertFalse(source.exists())

    def test_no_arguments_and_check_never_dispatch_build(self):
        with patch('sys.argv', [str(SCRIPT)]), contextlib.redirect_stderr(io.StringIO()), patch.object(release, 'inspect_inputs') as inspect:
            with self.assertRaises(SystemExit): release.main()
            inspect.assert_not_called()
        inputs = self.inspect()
        with patch('sys.argv', [str(SCRIPT), '--check']), patch.object(release.platform, 'system', return_value='Darwin'), \
             patch.object(release.platform, 'machine', return_value='arm64'), patch.object(release, 'inspect_inputs', return_value=inputs), \
             patch.object(release, 'verify_binary') as verify, patch.object(release, 'build_release', side_effect=AssertionError('check cannot build')), \
             contextlib.redirect_stdout(io.StringIO()) as output:
            release.main()
        report = json.loads(output.getvalue())
        self.assertFalse(report['built']); self.assertEqual(verify.call_count, 4)
        self.assertEqual(set(report['packages']), {'lite', 'with-vm'})

    @unittest.skipUnless(platform.system() == 'Darwin', 'macOS publishing command orchestration')
    def test_build_uses_fresh_core_and_helper_and_publishes_only_after_validation(self):
        inputs = self.inspect(); output = self.root/'finished'; calls = []
        def run(command, **kwargs):
            calls.append(command)
            if command[:2] == ['cargo','build']:
                target = Path(command[command.index('--target-dir')+1]); component = target.name
                self.write(target/'release'/('vpnmgr-'+component), 'FRESH '+component, executable=True)
            elif command[:2] == ['swift','build']:
                target = Path(command[command.index('--scratch-path')+1])
                self.write(target/'arm64-apple-macosx/release/VPNManager', 'FRESH SwiftUI', executable=True)
            elif command[:2] == ['/usr/bin/hdiutil','create']:
                disk = Path(command[command.index('-srcfolder')+1])
                resources = disk / 'vpnmgr.app/Contents/Resources'
                mode = json.loads((resources/'bundle-mode.json').read_text())['mode']
                self.assertEqual(disk.name, 'disk-' + mode)
                self.assertEqual((resources/'vm-image').exists(), mode == 'with-vm')
                self.write(Path(command[-1]), 'SYNTHETIC TEST DATA, NOT A DISK IMAGE')
            elif command[0] not in ('/usr/bin/lipo','/usr/bin/codesign','/usr/bin/hdiutil','/bin/sh'):
                self.fail('unexpected command: '+str(command))
            return subprocess.CompletedProcess(command, 0)
        with patch.object(release, 'git_revision', return_value='fixture-commit'), patch.object(subprocess, 'check_output', return_value='fixture compiler'), \
             patch.object(subprocess, 'run', side_effect=run), contextlib.redirect_stdout(io.StringIO()):
            release.build_release(inputs, output, self.repo)
        report = json.loads((output/'build-info.json').read_text())
        self.assertEqual(set(report['packages']), {'lite', 'with-vm'})
        for mode in inputs['package_modes']:
            resources = output/mode/'vpnmgr.app/Contents/Resources'
            self.assertEqual((resources/'vpnmgr-core').read_text(), 'FRESH core')
            self.assertEqual((resources/'runtime/helper/vpnmgr-helper').read_text(), 'FRESH helper')
            with (output/mode/'vpnmgr.app/Contents/Info.plist').open('rb') as stream: info = plistlib.load(stream)
            self.assertEqual(info['CFBundleExecutable'], 'VPNManager')
            package = report['packages'][mode]
            self.assertFalse(package['notarized']); self.assertEqual(package['ui'], 'SwiftUI')
            self.assertEqual(package['package_mode'], mode)
            self.assertEqual(package, json.loads((resources/'build-info.json').read_text()))
            self.assertEqual(json.loads((resources/'bundle-mode.json').read_text()),
                             {'schema': 1, 'mode': mode, 'vm_cache_key': self.vm_key})
            self.assertEqual((resources/'vm-image').exists(), mode == 'with-vm')
            self.assertEqual('vm-image/' + self.vm_key in package['input_sha256'], mode == 'with-vm')
            dmg = output / ('vpnmgr_2.7.3_arm64_' + mode + '.dmg')
            self.assertEqual(Path(str(dmg)+'.sha256').read_text(), release.digest(dmg) + '  ' + dmg.name + '\n')
        self.assertEqual(sum(command[:2] == ['swift','build'] for command in calls), 1)
        self.assertEqual(sum(command[:2] == ['cargo','build'] for command in calls), 2)
        self.assertFalse(list(self.root.glob('.vpnmgr-release-*')))
        self.assertEqual(calls[-1][:2], ['/usr/bin/hdiutil','verify'])
        self.assertTrue(any(command[:2] == ['/bin/sh','-n'] for command in calls))
        self.assertFalse(any(command[0] == '/usr/bin/lipo' and command[1].endswith('/bin/lima') for command in calls))
        failed_output = self.root/'failed'
        with patch.object(release, 'git_revision', return_value='fixture-commit'), patch.object(subprocess, 'check_output', return_value='fixture compiler'), \
             patch.object(subprocess, 'run', side_effect=subprocess.CalledProcessError(1, ['synthetic compiler'])):
            with self.assertRaises(subprocess.CalledProcessError): release.build_release(inputs, failed_output, self.repo)
        self.assertFalse(failed_output.exists()); self.assertFalse(list(self.root.glob('.vpnmgr-release-*')))
        def fail_second_image(command, **kwargs):
            if command[:2] == ['/usr/bin/hdiutil','verify'] and command[-1].endswith('_with-vm.dmg'):
                raise subprocess.CalledProcessError(1, command)
            return run(command, **kwargs)
        with patch.object(release, 'git_revision', return_value='fixture-commit'), patch.object(subprocess, 'check_output', return_value='fixture compiler'), \
             patch.object(subprocess, 'run', side_effect=fail_second_image):
            with self.assertRaises(subprocess.CalledProcessError): release.build_release(inputs, failed_output, self.repo)
        self.assertFalse(failed_output.exists()); self.assertFalse(list(self.root.glob('.vpnmgr-release-*')))
        # A later lite-only build uses no prior staging, even with no VM input available.
        (self.vm/self.vm_key).unlink()
        with patch.object(release, 'git_revision', return_value='fixture-commit'), patch.object(subprocess, 'check_output', return_value='fixture compiler'), \
             patch.object(subprocess, 'run', side_effect=run), contextlib.redirect_stdout(io.StringIO()):
            release.build_release(self.inspect(package_mode='lite'), failed_output, self.repo)
        self.assertTrue((failed_output/'lite/vpnmgr.app').is_dir())
        self.assertFalse((failed_output/'with-vm').exists())
        self.assertFalse((failed_output/'lite/vpnmgr.app/Contents/Resources/vm-image').exists())


if __name__ == '__main__':
    unittest.main()
