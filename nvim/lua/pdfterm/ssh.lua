-- The client-side pdfterm-ssh wrapper owns windows; this side owns only requests.
local socket = require('pdfterm.socket')
local M = {}

local function request(payload, callback)
  local done, result = false, nil
  local cancel = socket.request(vim.env.PDFTERM_LAUNCH_SOCKET, vim.json.encode(payload), function(error, _, reply)
    result = { code = error and 1 or 0, stdout = reply and reply.id or '', stderr = error or '' }
    done = true
    callback(result)
  end, 3000)
  return {
    wait = function(_, timeout)
      if not vim.wait(timeout or 3500, function()
        return done
      end, 10) then
        cancel()
        error('pdfterm: client terminal control timed out')
      end
      return result
    end,
  }
end

function M.launch(_, executable, pdf, callback, session)
  local argv = { executable, pdf }
  if session then
    vim.list_extend(argv, { '--session', session })
  end
  return request(
    { action = 'launch', argv = argv, path = vim.env.PATH, config_home = vim.env.XDG_CONFIG_HOME },
    callback
  )
end

function M.focus(source, callback)
  return request({ action = 'focus', id = source.id }, callback)
end

function M.close(split)
  local result = request({ action = 'close', id = split.id }, function() end):wait()
  if result.code ~= 0 then
    error('pdfterm: could not close client viewer: ' .. result.stderr)
  end
end

return M
