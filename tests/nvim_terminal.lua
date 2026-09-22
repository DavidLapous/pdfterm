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
local terminal = require('pdfterm.terminal')
local source = { kind = 'ssh', id = 'source' }
local owned, count = nil, 0
local successful = terminal.launch_split(source, 'viewer', 'paper.pdf', function(result, split)
  count = count + 1
  assert(result.code == 0)
  owned = assert(split)
end, 'session')
assert(successful:wait(1000).code == 0 and count == 1 and owned.id == 'viewer-7')
assert(requests[1].argv[3] == '--session' and requests[1].argv[4] == 'session')
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
print('terminal regressions passed: success, callback failure with exact-handle cleanup, repeat wait, transport rejection')
