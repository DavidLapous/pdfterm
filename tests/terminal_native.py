#!/usr/bin/env python3
"""Opt-in native contract: local Lua and SSH backends, using only owned surfaces.

Run with --terminal kitty --to unix:/path/to/kitty.sock, or --terminal ghostty.
Requires a running terminal and Neovim; never substitutes mocked terminal effects.
"""
import argparse
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


def wait(predicate):
    deadline = time.monotonic() + 5
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError('native terminal transition timed out')
        time.sleep(0.02)


def snapshot(kind):
    if kind == 'kitty':
        return {str(w['id']): (str(osw['id']), str(tab['id']), w, tab['layout'])
                for osw in json.loads(run(['kitten', '@', 'ls']))
                for tab in osw['tabs'] for w in tab['windows']}
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
    return applescript('return id of focused terminal of selected tab of front window') == identifier


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--terminal', choices=['kitty', 'ghostty'], required=True)
    parser.add_argument('--to', help='Kitty remote-control socket address')
    args = parser.parse_args()
    if args.terminal == 'kitty':
        if not args.to:
            parser.error('Kitty requires an explicit owned test instance via --to')
        os.environ['KITTY_LISTEN_ON'] = args.to
    else:
        os.environ.pop('KITTY_WINDOW_ID', None)
        os.environ['TERM_PROGRAM'] = 'ghostty'
    os.environ.pop('PDFTERM_LAUNCH_SOCKET', None)
    os.environ['PDFTERM_ROOT'] = str(ROOT)
    os.environ['PDFTERM_TEST_KIND'] = args.terminal
    with tempfile.TemporaryDirectory(prefix=".terminal-test-' ", dir=ROOT) as temporary:
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
assert(result.code == 0,result.stderr)
io.write(vim.json.encode(result), '\\n')
''')

        def local(action, identifier, *argv):
            result = subprocess.run(['nvim', '--headless', '-u', 'NONE', '-i', 'NONE',
                                     '-l', str(lua), action, identifier, *argv],
                                    stdin=subprocess.DEVNULL, capture_output=True,
                                    text=True, timeout=8)
            assert result.returncode == 0, result.stderr or result.stdout
            assert not result.stderr, result.stderr
            return json.loads(result.stdout).get('stdout', '').strip()

        for route in ('local', 'ssh'):
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
            backend = launcher['capture_terminal']()
            viewer = None
            try:
                if args.terminal == 'kitty':
                    run(['kitten', '@', 'set-enabled-layouts', '--match', 'window_id:' + source,
                         'tall', 'splits'])
                    run(['kitten', '@', 'goto-layout', '--match', 'window_id:' + source, 'tall'])
                assert backend.source == source
                assert local('capture', source) == source
                token = "session ' λ"
                viewer = (local('launch', source, sys.executable, str(reader), token)
                          if route == 'local' else backend.launch(
                              ['env', 'PATH=' + os.environ['PATH'],
                               'XDG_CONFIG_HOME=' + temporary, sys.executable,
                               str(reader), '--session', token]))
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
                assert focused(args.terminal, source)
                assert backend.existing({source, viewer}) == {source, viewer}
                backend.focus(viewer)
                assert focused(args.terminal, viewer)
                if route == 'local':
                    local('focus', source)
                else:
                    backend.focus(source)
                assert focused(args.terminal, source)
                for _ in range(2):
                    if route == 'local':
                        local('close', viewer)
                    else:
                        backend.close(viewer)
                    wait(lambda: viewer not in snapshot(args.terminal))
                assert source in snapshot(args.terminal)
                assert backend.existing({viewer}) == set()
                print(f'{args.terminal}/{route}: right split, argv/env, focus, liveness, cleanup passed')
            finally:
                try:
                    if viewer is not None:
                        backend.close(viewer)
                finally:
                    backend.close(source)
                    wait(lambda: source not in snapshot(args.terminal))


if __name__ == '__main__':
    main()
