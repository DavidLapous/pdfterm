"""Headless regressions for the local launcher's ownership and bounded protocol."""
import errno
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
import threading
import unittest
from unittest.mock import patch

launcher = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'scripts/pdfterm-ssh'))
Bridge = launcher['Bridge']


class Windows:
    source = 'editor'

    def __init__(self):
        self.live = set()
        self.fail = False
        self.serial = 0
        self.query_fail = False

    def launch(self, argv, deadline=None):
        if self.fail:
            raise RuntimeError('terminal control unavailable')
        self.serial += 1
        identifier = 'viewer-' + str(self.serial)
        self.live.add(identifier)
        return identifier

    def focus(self, identifier, deadline=None):
        if identifier != self.source and identifier not in self.live:
            raise RuntimeError('window is gone')

    def existing(self, identifiers, deadline=None):
        if self.query_fail:
            raise RuntimeError('liveness query failed')
        return self.live & identifiers

    def close(self, identifier, deadline=None):
        self.live.discard(identifier)


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
            client.settimeout(6)
            client.connect(self.path)
            launching = isinstance(payload, dict) and payload.get('action') == 'launch'
            client.sendall(payload if isinstance(payload, bytes)
                           else json.dumps(payload).encode() + (b'\n' if launching else b''))
            if not launching:
                try:
                    client.shutdown(socket.SHUT_WR)
                except OSError as error:
                    # An early rejection may close before the client's half-close.
                    if error.errno != errno.ENOTCONN:
                        raise
            chunks = bytearray()
            while chunk := client.recv(4096):
                chunks.extend(chunk)
                if b'\n' in chunks:
                    break
            reply = json.loads(chunks)
            if launching and reply['ok']:
                client.sendall(b'\x06')
            return reply

    def nvim(self, code):
        script = Path(self.directory.name) / 'client.lua'
        root = str(Path(launcher['__file__']).resolve().parent.parent)
        script.write_text('vim.opt.runtimepath:prepend(' + json.dumps(root) + ')\n' + code)
        result = subprocess.run(
            ['nvim', '--headless', '-u', 'NONE', '-l', str(script)],
            env={**os.environ, 'PDFTERM_LAUNCH_SOCKET': self.path},
            capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, '', result.stderr)

    def test_slow_launch_transfers_ownership_to_real_lua_client(self):
        existing, launch = self.windows.existing, self.windows.launch

        def slow_existing(identifiers, deadline=None):
            time.sleep(1.7)
            return existing(identifiers, deadline)

        def slow_launch(argv, deadline=None):
            time.sleep(1.7)
            return launch(argv, deadline)

        with patch.object(self.windows, 'existing', slow_existing), \
                patch.object(self.windows, 'launch', slow_launch):
            self.nvim("""
local terminal = require('pdfterm.terminal')
local owned
local process = terminal.launch_split({ kind = 'ssh', id = 'source' }, 'pdfterm', 'paper.pdf',
  function(result, split)
    assert(result.code == 0, result.stderr)
    owned = assert(split)
  end)
assert(process:wait().code == 0)
assert(owned, 'successful launch did not transfer ownership')
terminal.close(owned)
vim.cmd('qa!')
""")
        self.assertEqual(self.windows.serial, 1)
        self.assertFalse(self.windows.live)

    def test_editor_exit_waits_for_pending_launch_then_closes_viewer(self):
        root = Path(launcher['__file__']).resolve().parent.parent
        binary = os.environ.get('PDFTERM_EXECUTABLE', str(root / 'target/debug/pdfterm'))
        pdf = Path(self.directory.name) / 'paper.pdf'
        pdf.write_text('%PDF-1.7\n')
        launch = self.windows.launch

        def slow_launch(argv, deadline=None):
            time.sleep(1)
            return launch(argv, deadline)

        with patch.object(self.windows, 'launch', slow_launch):
            self.nvim(f"""
vim.env.XDG_CONFIG_HOME = {json.dumps(self.directory.name)}
local adapter = require('pdfterm')
adapter.setup({{ executable = {json.dumps(binary)} }})
local terminal = require('pdfterm.terminal')
local launch = terminal.launch_split
terminal.launch_split = function(...)
  local process = launch(...)
  vim.defer_fn(function() vim.cmd('qa!') end, 50)
  return process
end
adapter.open({json.dumps(str(pdf))})
vim.wait(7000, function() return false end, 5)
error('editor did not exit during launch')
""")
        self.assertEqual(self.windows.serial, 1)
        self.assertFalse(self.windows.live)
        self.assertFalse(self.bridge.owned)

    def test_unacknowledged_launch_rolls_back_while_bridge_stays_alive(self):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(2)
            client.connect(self.path)
            client.sendall(b'{"action":"launch","argv":["viewer","paper.pdf"]}\n')
            reply = json.loads(client.recv(4096))
            self.assertTrue(reply['ok'])
            self.assertIn(reply['id'], self.windows.live)
            # No ownership receipt: a received reply alone cannot commit a launch.
        self.assertTrue(self.request({'action': 'focus', 'id': 'source'})['ok'])
        self.assertNotIn(reply['id'], self.windows.live)
        self.assertNotIn(reply['id'], self.bridge.owned)
        self.assertTrue(self.bridge.thread.is_alive())

    def test_lua_timeout_during_launch_rolls_back_without_retry(self):
        launch = self.windows.launch
        finished = threading.Event()

        def delayed_launch(argv, deadline=None):
            time.sleep(0.2)
            result = launch(argv, deadline)
            finished.set()
            return result

        with patch.object(self.windows, 'launch', delayed_launch):
            self.nvim("""
local count, result = 0, nil
require('pdfterm.socket').request(vim.env.PDFTERM_LAUNCH_SOCKET,
  vim.json.encode({ action = 'launch', argv = { 'viewer', 'paper.pdf' } }),
  function(error, _, reply)
    count = count + 1
    assert(error and error:find('timed out'), tostring(error))
    assert(not reply)
    result = true
  end, 50, true)
assert(vim.wait(1000, function() return result end, 5))
vim.wait(300, function() return false end, 5)
assert(count == 1)
vim.cmd('qa!')
""")
            self.assertTrue(finished.wait(1))
        self.assertTrue(self.request({'action': 'focus', 'id': 'source'})['ok'])
        self.assertEqual(self.windows.serial, 1)
        self.assertFalse(self.windows.live)
        self.assertFalse(self.bridge.owned)

    def test_queued_launch_expires_before_terminal_effects(self):
        self.bridge.control.acquire()
        try:
            start = time.monotonic()
            reply = self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})
            self.assertFalse(reply['ok'])
            self.assertIn('deadline', reply['error'])
            self.assertLess(time.monotonic() - start, 6)
            self.assertEqual(self.windows.serial, 0)
        finally:
            self.bridge.control.release()

    def test_malformed_requests_do_not_kill_listener_or_control_unowned_windows(self):
        for payload in (b'{', [], {'action': 'close', 'id': []}, {'action': 'close', 'id': 'editor'},
                        {'action': 'launch', 'argv': ['viewer', None]}, b' ' * 32769):
            self.assertFalse(self.request(payload)['ok'])
        self.assertTrue(self.request({'action': 'focus', 'id': 'source'})['ok'])
        self.windows.fail = True
        self.assertFalse(self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})['ok'])
        self.windows.fail = False
        viewer = self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})
        self.assertTrue(viewer['ok'])
        self.assertFalse(self.request({'action': 'close', 'id': 'editor'})['ok'])
        self.assertIn(viewer['id'], self.windows.live)
        self.assertTrue(self.request({'action': 'close', 'id': viewer['id']})['ok'])
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


    def test_finite_helper_does_not_inherit_terminal_input(self):
        master, slave = pty.openpty()
        try:
            probe = 'import os; print(os.isatty(0), repr(os.read(0, 1)))'
            code = ('import runpy, sys; '
                    f'run = runpy.run_path({launcher["__file__"]!r})["run"]; '
                    f'print(run([sys.executable, "-c", {probe!r}]))')
            result = subprocess.run([sys.executable, '-c', code], stdin=slave,
                                    capture_output=True, text=True, timeout=4, check=True)
            self.assertEqual(result.stdout.strip(), "False b''")
        finally:
            os.close(master)
            os.close(slave)

    def test_external_exits_do_not_exhaust_limit_and_late_close_is_owned(self):
        retired = []
        for _ in range(32):
            response = self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})
            self.assertTrue(response['ok'], response)
            retired.append(response['id'])
            self.windows.live.remove(response['id'])
        for identifier in retired:
            self.assertTrue(self.request({'action': 'close', 'id': identifier})['ok'])
        self.assertFalse(self.request({'action': 'close', 'id': 'unowned'})['ok'])
        for _ in range(16):
            self.assertTrue(self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})['ok'])
        self.assertFalse(self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})['ok'])
        self.assertEqual(len(self.windows.live), 16)

    def test_query_failure_does_not_forget_ownership_or_launch(self):
        viewer = self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})['id']
        self.windows.query_fail = True
        response = self.request({'action': 'launch', 'argv': ['viewer', 'paper.pdf']})
        self.assertFalse(response['ok'])
        self.assertIn('liveness query failed', response['error'])
        self.assertEqual(self.windows.live, {viewer})
        self.windows.query_fail = False
        self.assertTrue(self.request({'action': 'close', 'id': viewer})['ok'])

    def test_helper_descendants_are_stopped_on_timeout_overflow_and_success(self):
        for outcome in ('timeout', 'overflow', 'success', 'inherited-pipe'):
            with self.subTest(outcome=outcome):
                marker = Path(self.directory.name) / outcome
                child_code = f'import time; from pathlib import Path; time.sleep(1); Path({str(marker)!r}).touch()'
                helper = ('import subprocess, sys, time, os; '
                          f'subprocess.Popen([sys.executable, "-c", {child_code!r}]'
                          + ('); ' if outcome == 'inherited-pipe' else
                             ', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); '))
                helper += {'timeout': 'time.sleep(30)',
                           'overflow': 'os.write(1, b"x" * 1100000)',
                           'success': 'print("finished")',
                           'inherited-pipe': 'print("parent finished")'}[outcome]
                if outcome == 'success':
                    self.assertEqual(launcher['run']([sys.executable, '-c', helper]), 'finished')
                else:
                    exception = RuntimeError if outcome == 'overflow' else subprocess.TimeoutExpired
                    with self.assertRaises(exception):
                        launcher['run']([sys.executable, '-c', helper], timeout=0.4)
                time.sleep(1.1)
                self.assertFalse(marker.exists(), 'helper descendant survived cleanup')

    def test_exited_unreaped_master_group_cleans_up(self):
        with subprocess.Popen([sys.executable, '-c', 'pass'], process_group=0) as child:
            os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT)
            launcher['kill_group'](child)
            self.assertEqual(child.returncode, 0)

    def test_master_allows_forwarding_without_confirmation(self):
        class CaptureMaster(Exception):
            pass

        # Resolve the actual launch argv through OpenSSH, without connecting.
        # Combining ControlMaster=yes with -M silently enables confirmation.
        with patch.dict(os.environ, SSH_CONNECTION='', SSH_TTY='', TMUX=''), \
                patch.object(sys, 'argv', ['pdfterm-ssh', 'test.invalid']), \
                patch.object(sys.stdin, 'isatty', return_value=True), \
                patch.object(os, 'tcgetpgrp', return_value=0), \
                patch.dict(launcher['main'].__globals__, Windows=lambda: self.windows), \
                patch.object(subprocess, 'Popen', side_effect=CaptureMaster) as start:
            with self.assertRaises(CaptureMaster):
                launcher['main']()
        argv = start.call_args.args[0]
        result = subprocess.run([argv[0], '-G', '-F', os.devnull, *argv[1:]],
                                capture_output=True, text=True, check=True, timeout=5)
        policy = dict(line.split(maxsplit=1) for line in result.stdout.splitlines())
        self.assertIn(policy['controlmaster'], ('true', 'yes'))
        self.assertEqual(policy['stdinnull'], 'yes')


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
