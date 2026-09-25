#!/usr/bin/env python3
"""Opt-in native contract: local Lua and SSH backends, using only owned surfaces.

Run with --terminal kitty --to unix:/path/to/kitty.sock, --terminal ghostty,
or --terminal wezterm [--to /path/to/owned/gui.sock]. Without --to the WezTerm
probe starts its own isolated GUI instance.
Requires Neovim and a GUI session; never substitutes mocked terminal effects.
"""
import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
launcher = runpy.run_path(str(ROOT / 'scripts/pdfterm-ssh'))
run, applescript = launcher['run'], launcher['applescript']

def wezterm(*args):
    return run(['env', 'WEZTERM_UNIX_SOCKET=' + os.environ['WEZTERM_UNIX_SOCKET'],
                'wezterm', 'cli', '--no-auto-start', *args])


def wait(predicate, timeout=5):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError('native terminal transition timed out')
        time.sleep(0.02)


def snapshot(kind):
    if kind == 'kitty':
        return {str(w['id']): (str(osw['id']), str(tab['id']), w, tab['layout'])
                for osw in json.loads(run(['kitten', '@', 'ls']))
                for tab in osw['tabs'] for w in tab['windows']}
    if kind == 'wezterm':
        return {str(row['pane_id']): (str(row['window_id']), str(row['tab_id']), row)
                for row in json.loads(wezterm('list', '--format', 'json'))}
    rows = applescript('''set rows to {}
repeat with w in windows
repeat with t in tabs of w
repeat with s in terminals of t
set end of rows to (id of s as text) & " " & (id of w as text) & " " & (id of t as text)
end repeat
end repeat
end repeat
set AppleScript's text item delimiters to linefeed
return rows as text''')
    return {parts[0]: (parts[1], parts[2], {}) for row in rows.splitlines()
            if (parts := row.split())}


def focused(kind, identifier):
    if kind == 'kitty':
        return snapshot(kind)[identifier][2]['is_active']
    if kind == 'wezterm':
        return snapshot(kind)[identifier][2]['is_active']
    return applescript('return id of focused terminal of selected tab of front window') == identifier


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--terminal', choices=['kitty', 'ghostty', 'wezterm'], required=True)
    parser.add_argument('--to', help='explicit owned Kitty/WezTerm GUI socket')
    args = parser.parse_args()
    if args.terminal == 'kitty':
        if not args.to:
            parser.error('Kitty requires an explicit owned test instance via --to')
        os.environ['KITTY_LISTEN_ON'] = args.to
    else:
        os.environ.pop('KITTY_WINDOW_ID', None)
        os.environ['TERM_PROGRAM'] = 'WezTerm' if args.terminal == 'wezterm' else 'ghostty'
    if args.terminal != 'wezterm' and args.terminal != 'kitty' and args.to:
        parser.error('--to is only valid for Kitty or WezTerm')
    os.environ.pop('PDFTERM_LAUNCH_SOCKET', None)
    os.environ['PDFTERM_ROOT'] = str(ROOT)
    os.environ['PDFTERM_TEST_KIND'] = args.terminal
    with tempfile.TemporaryDirectory(prefix=".terminal-test-' ", dir=ROOT) as temporary, \
            ExitStack() as cleanup:
        directory = Path(temporary)
        os.environ['XDG_CONFIG_HOME'] = temporary
        reader = directory / "reader ' λ.py"
        reader.write_text('''import json, os, signal, sys, time
from pathlib import Path
signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
Path(os.environ['XDG_CONFIG_HOME'], 'ready.json').write_text(json.dumps({
    'argv': sys.argv[1:], 'config': os.environ['XDG_CONFIG_HOME'], 'path': os.environ['PATH']}))
time.sleep(30)
''')
        source_probe = directory / 'wezterm-source.py'
        source_probe.write_text('''import json, os, signal, sys, time
from pathlib import Path
signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
Path(sys.argv[1]).write_text(json.dumps({
    'pane': os.environ.get('WEZTERM_PANE'),
    'socket': os.environ.get('WEZTERM_UNIX_SOCKET'),
    'program': os.environ.get('TERM_PROGRAM')}))
time.sleep(120)
''')
        if args.terminal == 'wezterm':
            if args.to:
                os.environ['WEZTERM_UNIX_SOCKET'] = args.to
                wezterm('list', '--format', 'json')  # Reject a missing socket, never find another GUI.
            else:
                isolated = directory / 'isolated.json'
                log = (directory / 'wezterm.log').open('wb')
                gui_env = os.environ.copy()
                gui_env.pop('WEZTERM_UNIX_SOCKET', None)
                gui_env.pop('WEZTERM_PANE', None)
                with log:
                    gui = subprocess.Popen(
                        ['wezterm', '-n', 'start', '--always-new-process',
                         '--cwd', temporary, '--', sys.executable,
                         str(source_probe), str(isolated)],
                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                        stderr=log, env=gui_env, start_new_session=True)

                def stop_gui():
                    if gui.poll() is None and sentinel is not None:
                        try:
                            wezterm('send-text', '--no-paste', '--pane-id', sentinel, '\x03')
                            gui.wait(timeout=3)
                        except (RuntimeError, subprocess.TimeoutExpired):
                            pass
                    if gui.poll() is None:
                        gui.terminate()
                        gui.wait(timeout=3)

                # Register before waiting, so startup failures cannot leave our GUI alive.
                sentinel = None
                cleanup.callback(stop_gui)

                def gui_ready():
                    if gui.poll() is not None:
                        raise RuntimeError('isolated WezTerm GUI exited: ' +
                                           (directory / 'wezterm.log').read_text())
                    return isolated.exists()

                wait(gui_ready, timeout=15)
                identity = json.loads(isolated.read_text())
                sentinel = identity['pane']
                socket_path = identity['socket']
                assert sentinel and socket_path, identity
                os.environ['WEZTERM_UNIX_SOCKET'] = socket_path
                assert sentinel in snapshot('wezterm')
        lua = directory / 'control.lua'
        lua.write_text('''vim.opt.runtimepath:prepend(vim.env.PDFTERM_ROOT)
local terminal = require('pdfterm.terminal')
local action, source = arg[1], {kind=vim.env.PDFTERM_TEST_KIND,id=arg[2]}
local done, result = false, nil
local function finish(value) result=value; done=true end
if action == 'capture' then
  terminal.capture_source(function(error, handle)
    finish({code=error and 1 or 0,stderr=error,stdout=handle and handle.id})
  end)
elseif action == 'launch' then
  terminal.launch_split(source,arg[3],arg[4],finish,arg[5]):wait()
  assert(done,'launch wait returned before ownership callback')
elseif action == 'focus' then
  terminal.focus(source,finish)
elseif action == 'close' then
  terminal.close(source); finish({code=0})
else error('unknown native test action') end
assert(vim.wait(5000,function() return done end,10),'terminal callback timed out')
if vim.env.PDFTERM_EXPECT_KITTY_TTY_FAILURE == '1' then
  assert(result.code ~= 0 and result.stderr:find('requires a listen_on socket',1,true),result.stderr)
else
  assert(result.code == 0,result.stderr)
end
io.write(vim.json.encode(result), '\\n')
''')

        def local(action, identifier, *argv, no_socket=False):
            environment = os.environ.copy()
            if no_socket:
                environment.pop('KITTY_LISTEN_ON', None)
                environment['PDFTERM_EXPECT_KITTY_TTY_FAILURE'] = '1'
            result = subprocess.run(['nvim', '--headless', '-u', 'NONE', '-i', 'NONE',
                                     '-l', str(lua), action, identifier, *argv],
                                    stdin=subprocess.DEVNULL, capture_output=True,
                                    text=True, timeout=8, start_new_session=True,
                                    env=environment)
            assert result.returncode == 0, result.stderr or result.stdout
            assert not result.stderr, result.stderr
            reply = json.loads(result.stdout)
            return reply['stderr'] if no_socket else reply.get('stdout', '').strip()

        for route in ('local', 'ssh'):
            if args.terminal == 'wezterm':
                sources = snapshot('wezterm')
                assert sources, 'the captured WezTerm GUI has no source pane'
                anchor = sentinel if not args.to else next(iter(sources))
                probe = directory / ('source-' + route + '.json')
                source = wezterm('spawn', '--pane-id', anchor, '--new-window',
                                 '--cwd', temporary, '--', sys.executable,
                                 str(source_probe), str(probe))

                def close_source(identifier):
                    if identifier in snapshot('wezterm'):
                        wezterm('send-text', '--no-paste', '--pane-id', identifier, '\x03')
                        wait(lambda: identifier not in snapshot('wezterm'))

                cleanup.callback(close_source, source)
                wait(probe.exists)
                identity = json.loads(probe.read_text())
                assert identity == {
                    'pane': source, 'socket': os.environ['WEZTERM_UNIX_SOCKET'],
                    'program': 'WezTerm'}, identity
                os.environ['WEZTERM_PANE'] = source
            else:
                source = (run(['kitten', '@', 'launch', '--type=os-window', '/bin/sleep', '60'])
                          if args.terminal == 'kitty' else applescript('''set cfg to new surface configuration
set command of cfg to "/bin/sleep 60"
set wait after command of cfg to false
set w to new window with configuration cfg
set s to focused terminal of selected tab of w
focus s
return id of s'''))
                if args.terminal == 'kitty':
                    os.environ['KITTY_WINDOW_ID'] = source
            viewer = None
            backend = None
            try:
                backend = launcher['capture_terminal']()
                if args.terminal == 'kitty':
                    run(['kitten', '@', 'set-enabled-layouts', '--match', 'window_id:' + source,
                         'tall', 'splits'])
                    run(['kitten', '@', 'goto-layout', '--match', 'window_id:' + source, 'tall'])
                assert backend.source == source
                local_source = local('capture', source)
                if args.terminal == 'wezterm':
                    assert json.loads(local_source) == {
                        'socket': os.environ['WEZTERM_UNIX_SOCKET'], 'pane': source}
                else:
                    assert local_source == source
                if args.terminal == 'wezterm':
                    before = snapshot('wezterm')
                    os.environ['WEZTERM_PANE'] = '999999999'
                    try:
                        try:
                            launcher['capture_terminal']()
                        except RuntimeError as error:
                            assert 'source pane is not on' in str(error), error
                        else:
                            raise AssertionError('foreign WezTerm source was accepted')
                    finally:
                        os.environ['WEZTERM_PANE'] = source
                    assert snapshot('wezterm') == before, 'invalid source changed WezTerm panes'
                if args.terminal == 'wezterm' and route == 'ssh':
                    before = set(snapshot('wezterm'))
                    restore = backend.focus

                    def fail_restore(identifier, deadline=None):
                        raise RuntimeError('simulated source focus failure')

                    backend.focus = fail_restore
                    try:
                        try:
                            backend.launch(['/bin/sleep', '30'])
                        except RuntimeError as error:
                            assert 'simulated source focus failure' in str(error), error
                        else:
                            raise AssertionError('source focus failure was ignored')
                    finally:
                        backend.focus = restore
                    wait(lambda: set(snapshot('wezterm')) == before)
                token = "session ' λ"
                if route == 'local' and args.terminal == 'kitty':
                    before = snapshot('kitty')
                    assert 'requires a listen_on socket' in local(
                        'launch', source, sys.executable, str(reader), token, no_socket=True)
                    assert snapshot('kitty') == before, 'failed control changed Kitty surfaces'
                    assert not (directory / 'ready.json').exists(), 'failed control launched a reader'
                    print('kitty/local: detached Neovim without socket rejected before split')
                viewer = (local('launch', local_source, sys.executable, str(reader), token)
                          if route == 'local' else backend.launch(
                              ['env', 'PATH=' + os.environ['PATH'],
                               'XDG_CONFIG_HOME=' + temporary, sys.executable,
                               str(reader), '--session', token]))
                viewer_handle = viewer
                if args.terminal == 'wezterm' and route == 'local':
                    identity = json.loads(viewer)
                    assert identity['socket'] == os.environ['WEZTERM_UNIX_SOCKET']
                    viewer = identity['pane']
                ready = directory / 'ready.json'
                wait(ready.exists)
                assert json.loads(ready.read_text()) == {
                    'argv': ['--session', token], 'config': temporary,
                    'path': os.environ['PATH']}
                ready.unlink()
                surfaces = snapshot(args.terminal)
                assert surfaces[source][:2] == surfaces[viewer][:2], surfaces
                if args.terminal == 'kitty':
                    assert surfaces[source][3] == 'splits'
                    assert int(viewer) in surfaces[source][2]['neighbors']['right']
                if args.terminal == 'wezterm':
                    assert wezterm('get-pane-direction', '--pane-id', source,
                                   'Right') == viewer, 'viewer is not right of source'
                assert focused(args.terminal, source)
                assert backend.existing({source, viewer}) == {source, viewer}
                if args.terminal == 'wezterm':
                    assert backend.existing({source, viewer, '999999999'}) == {source, viewer}
                backend.focus(viewer)
                assert focused(args.terminal, viewer)
                if route == 'local':
                    local('focus', local_source)
                else:
                    backend.focus(source)
                assert focused(args.terminal, source)
                for _ in range(2):
                    if route == 'local':
                        local('close', viewer_handle)
                    else:
                        backend.close(viewer)
                    wait(lambda: viewer not in snapshot(args.terminal))
                assert source in snapshot(args.terminal)
                assert backend.existing({viewer}) == set()
                focus_proof = 'pane activation' if args.terminal == 'wezterm' else 'focus'
                print(f'{args.terminal}/{route}: right split, argv/env, {focus_proof}, liveness, cleanup passed')
            finally:
                try:
                    if viewer is not None and backend is not None:
                        backend.close(viewer)
                finally:
                    if args.terminal == 'wezterm':
                        close_source(source)
                    else:
                        backend.close(source)
                    wait(lambda: source not in snapshot(args.terminal))


if __name__ == '__main__':
    main()
