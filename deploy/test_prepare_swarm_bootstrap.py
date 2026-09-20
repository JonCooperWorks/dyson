import contextlib
import io
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name('prepare-swarm-bootstrap.py')

class BootstrapProvisioningTests(unittest.TestCase):
    def run_script(self, path, sealed=False, seal_ok=True):
        calls = []
        def run(argv, **kwargs):
            calls.append((argv, kwargs.get('input')))
            if 'system-list' in argv:
                return subprocess.CompletedProcess(argv, 0, 'dyson.bootstrap_token' if sealed else '', '')
            if 'hash-bearer' in argv:
                self.assertIn('--stdin', argv)
                self.assertEqual(len(kwargs['input']), 64)
                self.assertNotIn(kwargs['input'], ' '.join(argv))
                return subprocess.CompletedProcess(argv, 0, '$argon2id$test-only-hash\n', '')
            self.assertIn('system-set', argv)
            self.assertIn('--stdin', argv)
            self.assertNotIn(kwargs['input'], ' '.join(argv))
            return subprocess.CompletedProcess(argv, 0 if seal_ok else 1, '', '')
        with patch.object(sys, 'argv', [str(SCRIPT), '--dyson', '/fake/dyson', '--hash-file', str(path), '--config', '/fake/config']), patch('subprocess.run', side_effect=run), contextlib.redirect_stdout(io.StringIO()) as output:
            runpy.run_path(str(SCRIPT), run_name='__main__')
        for _, secret in calls:
            if secret: self.assertNotIn(secret, output.getvalue())
        return calls

    def test_fresh_bootstrap_is_sealed_and_only_hash_is_saved(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'bootstrap.hash'
            calls = self.run_script(path)
            self.assertEqual(path.read_text(), '$argon2id$test-only-hash\n')
            self.assertEqual(calls[1][1], calls[2][1])
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_existing_pair_is_stable(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'bootstrap.hash'
            path.write_text('$argon2id$existing')
            self.assertEqual(len(self.run_script(path, sealed=True)), 1)
            self.assertEqual(path.read_text(), '$argon2id$existing')

    def test_missing_half_never_rotates_existing_trust(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'bootstrap.hash'
            with self.assertRaises(SystemExit): self.run_script(path, sealed=True)
            self.assertFalse(path.exists())
            path.write_text('$argon2id$existing')
            with self.assertRaises(SystemExit): self.run_script(path, sealed=False)
            self.assertEqual(path.read_text(), '$argon2id$existing')

    def test_sealing_failure_leaves_no_publishable_hash(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'bootstrap.hash'
            with self.assertRaises(SystemExit): self.run_script(path, seal_ok=False)
            self.assertFalse(path.exists())
            self.assertFalse(path.with_suffix('.pending').exists())

if __name__ == '__main__': unittest.main()
