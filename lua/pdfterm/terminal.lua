-- Terminal control, not graphics: Kitty and Ghostty both render Kitty protocol.
-- Handles identify exact surfaces; closing a handle gracefully quits its reader.
local platform = require('pdfterm.platform')
local ghostty_control = require('pdfterm.ghostty')
local M = {}
local kitty, ghostty = {}, {}
local adapters = { kitty = kitty, ghostty = ghostty, ssh = require('pdfterm.ssh') }

local function remote(arguments, callback)
  local command = { 'kitten', '@' }
  vim.list_extend(command, arguments)
  return vim.system(command, { text = true, timeout = 3000 }, callback)
end

function kitty.launch(source, argv, callback)
  local arguments = {
    'launch',
    '--match',
    'window_id:' .. source.id,
    '--type=window',
    '--location=vsplit',
    '--keep-focus',
    '--next-to',
    'id:' .. source.id,
    '--env',
    'PATH=' .. vim.env.PATH,
    '--env',
    'XDG_CONFIG_HOME=' .. (vim.env.XDG_CONFIG_HOME or ''),
  }
  vim.list_extend(arguments, argv)
  return remote(
    { 'goto-layout', '--match', 'window_id:' .. source.id, 'splits' },
    vim.schedule_wrap(function(result)
      if result.code ~= 0 then
        callback(result)
        return
      end
      remote(arguments, callback)
    end)
  )
end

function kitty.focus(source, callback)
  return remote({ 'focus-window', '--match', 'id:' .. source.id }, callback)
end

function kitty.close(split)
  local found = remote({ 'ls' }):wait()
  if found.code ~= 0 then
    error('pdfterm: could not locate PDF split: ' .. (found.stderr or ''))
  end
  for _, os_window in ipairs(vim.json.decode(found.stdout)) do
    for _, tab in ipairs(os_window.tabs) do
      for _, window in ipairs(tab.windows) do
        if tostring(window.id) == split.id then
          local result = remote({ 'send-text', '--match', 'id:' .. split.id, '\003' }):wait()
          if result.code ~= 0 then
            error('pdfterm: could not quit owned PDF split: ' .. (result.stderr or ''))
          end
          return
        end
      end
    end
  end
end

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

local function adapter(handle)
  local result = adapters[handle.kind]
  if not result or type(handle.id) ~= 'string' or handle.id == '' then
    error('pdfterm: invalid terminal handle')
  end
  return result
end

function kitty.capture(callback)
  callback(nil, vim.env.KITTY_WINDOW_ID)
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
    or vim.env.KITTY_WINDOW_ID and 'kitty'
    or vim.env.TERM_PROGRAM == 'ghostty' and 'ghostty'
  if not kind then
    callback('terminal launch/focus requires Kitty or Ghostty')
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
function M.launch_split(source, executable, pdf, callback, session)
  local argv = { executable, pdf }
  if session then
    vim.list_extend(argv, { '--session', session })
  end
  local done, reply = false, nil
  adapter(source).launch(source, argv, function(result)
    done, reply = true, result
    local id = vim.trim(result.stdout)
    callback(result, result.code == 0 and id ~= '' and { kind = source.kind, id = id } or nil)
  end)
  return {
    wait = function(_, timeout)
      if not vim.wait(timeout or 8500, function()
        return done
      end, 10) then
        error('pdfterm: terminal launch timed out')
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
