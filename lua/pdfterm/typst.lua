-- Tinymist's preview uses Typst source spans, not PDF text matching. Its LSP
-- scrollPreview command delivers document positions over the local data plane.
local M = {}
local active = {}

local function canonical(path, cwd)
  local absolute = vim.fs.normalize(vim.startswith(path, '/') and path or cwd .. '/' .. path)
  return vim.uv.fs_realpath(absolute) or absolute
end

local function compile_args(project)
  local argv = project.build
  local program = type(argv) == 'table' and vim.fs.basename(argv[1] or '') or ''
  assert((program == 'typst' or program == 'tinymist') and argv[2] == 'compile',
    'Typst forward search requires a typst/tinymist compile command, not a custom build wrapper')
  local values = {
    ['--root'] = true, ['--input'] = true, ['--font-path'] = true,
    ['--package-path'] = true, ['--package-cache-path'] = true,
    ['--creation-timestamp'] = true, ['--pdf-standard'] = true,
  }
  local switches = { ['--ignore-system-fonts'] = true, ['--no-pdf-tags'] = true }
  local args, positional, index, root = {}, {}, 3, nil
  while index <= #argv do
    local arg = argv[index]
    local flag, value = arg:match('^(%-%-[^=]+)=(.*)$')
    flag = flag or arg
    if values[flag] or flag == '--format' or flag == '-f' then
      if not value then
        index = index + 1
        value = assert(argv[index], 'missing value for ' .. flag)
      end
      if flag == '--format' or flag == '-f' then
        assert(value == 'pdf', 'Typst forward search requires PDF output')
      else
        if flag == '--root' then
          value = canonical(value, project.cwd)
          root = value
        end
        args[#args + 1], args[#args + 2] = flag, value
      end
    elseif switches[arg] then
      args[#args + 1] = arg
    elseif arg:sub(1, 1) == '-' then
      error('Typst forward search does not support build option ' .. arg)
    else
      positional[#positional + 1] = arg
    end
    index = index + 1
  end
  assert(#positional >= 1 and #positional <= 2
    and canonical(positional[1], project.cwd) == project.main
    and (not positional[2] or canonical(positional[2], project.cwd) == canonical(project.pdf, project.cwd)),
    'Typst build input/output do not match the selected project')
  -- LSP workspace roots otherwise override the CLI's entry-directory default.
  if not root then
    root = canonical(vim.env.TYPST_ROOT or vim.fs.dirname(project.main), project.cwd)
    args[#args + 1], args[#args + 2] = '--root', root
  end
  args[#args + 1] = project.main
  return args, root
end

local function revision(path)
  local stat = assert(vim.uv.fs_stat(path), 'cannot stat Typst PDF: ' .. path)
  return {
    device = stat.dev, inode = stat.ino, length = stat.size,
    modified_seconds = stat.mtime.sec, modified_nanoseconds = stat.mtime.nsec,
    changed_seconds = stat.ctime.sec, changed_nanoseconds = stat.ctime.nsec,
  }
end

-- Fail closed if saved project inputs change between preview and PDF export.
-- Follow symlinks so an included dependency cannot bypass the source guard.
local function manifest(root, excluded)
  local entries, visited = {}, {}
  local function visit(path)
    if path == excluded or visited[path] then return end
    visited[path] = true
    local stat = assert(vim.uv.fs_lstat(path), 'cannot inspect Typst input: ' .. path)
    entries[path] = { stat.type, stat.dev, stat.ino, stat.size, stat.mtime, stat.ctime }
    if stat.type == 'link' then
      local target = vim.uv.fs_realpath(path)
      if target then visit(target) end
    elseif stat.type == 'directory' then
      local scan = assert(vim.uv.fs_scandir(path), 'cannot scan Typst input directory: ' .. path)
      while true do
        local name = vim.uv.fs_scandir_next(scan)
        if not name then break end
        visit(path .. '/' .. name)
      end
    end
  end
  visit(root)
  return entries
end

-- Only a local Tinymist data-plane connection is accepted. Frames are drained
-- incrementally, retaining at most 512 bytes: SVG updates can be very large.
local function websocket(port, ready, message, failure)
  local socket = assert(vim.uv.new_tcp())
  local buffer, upgraded, frame = '', false, nil
  local function send(opcode, payload)
    local mask = assert(vim.uv.random(4))
    local bytes = {}
    for i = 1, #payload do
      bytes[i] = string.char(bit.bxor(payload:byte(i), mask:byte((i - 1) % 4 + 1)))
    end
    socket:write(string.char(128 + opcode, 128 + #payload) .. mask .. table.concat(bytes))
  end
  local function consume(data)
    buffer = buffer .. data
    if not upgraded then
      local ending = buffer:find('\r\n\r\n', 1, true)
      if not ending then
        assert(#buffer <= 8192, 'oversized Tinymist websocket handshake')
        return
      end
      local header = buffer:sub(1, ending + 3)
      -- Fixed RFC 6455 nonce is sufficient for this private one-shot connection;
      -- checking its known response also rejects ordinary HTTP endpoints.
      assert(header:match('^HTTP/1%.1 101 ') and header:find('s3pPLMBiTxaQ9kYGzzhZRbK+xOo=', 1, true),
        'Tinymist refused the preview websocket connection')
      buffer, upgraded = buffer:sub(ending + 4), true
      ready(function() send(1, 'current') end)
    end
    while true do
      if not frame then
        if #buffer < 2 then return end
        local first, second = buffer:byte(1, 2)
        assert(first >= 128 and first < 144 and second < 128,
          'unsupported Tinymist websocket frame')
        local length, offset = second, 3
        if length == 126 then
          if #buffer < 4 then return end
          length, offset = buffer:byte(3) * 256 + buffer:byte(4), 5
        elseif length == 127 then
          if #buffer < 10 then return end
          length, offset = 0, 11
          for i = 3, 10 do length = length * 256 + buffer:byte(i) end
        end
        assert(length <= 128 * 1024 * 1024, 'Tinymist preview frame exceeds 128 MiB')
        frame = { remaining = length, prefix = '', opcode = first - 128 }
        buffer = buffer:sub(offset)
      end
      local count = math.min(frame.remaining, #buffer)
      frame.prefix = frame.prefix .. buffer:sub(1, math.min(count, 512 - #frame.prefix))
      frame.remaining = frame.remaining - count
      buffer = buffer:sub(count + 1)
      if frame.remaining > 0 then return end
      local completed = frame
      frame = nil
      if completed.opcode == 8 then
        error('Tinymist closed the preview connection')
      elseif completed.opcode == 9 then
        assert(#completed.prefix <= 125, 'invalid Tinymist websocket ping')
        send(10, completed.prefix)
      elseif completed.opcode == 1 or completed.opcode == 2 then
        message(completed.prefix)
      end
    end
  end
  socket:connect('127.0.0.1', port, function(err)
    if socket:is_closing() then return end
    if err then failure(tostring(err)); return end
    socket:read_start(function(read_error, data)
      if read_error or not data then
        failure(tostring(read_error or 'Tinymist preview disconnected'))
        return
      end
      local ok, reason = pcall(consume, data)
      if not ok then failure(tostring(reason)) end
    end)
    socket:write(table.concat({
      'GET / HTTP/1.1', 'Host: 127.0.0.1:' .. port,
      'Origin: http://127.0.0.1:' .. port, 'Upgrade: websocket', 'Connection: Upgrade',
      'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==', 'Sec-WebSocket-Version: 13', '', '',
    }, '\r\n'))
  end)
  return socket
end

function M.resolve(project, file, line, byte_column, callback)
  local rpc, socket, timer, temporary, done, exited, result
  local function cleanup()
    if temporary then
      local entries = vim.uv.fs_scandir(temporary)
      if entries then
        while true do
          local name = vim.uv.fs_scandir_next(entries)
          if not name then break end
          vim.uv.fs_unlink(temporary .. '/' .. name)
        end
      end
      vim.uv.fs_rmdir(temporary)
      temporary = nil
    end
    if result then
      local completed = result
      result = nil
      vim.schedule(function() callback(completed) end)
    end
  end
  local function finish(error, payload)
    if done then return end
    done = true
    active[finish] = nil
    if timer then timer:stop(); timer:close() end
    if socket and not socket:is_closing() then socket:read_stop(); socket:close() end
    -- Wait for process exit before deleting its export directory: a cancelled
    -- export must not recreate files after cleanup.
    result = { code = error and 1 or 0, stdout = payload or '', stderr = error or '' }
    if rpc and not exited then rpc.terminate() else cleanup() end
  end
  local function failure(error)
    finish('Typst forward search: ' .. tostring(error))
  end
  local function guarded(fn)
    return function(...)
      if done then return end
      local ok, reason = pcall(fn, ...)
      if not ok then failure(reason) end
    end
  end
  local function command(name, arguments, next_step)
    if done then return end
    assert(rpc.request('workspace/executeCommand', { command = name, arguments = arguments },
      guarded(function(err, result)
        if err then failure(err.message or vim.inspect(err)); return end
        next_step(result)
      end)), 'Tinymist request could not be sent')
  end
  active[finish] = true
  local start = guarded(function()
    assert(vim.fn.executable('tinymist') == 1, 'tinymist is required for Typst cursor navigation')
    -- Tinymist assigns source IDs lexically under its root. Mixing a symlink
    -- spelling for main with a realpath cursor (e.g. /tmp and /private/tmp)
    -- otherwise makes an included source appear to be outside the project.
    project = vim.tbl_extend('force', project, {
      main = canonical(project.main, project.cwd),
      cwd = canonical(project.cwd, project.cwd),
    })
    file = canonical(file, project.cwd)
    local args, root = compile_args(project)
    local source = vim.fn.readfile(file, '', line)[line]
    assert(source and byte_column >= 0 and byte_column <= #source, 'cursor is outside the saved Typst source')
    -- Preview's line_column_to_byte counts Unicode scalars, unlike LSP UTF-16.
    local character = vim.fn.strchars(source:sub(1, byte_column))
    local before = revision(project.pdf)
    temporary = canonical(assert(vim.uv.fs_mkdtemp(vim.fs.dirname(project.pdf) .. '/.pdfterm-XXXXXX')),
      project.cwd)
    local output = temporary .. '/forward.pdf'
    local inputs = manifest(root, temporary)
    local rendered, exported = false, false
    local function scroll()
      command('tinymist.scrollPreview', { 'pdfterm', {
        event = 'panelScrollTo', filepath = file, line = line - 1, character = character,
      } }, function() end)
    end
    local function on_message(data)
      if done then return end
      if not rendered and (data:match('^new,') or data:match('^diff%-v1,')) then
        rendered = true
        command('tinymist.exportPdf', { project.main }, function(result)
          assert(result ~= vim.NIL and result ~= nil and vim.uv.fs_stat(output),
            'Tinymist could not export the mapped PDF at ' .. output .. ': ' .. vim.inspect(result))
          exported = true
          scroll()
        end)
      elseif exported and data:match('^jump,') then
        local page, x, y = data:match('^jump,(%d+) ([^ ,]+) ([^ ,]+)')
        page, x, y = tonumber(page), tonumber(x), tonumber(y)
        assert(page and page > 0 and x and y and x == x and y == y
          and math.abs(x) < math.huge and math.abs(y) < math.huge, 'invalid Tinymist document position')
        assert(vim.deep_equal(before, revision(project.pdf)), 'PDF changed during Typst forward search; repeat navigation')
        assert(vim.deep_equal(inputs, manifest(root, temporary)),
          'project inputs changed during Typst forward search; repeat navigation')
        assert(vim.uv.fs_rename(output, project.pdf))
        -- Typst supplies a point in PDF points measured from the page top.
        -- A zero-size SyncTeX rectangle preserves that exact point.
        finish(nil, vim.json.encode({ pdf = project.pdf, revision = revision(project.pdf),
          page = page, h = x, v = y, width = 0, height = 0, word = vim.NIL }))
      end
    end
    rpc = vim.lsp.rpc.start({ 'tinymist', 'lsp' }, {
      notification = guarded(function(method, params)
        if method == 'window/showMessage' and params.type <= 2 then
          -- In particular, never silently map a default layout after rejected
          -- typstExtraArgs: Tinymist reports those as configuration warnings.
          failure(params.message)
        end
      end),
      server_request = function() return vim.NIL end,
      on_error = function(_, err) failure(vim.inspect(err)) end,
      on_exit = function()
        exited = true
        cleanup()
        failure('Tinymist exited before resolving the cursor')
      end,
    }, { cwd = project.cwd, detached = false })
    timer = assert(vim.uv.new_timer())
    timer:start(30000, 0, function() failure('timed out (the cursor may have no rendered position)') end)
    assert(rpc.request('initialize', {
      processId = vim.fn.getpid(), rootUri = vim.uri_from_fname(project.cwd),
      capabilities = vim.empty_dict(),
      initializationOptions = {
        -- Tinymist appends the format extension to outputPath.
        exportPdf = 'never', outputPath = temporary .. '/forward', typstExtraArgs = args,
        formatterMode = 'disable', semanticTokens = 'disable',
      },
    }, guarded(function(err, result)
      if err then failure(err.message); return end
      local commands = result.capabilities.executeCommandProvider
      assert(commands and vim.tbl_contains(commands.commands, 'tinymist.doStartPreview'),
        'installed Tinymist does not support the preview source-map protocol')
      rpc.notify('initialized', vim.empty_dict())
      command('tinymist.pinMain', { project.main }, function()
        command('tinymist.doStartPreview', { {
          '--task-id=pdfterm', '--data-plane-host=127.0.0.1:0', '--no-open', project.main,
        } }, function(preview)
          local port = preview.dataPlanePort
          assert(type(port) == 'number' and port > 0 and port < 65536,
            'Tinymist did not expose a local preview data plane')
          socket = websocket(port, function(current) current() end,
            function(data) vim.schedule(guarded(function() on_message(data) end)) end, failure)
        end)
      end)
    end)), 'Tinymist initialization could not be sent')
  end)
  start()
  return function() finish('navigation cancelled') end
end

vim.api.nvim_create_autocmd('VimLeavePre', {
  callback = function()
    for finish in pairs(active) do finish('Neovim is exiting') end
  end,
})

return M
