-- Terminal control, not graphics: Kitty, Ghostty, and WezTerm render Kitty protocol.
-- Handles identify exact surfaces; closing a handle gracefully quits its reader.
local platform = require('pdfterm.platform')
local ghostty_control = require('pdfterm.ghostty')
local M = {}
local ghostty, neovide = {}, {}
local adapters = {
  kitty = require('pdfterm.kitty'),
  ghostty = ghostty,
  wezterm = require('pdfterm.wezterm'),
  ssh = require('pdfterm.ssh'),
  neovide = neovide,
}

local split_script = [[
on run argv
  tell application "Ghostty"
    set sourceTerminal to terminal id (item 2 of argv)
    set cfg to new surface configuration
    set command of cfg to item 1 of argv
    set wait after command of cfg to false
    set environment variables of cfg to {"PATH=" & (item 3 of argv), "XDG_CONFIG_HOME=" & (item 4 of argv)}
    set viewer to split sourceTerminal direction right with configuration cfg
    focus sourceTerminal
    return id of viewer
  end tell
end run
]]

function ghostty.launch(source, argv, callback)
  local command = table.concat(vim.tbl_map(vim.fn.shellescape, argv), ' ')
  return platform.applescript(
    split_script,
    { command, source.id, vim.env.PATH, vim.env.XDG_CONFIG_HOME or '' },
    callback
  )
end

function ghostty.focus(source, callback)
  return ghostty_control.request('focus', source.id, callback)
end

function ghostty.close(split)
  local result = platform
    .applescript(
      [[
on run argv
  tell application "Ghostty"
    repeat 20 times
      if not (exists terminal id (item 1 of argv)) then return
      -- Ctrl-C quits the reader; another key dismisses Ghostty's retained exit screen.
      send key "c" modifiers "control" to terminal id (item 1 of argv)
      delay 0.1
    end repeat
    if exists terminal id (item 1 of argv) then error "owned PDF split did not exit"
  end tell
end run
]],
      { split.id }
    )
    :wait()
  if result.code ~= 0 then
    error('pdfterm: could not close owned PDF split: ' .. (result.stderr or ''))
  end
end

-- Neovide cannot render Kitty graphics in :terminal. Give its viewer a real
-- Ghostty surface, without treating an inherited terminal ID as the GUI.
function neovide.capture(callback)
  callback(nil, tostring(vim.fn.getpid()))
end

function neovide.launch(_, argv, callback)
  return platform.applescript(
    [[
on run argv
  tell application "Ghostty"
    set cfg to new surface configuration
    set command of cfg to item 1 of argv
    set wait after command of cfg to false
    set environment variables of cfg to {"PATH=" & (item 2 of argv), "XDG_CONFIG_HOME=" & (item 3 of argv)}
    set viewerWindow to new window with configuration cfg
    return id of focused terminal of selected tab of viewerWindow
  end tell
end run
]],
    {
      table.concat(vim.tbl_map(vim.fn.shellescape, argv), ' '),
      vim.env.PATH,
      vim.env.XDG_CONFIG_HOME or '',
    },
    vim.schedule_wrap(function(result)
      local ok, failure = xpcall(callback, debug.traceback, result)
      if not ok then
        -- The OS window exists before its handle is recorded. Roll back only
        -- that exact surface if the ownership callback fails.
        if result.code == 0 and result.stdout and vim.trim(result.stdout) ~= '' then
          local closed, close_error = pcall(ghostty.close, { id = vim.trim(result.stdout) })
          if not closed then
            error(failure .. '; could not close unowned Ghostty window: ' .. tostring(close_error))
          end
        end
        error(failure)
      end
    end)
  )
end

function neovide.focus(_, callback)
  callback({ code = 1, stderr = 'Neovide source focus is unavailable; disable focus_on_inverse' })
end

function neovide.close()
  error('pdfterm: a Neovide source is not an owned viewer')
end

local function adapter(handle)
  local result = adapters[handle.kind]
  if not result or type(handle.id) ~= 'string' or handle.id == '' then
    error('pdfterm: invalid terminal handle')
  end
  return result
end

function ghostty.capture(callback)
  return ghostty_control.request(
    'capture',
    nil,
    vim.schedule_wrap(function(result)
      callback(result.code ~= 0 and result.stderr or nil, vim.trim(result.stdout or ''))
    end)
  )
end

-- A backend implements the entire contract; session/argv/handle policy stays here.
for name, backend in pairs(adapters) do
  for _, action in ipairs({ 'capture', 'launch', 'focus', 'close' }) do
    assert(type(backend[action]) == 'function', name .. ' terminal lacks ' .. action)
  end
end

function M.capture_source(callback)
  local kind = (vim.env.PDFTERM_LAUNCH_SOCKET or '') ~= '' and 'ssh'
    or vim.g.neovide and 'neovide'
    or vim.env.TERM_PROGRAM == 'WezTerm' and 'wezterm'
    or vim.env.TERM_PROGRAM == 'ghostty' and 'ghostty'
    or vim.env.KITTY_WINDOW_ID and 'kitty'
  if not kind then
    callback(
      'automatic split requires Kitty, Ghostty, or WezTerm control; use :PdfTermViewerCommand to launch in another Kitty-graphics-compatible terminal'
    )
    return
  end
  local ok, error = pcall(adapters[kind].capture, function(problem, id)
    if problem or not id or id == '' then
      callback(problem or ('could not identify source ' .. kind .. ' terminal'))
    else
      callback(nil, { kind = kind, id = id })
    end
  end)
  if not ok then
    callback(tostring(error))
  end
end

-- Callback runs before scheduling editor work so VimLeavePre can retain ownership
-- even when it is waiting for an in-flight launch to finish.
function M.launch_split(source, executable, pdf, callback, session, focus_token)
  local argv = { executable, pdf }
  if session then
    vim.list_extend(argv, { '--session', session })
  end
  if focus_token then
    vim.list_extend(argv, { '--focus-token', focus_token })
  end
  local done, reply, callback_failure = false, nil, nil
  adapter(source).launch(source, argv, function(result)
    local ok, failure = xpcall(function()
      local id = vim.trim(result.stdout or '')
      local kind = source.kind == 'neovide' and 'ghostty' or source.kind
      -- Failed rollback still transfers the exact provisional handle for exit cleanup.
      callback(result, (result.code == 0 or result.unclosed) and id ~= '' and { kind = kind, id = id } or nil)
    end, debug.traceback)
    done, reply = true, result
    if not ok then
      -- wait() must not report transport success when ownership recording failed.
      -- Rethrow as well: backends with provisional handles roll them back.
      callback_failure = failure
      error(failure)
    end
  end)
  return {
    wait = function(_, timeout)
      if not vim.wait(timeout or 8500, function()
        return done
      end, 10) then
        error('pdfterm: terminal launch timed out')
      end
      if callback_failure then
        error(callback_failure)
      end
      return reply
    end,
  }
end

function M.focus(source, callback)
  return adapter(source).focus(source, callback)
end

function M.close(split)
  return adapter(split).close(split)
end

return M
