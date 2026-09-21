-- Socket transport does not require terminal/window control.
local M = {}

function M.forward(path, payload, callback)
  local pipe = assert(vim.uv.new_pipe(false))
  local timer = assert(vim.uv.new_timer())
  local chunks, size, done = {}, 0, false
  local function finish(error, connection_error)
    if done then return end
    done = true
    timer:stop(); timer:close()
    if not pipe:is_closing() then pipe:close() end
    vim.schedule(function() callback(error, connection_error) end)
  end
  timer:start(31000, 0, function() finish('forward connection/reply timed out') end)
  pipe:connect(path, function(error)
    if done then return end
    if error then finish(error, true); return end
    pipe:read_start(function(read_error, chunk)
      if done then return end
      if read_error then finish(read_error)
      elseif chunk then
        size = size + #chunk
        if size > 4096 then finish('forward reply exceeds 4096 bytes')
        else chunks[#chunks + 1] = chunk end
      else
        local ok, reply = pcall(vim.json.decode, table.concat(chunks))
        if not ok or type(reply) ~= 'table' or reply.ok ~= true then
          finish(ok and type(reply) == 'table' and reply.error or 'forward request rejected or invalid reply')
        else finish() end
      end
    end)
    pipe:write(payload, function(write_error)
      if done then return end
      if write_error then finish(write_error); return end
      pipe:shutdown(function(shutdown_error) if shutdown_error then finish(shutdown_error) end end)
    end)
  end)
  return function() finish('forward request cancelled') end
end

function M.listen(path, on_location)
  local parent = assert(vim.uv.fs_lstat(vim.fs.dirname(path)))
  assert(parent.type == 'directory' and parent.uid == vim.uv.getuid() and bit.band(parent.mode, 63) == 0,
    'pdfterm: socket parent must be a current-user-owned mode-0700 directory')
  local server = assert(vim.uv.new_pipe(false))
  local ok, bind_error = server:bind(path)
  if not ok then server:close(); error('pdfterm: cannot bind ' .. path .. ': ' .. tostring(bind_error) .. '; stop the existing editor or explicitly remove its stale socket') end
  local identity = assert(vim.uv.fs_lstat(path))
  local clients, count = {}, 0
  local function close()
    for finish in pairs(clients) do finish('editor stopped') end
    if not server:is_closing() then server:close() end
    local current = vim.uv.fs_lstat(path)
    if current and current.dev == identity.dev and current.ino == identity.ino then
      assert(vim.uv.fs_unlink(path))
    end
  end
  local secured, chmod_error = vim.uv.fs_chmod(path, 384)
  if not secured then close(); error('pdfterm: cannot secure socket: ' .. tostring(chmod_error)) end
  server:listen(16, function(listen_error)
    if listen_error then vim.schedule(function() vim.notify('pdfterm: ' .. listen_error, vim.log.levels.ERROR) end); return end
    local client = assert(vim.uv.new_pipe(false))
    server:accept(client)
    if count >= 16 then client:close(); return end
    count = count + 1
    local timer = assert(vim.uv.new_timer())
    local chunks, size = {}, 0
    local finish
    finish = function(message, deliver)
      if client:is_closing() then return end
      clients[finish] = nil; count = count - 1
      timer:stop(); timer:close(); client:close()
      vim.schedule(function()
        if message then vim.notify('pdfterm inverse search: ' .. message, vim.log.levels.ERROR)
        elseif deliver then
          local decoded, location = pcall(vim.json.decode, table.concat(chunks))
          if not decoded or type(location) ~= 'table' or type(location.file) ~= 'string'
              or location.file:sub(1, 1) ~= '/' or location.file:find('%z')
              or type(location.line) ~= 'number' or location.line < 1 or location.line % 1 ~= 0
              or type(location.byte_column) ~= 'number' or location.byte_column < 0 or location.byte_column % 1 ~= 0 then
            vim.notify('pdfterm: invalid inverse-search JSON location', vim.log.levels.ERROR)
          else on_location(location) end
        end
      end)
    end
    clients[finish] = true
    timer:start(1000, 0, function() finish('request timed out') end)
    client:read_start(function(read_error, chunk)
      if read_error then finish(read_error)
      elseif not chunk then finish(nil, true)
      else
        size = size + #chunk
        if size > 16384 then finish('request exceeds 16384 bytes') else chunks[#chunks + 1] = chunk end
      end
    end)
  end)
  return close
end
return M
