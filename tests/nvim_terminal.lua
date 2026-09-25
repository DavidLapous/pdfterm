-- Run: nvim --headless -u NONE -l tests/nvim_terminal.lua
-- Exercise the real terminal wrapper and SSH adapter; only socket/GUI I/O is mocked.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local function unexpected()
  error('this regression must not invoke a native terminal')
end
package.loaded['pdfterm.platform'] = { applescript = unexpected }
package.loaded['pdfterm.ghostty'] = { request = unexpected }
package.loaded['pdfterm.kitty'] = {
  capture = unexpected,
  launch = unexpected,
  focus = unexpected,
  close = unexpected,
}
local requests, launch_error = {}, nil
package.loaded['pdfterm.socket'] = {
  request = function(_, payload, callback, _, acknowledge)
    local request = vim.json.decode(payload)
    requests[#requests + 1] = request
    assert(request.action == 'launch' or request.action == 'close')
    local problem = request.action == 'launch' and launch_error or nil
    vim.schedule(function()
      if problem then
        callback(problem)
      elseif request.action == 'launch' then
        assert(acknowledge)
        callback(nil, nil, { ok = true, id = 'viewer-7', confirmed = true })
      else
        assert(request.id == 'viewer-7', 'cleanup must target the exact offered handle')
        callback(nil, nil, { ok = true })
      end
    end)
    return unexpected -- No cancellation is needed for immediate mocked replies.
  end,
}
-- Capture expected scheduled exceptions so headless stderr stays clean.
local schedule, failures = vim.schedule, {}
vim.schedule = function(callback)
  schedule(function()
    local ok, failure = xpcall(callback, debug.traceback)
    if not ok then
      failures[#failures + 1] = failure
    end
  end)
end
local token_file = vim.fn.getcwd() .. '/target/pdfterm-terminal-token-' .. vim.fn.getpid()
local token = string.rep('a', 64)
vim.fn.writefile({ token }, token_file)
vim.env.PDFTERM_LAUNCH_TOKEN_FILE = token_file
local terminal = require('pdfterm.terminal')
local source = { kind = 'ssh', id = 'source' }
local owned, count = nil, 0
local successful = terminal.launch_split(source, 'viewer', 'paper.pdf', function(result, split)
  count = count + 1
  assert(result.code == 0)
  owned = assert(split)
end, 'session', string.rep('f', 32))
assert(successful:wait(1000).code == 0 and count == 1 and owned.id == 'viewer-7')
assert(requests[1].argv[3] == '--session' and requests[1].argv[4] == 'session')
assert(requests[1].argv[5] == '--focus-token' and requests[1].argv[6] == string.rep('f', 32))
assert(requests[1].token == token, 'SSH launch must authenticate the exact source session')
terminal.close(owned)
assert(#requests == 2 and #failures == 0)

requests, count = {}, 0
local failed = terminal.launch_split(source, 'viewer', 'paper.pdf', function(result, split)
  count = count + 1
  assert(result.code == 0 and split.id == 'viewer-7')
  error('deliberate ownership callback failure')
end)
local ok, failure = pcall(failed.wait, failed, 1000)
assert(not ok, 'terminal launch wait hid an ownership callback failure')
assert(tostring(failure):find('deliberate ownership callback failure', 1, true), tostring(failure))
assert(count == 1 and #failures == 1, 'callback must fail exactly once')
assert(#requests == 2 and requests[2].action == 'close' and requests[2].id == 'viewer-7')
-- A later wait must not convert a recorded failure back to transport success.
ok, failure = pcall(failed.wait, failed, 10)
assert(not ok and tostring(failure):find('deliberate ownership callback failure', 1, true))

requests, failures, count, launch_error = {}, {}, 0, 'deliberate transport rejection'
local rejected = terminal.launch_split(source, 'viewer', 'paper.pdf', function(result, split)
  count = count + 1
  assert(result.code ~= 0 and split == nil and result.stderr == launch_error)
end)
assert(rejected:wait(1000).code ~= 0 and count == 1)
assert(#requests == 1 and #failures == 0, 'no offered handle means no invented cleanup')
vim.schedule = schedule
vim.fn.delete(token_file)
vim.env.PDFTERM_LAUNCH_TOKEN_FILE = nil
local capture_error, captured
require('pdfterm.ssh').capture(function(problem, id)
  capture_error, captured = problem, id
end)
assert(
  capture_error and capture_error:find('missing PDFTERM_LAUNCH_TOKEN_FILE', 1, true)
    and captured == nil,
  'an incomplete SSH bridge must not claim a source terminal'
)
local focus_result
vim.schedule(function()
  terminal.focus(source, vim.schedule_wrap(function(result)
    focus_result = result
  end))
end)
assert(
  vim.wait(1000, function()
    return focus_result ~= nil
  end, 10),
  'inverse focus must report the missing token without throwing in a scheduled callback'
)
assert(
  focus_result.code ~= 0
    and focus_result.stderr:find('missing PDFTERM_LAUNCH_TOKEN_FILE', 1, true)
)
assert(#requests == 1, 'missing authentication must not contact the bridge')

-- Neovide is a GUI even when its Neovim child inherits Kitty/Ghostty variables.
vim.g.neovide = true
vim.env.KITTY_WINDOW_ID, vim.env.TERM_PROGRAM = '999', 'ghostty'
local gui_source
terminal.capture_source(function(problem, handle)
  assert(not problem)
  gui_source = handle
end)
assert(gui_source.kind == 'neovide' and gui_source.id == tostring(vim.fn.getpid()))
local scripts = {}
package.loaded['pdfterm.platform'].applescript = function(script, argv, callback)
  scripts[#scripts + 1] = { script = script, argv = argv }
  if callback then
    callback({ code = 0, stdout = 'ghostty-viewer-7\n', stderr = '' })
    return
  end
  return { wait = function()
    return { code = 0 }
  end }
end
local gui_viewer
local gui_launch = terminal.launch_split(
  gui_source,
  '/bin/viewer with space',
  "paper's page.pdf",
  function(result, handle)
    assert(result.code == 0)
    gui_viewer = handle
  end,
  'session',
  string.rep('f', 32)
)
assert(gui_launch:wait(1000).code == 0)
assert(gui_viewer.kind == 'ghostty' and gui_viewer.id == 'ghostty-viewer-7')
assert(scripts[1].script:find('new window with configuration cfg', 1, true))
assert(scripts[1].argv[1] == table.concat(vim.tbl_map(vim.fn.shellescape, {
  '/bin/viewer with space',
  "paper's page.pdf",
  '--session',
  'session',
  '--focus-token',
  string.rep('f', 32),
}), ' '))
local source_focus
terminal.focus(gui_source, function(result)
  source_focus = result
end)
assert(source_focus.code ~= 0 and source_focus.stderr:find('Neovide source focus is unavailable'))
package.loaded['pdfterm.ghostty'].request = function(action, id, callback)
  assert(action == 'focus' and id == 'ghostty-viewer-7')
  callback({ code = 0 })
end
terminal.focus(gui_viewer, function(result)
  assert(result.code == 0)
end)
terminal.close(gui_viewer)
assert(scripts[2].script:find('terminal id', 1, true) and scripts[2].argv[1] == gui_viewer.id)
scripts, failures = {}, {}
vim.schedule = function(callback)
  schedule(function()
    local ok, failure = xpcall(callback, debug.traceback)
    if not ok then
      failures[#failures + 1] = failure
    end
  end)
end
local unowned = terminal.launch_split(gui_source, 'viewer', 'paper.pdf', function()
  error('deliberate GUI ownership failure')
end)
local owned_ok, owned_error = pcall(unowned.wait, unowned, 1000)
assert(not owned_ok and tostring(owned_error):find('deliberate GUI ownership failure', 1, true))
assert(#scripts == 2 and scripts[2].argv[1] == 'ghostty-viewer-7')
assert(#failures == 1 and failures[1]:find('deliberate GUI ownership failure', 1, true))
vim.schedule = schedule
vim.g.neovide, vim.env.KITTY_WINDOW_ID, vim.env.TERM_PROGRAM = nil, nil, nil

-- A WezTerm pane ID alone is not sufficient: all operations use the captured
-- GUI socket and an explicit target even if Neovim's environment later changes.
local original_system = vim.system
local wez_calls, wez_live, wez_focused, wez_fail_focus, wez_fail_rollback =
  {}, { ['11'] = true }, '11', false, false
vim.system = function(argv, options, callback)
  assert(argv[1] == 'wezterm' and argv[2] == 'cli' and argv[3] == '--no-auto-start')
  assert(options.env.WEZTERM_UNIX_SOCKET == '/tmp/pdfterm-owned-wezterm.sock')
  wez_calls[#wez_calls + 1] = argv
  local action, result = argv[4], { code = 0, stdout = '', stderr = '' }
  if action == 'list' then
    local rows = {}
    for pane in pairs(wez_live) do
      rows[#rows + 1] = { pane_id = tonumber(pane), window_id = 1, tab_id = 1 }
    end
    result.stdout = vim.json.encode(rows)
  elseif action == 'split-pane' then
    assert(argv[5] == '--pane-id' and argv[6] == '11' and argv[7] == '--right')
    assert(argv[8] == '--cwd' and argv[9] == vim.fn.getcwd() and argv[10] == '--')
    assert(argv[11] == 'env' and argv[12] == 'PATH=' .. vim.env.PATH)
    assert(argv[13] == 'XDG_CONFIG_HOME=' .. (vim.env.XDG_CONFIG_HOME or ''))
    assert(wez_live['11'] and not wez_live['19'])
    wez_live['19'], wez_focused, result.stdout = true, '19', '19\n'
  elseif action == 'activate-pane' then
    assert(argv[5] == '--pane-id' and (argv[6] == '11' or argv[6] == '19'))
    if wez_fail_focus and argv[6] == '11' then
      result.code, result.stderr = 1, 'refocus rejected'
    else
      assert(wez_live[argv[6]])
      wez_focused = argv[6]
    end
  elseif action == 'send-text' then
    assert(argv[5] == '--no-paste' and argv[6] == '--pane-id')
    assert(argv[7] == '19' and argv[8] == '\003')
    assert(wez_live['19'], 'must not send Ctrl-C to an exited pane')
    wez_live['19'] = nil
  elseif action == 'kill-pane' then
    assert(argv[5] == '--pane-id' and argv[6] == '19' and wez_live['19'])
    if wez_fail_rollback then
      result.code, result.stderr = 1, 'rollback rejected'
    else
      wez_live['19'] = nil
    end
  else
    error('unexpected WezTerm action: ' .. action)
  end
  if callback then
    vim.schedule(function()
      callback(result)
    end)
  end
  return { wait = function()
    return result
  end }
end
vim.env.TERM_PROGRAM = 'WezTerm'
vim.env.WEZTERM_PANE = '11'
vim.env.WEZTERM_UNIX_SOCKET = '/tmp/pdfterm-owned-wezterm.sock'
local wez_source, wez_capture_error
terminal.capture_source(function(problem, handle)
  wez_capture_error, wez_source = problem, handle
end)
assert(vim.wait(1000, function()
  return wez_source ~= nil
end, 10), wez_capture_error)
assert(wez_source.kind == 'wezterm' and vim.json.decode(wez_source.id).pane == '11')
vim.env.WEZTERM_UNIX_SOCKET = '/tmp/another-wezterm.sock'
local wez_split
local wez_launch = terminal.launch_split(wez_source, 'viewer', 'paper.pdf', function(result, handle)
  assert(result.code == 0)
  wez_split = assert(handle)
end)
assert(wez_launch:wait(1000).code == 0)
assert(vim.json.decode(wez_split.id).pane == '19' and wez_focused == '11')
terminal.focus(wez_split, function(result)
  assert(result.code == 0)
end)
assert(vim.wait(1000, function()
  return wez_focused == '19'
end, 10))
terminal.close(wez_split)
assert(not wez_live['19'] and wez_live['11'])
local closed_at = #wez_calls
terminal.close(wez_split)
assert(#wez_calls == closed_at + 1, 'a retired pane is not signalled again')

wez_fail_focus = true
local refused
local failed_focus = terminal.launch_split(wez_source, 'viewer', 'paper.pdf', function(result, handle)
  assert(result.code ~= 0 and handle == nil)
  refused = result.stderr
end)
failed_focus:wait(1000)
assert(refused and not wez_live['19'])
assert(wez_calls[#wez_calls][4] == 'kill-pane', 'failed launch must roll back its exact split')
wez_fail_rollback = true
local provisional, rejected_result
local stranded = terminal.launch_split(wez_source, 'viewer', 'paper.pdf', function(result, handle)
  provisional, rejected_result = handle, result
end)
assert(stranded:wait(1000).code ~= 0)
assert(rejected_result.unclosed and provisional and wez_live['19'])
assert(vim.json.decode(provisional.id).pane == '19', 'failed cleanup must retain its exact pane')
wez_fail_rollback, wez_fail_focus = false, false
terminal.close(provisional)
assert(not wez_live['19'] and wez_live['11'])
wez_live['11'] = nil
local stale
terminal.launch_split(wez_source, 'viewer', 'paper.pdf', function(result, handle)
  stale = result.stderr
  assert(result.code ~= 0 and not handle)
end):wait(1000)
assert(stale and not wez_live['19'])
assert(wez_calls[#wez_calls][4] == 'list', 'stale source must not split another pane')
local queried = #wez_calls
vim.env.WEZTERM_UNIX_SOCKET = nil
terminal.capture_source(function(problem, handle)
  assert(not handle)
  assert(problem)
end)
assert(#wez_calls == queried, 'missing instance identity must fail before CLI control')
vim.system = original_system
vim.env.WEZTERM_PANE, vim.env.TERM_PROGRAM = nil, nil
print('WezTerm exact socket/pane control, refocus rollback, and owned cleanup passed')
local generic_error
terminal.capture_source(function(problem, handle)
  generic_error = problem
  assert(handle == nil)
end)
assert(generic_error, 'unsupported terminals must not claim automatic split control')
print('Neovide opens and owns a Ghostty window, not a terminal buffer')
print('terminal regressions passed: success, callback failure with exact-handle cleanup, repeat wait, transport rejection')
