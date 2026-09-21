"""Headless regressions for the local launcher's ownership and bounded protocol."""
import json
import os
import pty
from pathlib import Path
import runpy
import shlex
import shutil
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


class ShellHookTests(unittest.TestCase):
    def test_only_selected_bare_interactive_connections_use_launcher(self):
        hook = Path(__file__).resolve().parents[1] / 'scripts/pdfterm-shell.sh'
        for shell in ('bash', 'zsh'):
            executable = shutil.which(shell)
            if executable is None:
                self.skipTest(f'{shell} is not installed')
            with self.subTest(shell=shell), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for name in ('ssh', 'pdfterm-ssh'):
                    program = root / name
                    program.write_text(f'#!{sys.executable}\n'
                                       'import json, os, sys\n'
                                       'with open(os.environ["CALLS"], "a") as f:\n'
                                       '    f.write(json.dumps(sys.argv) + "\\n")\n')
                    program.chmod(0o700)
                env = dict(os.environ, PATH=directory + ':' + os.environ['PATH'],
                           CALLS=str(root / 'calls'), PDFTERM_SSH_HOSTS='enabled another')
                for key in ('SSH_CONNECTION', 'SSH_TTY', 'TMUX'):
                    env.pop(key, None)
                flags = ['--noprofile', '--norc'] if shell == 'bash' else ['-f']
                commands = ('ssh enabled; ssh other; ssh enabled true; ssh -N enabled; '
                            'ssh -F config enabled; command ssh enabled; '
                            'SSH_CONNECTION=remote ssh enabled')
                master, slave = pty.openpty()
                try:
                    subprocess.run([executable, *flags, '-ic',
                                    '. ' + shlex.quote(str(hook)) + '; ' + commands],
                                   env=env, stdin=slave, stdout=slave, stderr=slave,
                                   check=True, timeout=5)
                finally:
                    os.close(slave)
                    os.close(master)
                calls = [json.loads(line) for line in (root / 'calls').read_text().splitlines()]
                self.assertEqual([Path(call[0]).name for call in calls],
                                 ['pdfterm-ssh'] + ['ssh'] * 6)
                self.assertEqual([call[1:] for call in calls],
                                 [['enabled'], ['other'], ['enabled', 'true'],
                                  ['-N', 'enabled'], ['-F', 'config', 'enabled'],
                                  ['enabled'], ['enabled']])
                subprocess.run([executable, *flags, '-c',
                                '. ' + shlex.quote(str(hook)) + '; ssh enabled'],
                               env=env, check=True, timeout=5)
                last = json.loads((root / 'calls').read_text().splitlines()[-1])
                self.assertEqual(Path(last[0]).name, 'ssh')


if __name__ == '__main__':
    unittest.main()
