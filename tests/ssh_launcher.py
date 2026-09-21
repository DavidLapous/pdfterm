"""Headless regressions for the local launcher's ownership and bounded protocol."""
import json
from pathlib import Path
import runpy
import socket
import subprocess
import sys
import tempfile
import time
import unittest

launcher = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'scripts/pdfterm-ssh'))
Bridge = launcher['Bridge']


class Windows:
    source = 'editor'

    def __init__(self):
        self.live = set()
        self.fail = False

    def launch(self, argv):
        if self.fail:
            raise RuntimeError('terminal control unavailable')
        self.live.add('viewer')
        return 'viewer'

    def focus(self, identifier):
        if identifier != self.source and identifier not in self.live:
            raise RuntimeError('window is gone')

    def close(self, identifier):
        self.live.remove(identifier)


class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='pdfterm-test-', dir='/tmp')
        self.path = self.directory.name + '/launch.sock'
        self.windows = Windows()
        self.bridge = Bridge(self.path, ['ssh', 'configured-host'], self.windows)
        self.addCleanup(self.directory.cleanup)
        self.addCleanup(self.bridge.close)

    def request(self, payload):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(self.path)
            client.sendall(payload if isinstance(payload, bytes) else json.dumps(payload).encode())
            client.shutdown(socket.SHUT_WR)
            chunks = []
            while chunk := client.recv(4096):
                chunks.append(chunk)
            return json.loads(b''.join(chunks))

    def test_malformed_requests_do_not_kill_listener_or_control_unowned_windows(self):
        for payload in (b'{', [], {'action': 'close', 'id': []}, {'action': 'close', 'id': 'editor'},
                        {'action': 'launch', 'argv': ['viewer', None]}, b' ' * 32769):
            self.assertFalse(self.request(payload)['ok'])
        self.assertTrue(self.request({'action': 'focus', 'id': 'source'})['ok'])
        self.windows.fail = True
        self.assertFalse(self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})['ok'])
        self.windows.fail = False
        self.assertTrue(self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})['ok'])
        self.assertFalse(self.request({'action': 'close', 'id': 'editor'})['ok'])
        self.assertIn('viewer', self.windows.live)
        self.assertTrue(self.request({'action': 'close', 'id': 'viewer'})['ok'])
        self.assertFalse(self.windows.live)

    def test_stalled_peer_expires_and_disconnect_does_not_orphan_owned_viewer(self):
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(self.path)
            client.sendall(b'{')
            start = time.monotonic()
            self.assertTrue(self.request({'action': 'focus', 'id': 'source'})['ok'])
            self.assertLess(time.monotonic() - start, 2.5)
        self.assertTrue(self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})['ok'])
        self.bridge.close()
        self.assertFalse(self.windows.live)

    def test_helpers_enforce_deadline_and_output_cap(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            launcher['run']([sys.executable, '-c', 'import time; time.sleep(30)'], timeout=0.03)
        with self.assertRaisesRegex(RuntimeError, '1 MiB'):
            launcher['run']([sys.executable, '-c', 'import os; os.write(1, b\"x\" * 1100000)'])


if __name__ == '__main__':
    unittest.main()
