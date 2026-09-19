-- OS-specific editor operations. Unix sockets and terminal escape sequences do
-- not belong here: their existing Unix/protocol implementations are shared.
local M = {}

function M.check_supported()
  local system = vim.uv.os_uname().sysname
  if system ~= 'Darwin' and system ~= 'Linux' then
    error('pdfterm: unsupported OS ' .. system .. '; expected macOS or Linux')
  end
end

local function require_macos(feature)
  if vim.uv.os_uname().sysname ~= 'Darwin' then
    error('pdfterm: ' .. feature .. ' requires macOS')
  end
end

function M.applescript(script, arguments, callback)
  require_macos 'AppleScript terminal control'
  local command = { 'osascript', '-e', script }
  vim.list_extend(command, arguments or {})
  return vim.system(command, { text = true, timeout = 3000 }, callback)
end

function M.skim_forward(line, pdf, source)
  require_macos 'Skim'
  local job = vim.fn.jobstart({
    '/Applications/Skim.app/Contents/SharedSupport/displayline',
    '-g',
    '-r',
    tostring(line),
    pdf,
    source,
  }, { detach = true })
  if job == 0 or job == -1 then
    error 'pdfterm: failed to open Skim with displayline'
  end
end

return M
