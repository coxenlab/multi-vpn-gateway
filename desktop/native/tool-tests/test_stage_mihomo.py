"""Pinned-source staging uses private files and never signs or runs an engine."""
import gzip
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[2] / 'app/stage-mihomo.py'
definition = importlib.util.spec_from_file_location('stage_mihomo', SCRIPT)
stage = importlib.util.module_from_spec(definition)
definition.loader.exec_module(stage)


class StageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='vpnmgr-engine-source-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.binary = b'synthetic engine bytes'
        self.archive = self.root/'source.gz'
        self.archive.write_bytes(gzip.compress(self.binary))
        self.output = self.root/'output/mihomo'
        self.spec = {'url': 'https://fixture.invalid/engine.gz', 'sha256': stage.digest(self.archive),
                     'size': self.archive.stat().st_size, 'binary_sha256': hashlib.sha256(self.binary).hexdigest(),
                     'binary_size': len(self.binary)}

    def test_pinned_archive_is_published_once_without_resigning(self):
        with patch.object(stage.subprocess, 'run') as commands:
            self.assertTrue(stage.stage(self.output, self.spec, archive=self.archive))
            before = self.output.stat().st_mtime_ns
            self.assertFalse(stage.stage(self.output, self.spec))
        self.assertEqual(self.output.read_bytes(), self.binary)
        self.assertEqual(self.output.stat().st_mtime_ns, before)
        self.assertEqual(commands.call_count, 2)
        self.assertIn('--verify', commands.call_args_list[1].args[0])
        self.assertFalse(list(self.output.parent.glob('.mihomo-source-*')))

    def test_tampered_archive_binary_and_existing_output_are_preserved(self):
        self.archive.write_bytes(b'corrupt')
        with patch.object(stage.subprocess, 'run') as commands:
            with self.assertRaisesRegex(RuntimeError, '归档校验'): stage.stage(self.output, self.spec, archive=self.archive)
            other = self.root/'other'; other.write_bytes(b'wrong')
            with self.assertRaisesRegex(RuntimeError, '二进制'): stage.stage(self.output, self.spec, binary=other)
            self.output.write_bytes(b'existing')
            with self.assertRaisesRegex(RuntimeError, '已有 mihomo'): stage.stage(self.output, self.spec, binary=other)
            commands.assert_not_called()
        self.assertEqual(self.output.read_bytes(), b'existing')
        self.assertFalse(list(self.output.parent.glob('.mihomo-source-*')))

    def test_failed_or_oversized_download_leaves_no_partial_output(self):
        import io
        with patch.object(stage.urllib.request, 'urlopen', return_value=io.BytesIO(b'x' * (self.spec['size'] + 1))):
            with self.assertRaisesRegex(RuntimeError, '下载大小'): stage.stage(self.output, self.spec)
        self.assertFalse(self.output.exists())
        with patch.object(stage.urllib.request, 'urlopen', side_effect=OSError('synthetic network failure')):
            with self.assertRaises(OSError): stage.stage(self.output, self.spec)
        self.assertFalse(self.output.exists())
        self.assertFalse(list(self.output.parent.glob('.mihomo-source-*')))

    def test_racing_destination_is_not_overwritten(self):
        def verify(command, **kwargs):
            if command[0] == '/usr/bin/codesign': self.output.write_bytes(b'other writer')
        with patch.object(stage.subprocess, 'run', side_effect=verify):
            with self.assertRaises(FileExistsError): stage.stage(self.output, self.spec, archive=self.archive)
        self.assertEqual(self.output.read_bytes(), b'other writer')


if __name__ == '__main__':
    unittest.main()
