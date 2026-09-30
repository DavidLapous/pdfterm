-- Tinymist's preview uses Typst source spans, not PDF text matching. Its LSP
-- scrollPreview command delivers document positions over the local data plane.
local M = {}
local active = {}
local retained = {}
local closing = {}

local function canonical(path, cwd)
  local absolute = vim.fs.normalize(vim.startswith(path, '/') and path or cwd .. '/' .. path)
  return vim.uv.fs_realpath(absolute) or absolute
end

local function literal_word(source, byte_column)
  -- Bound the literal excerpt for the 4096-byte forward socket, even on long
  -- prose lines. Cut only at whitespace so UTF-8 and edge words remain whole.
  -- Rust owns Unicode tokenization; do not duplicate it with Vim's ASCII classes.
  local first, last = math.max(1, byte_column - 255), math.min(#source, byte_column + 256)
  if first > 1 then
    first = source:find('%s', first)
    if not first or first > byte_column then return vim.NIL end
    first = first + 1
  end
  if last < #source then
    local boundary = source:sub(first, last):match('.*()%s')
    if not boundary then return vim.NIL end
    last = first + boundary - 2
  end
  if byte_column < first - 1 or byte_column >= last then return vim.NIL end
  return { text = source:sub(first, last), byte_column = byte_column - first + 1 }
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
      ready(function(text)
        text = text or 'current'
        assert(#text <= 125, 'oversized Tinymist websocket command')
        send(1, text)
      end)
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

local function remove_directory(path)
  if not path then return end
  local entries = vim.uv.fs_scandir(path)
  if entries then
    while true do
      local name = vim.uv.fs_scandir_next(entries)
      if not name then break end
      vim.uv.fs_unlink(path .. '/' .. name)
    end
  end
  vim.uv.fs_rmdir(path)
end

local function integer(value, minimum)
  return type(value) == 'number' and value >= minimum and value < math.huge and value % 1 == 0
end

-- Preview source positions count Unicode scalars, including Tinymist's legacy
-- showDocument wrapper. UTF-16 is computed from the saved, compiled source.
local function location(path, row, character, inputs)
  assert(type(path) == 'string' and path:sub(1, 1) == '/' and not path:find('%z'),
    'Tinymist returned an invalid source path')
  path = canonical(path, '/')
  assert(inputs[path] and inputs[path][1] == 'file', 'mapped source is outside the saved project inputs')
  assert(integer(row, 0) and integer(character, 0), 'Tinymist returned an invalid source position')
  local lines = vim.fn.readfile(path, 'b', row + 2)
  local source = assert(lines[row + 1], 'Tinymist source line is outside the saved file')
  if lines[row + 2] then source = source:gsub('\r$', '') end
  local bytes, scalars, units = 0, 0, 0
  while scalars < character do
    local lead = assert(source:byte(bytes + 1), 'Tinymist source column is outside the saved line')
    local length = lead < 128 and 1 or lead >= 240 and 4 or lead >= 224 and 3 or lead >= 194 and 2
    assert(length and bytes + length <= #source, 'invalid UTF-8 in saved Typst source')
    for offset = 2, length do
      local continuation = source:byte(bytes + offset)
      assert(continuation >= 128 and continuation < 192, 'invalid UTF-8 in saved Typst source')
    end
    bytes, scalars, units = bytes + length, scalars + 1, units + (length == 4 and 2 or 1)
  end
  return { file = path, line = row + 1, byte_column = bytes,
    column = units + 1, column_char = scalars + 1, precise = true }
end

-- EOF frames both directions. Only one request reaches Tinymist at a time:
-- its source notifications have no request ID. An abandoned in-flight request
-- therefore invalidates the service instead of allowing a late reply to drift.
local function inverse_listener(path, dispatch)
  local server = assert(vim.uv.new_pipe(false))
  local clients, count = {}, 0
  local function close()
    if not server:is_closing() then server:close() end
    for close_client, is_answered in pairs(clients) do
      if not is_answered() then close_client() end
    end
    vim.uv.fs_unlink(path)
  end
  local ok, reason = pcall(function()
    assert(server:bind(path))
    assert(vim.uv.fs_chmod(path, 384))
    assert(server:listen(8, function(err)
      if err or server:is_closing() then return end
      local client = assert(vim.uv.new_pipe(false))
      if not server:accept(client) or count >= 8 then client:close(); return end
      count = count + 1
      local timer = assert(vim.uv.new_timer())
      local chunks, size, answered, cancel = {}, 0, false, nil
      local close_client
      close_client = function()
        if not clients[close_client] then return end
        clients[close_client], count = nil, count - 1
        timer:stop(); timer:close()
        if not client:is_closing() then client:read_stop(); client:close() end
      end
      clients[close_client] = function() return answered end
      local function abandon()
        local pending = cancel
        cancel = nil
        close_client()
        if pending then vim.schedule(pending) end
      end
      local function reply(value)
        if answered or client:is_closing() then return end
        answered, cancel = true, nil
        local encoded = vim.json.encode(value)
        if #encoded > 4096 then encoded = '{"ok":false,"error":"inverse response exceeds 4096 bytes"}' end
        client:write(encoded, function(write_error)
          if client:is_closing() then return end
          if write_error then close_client(); return end
          client:shutdown(function() close_client() end)
        end)
      end
      timer:start(9000, 0, function()
        if answered then close_client(); return end
        local pending = cancel
        reply({ ok = false, error = 'Typst inverse search timed out; point may have no rendered source; repeat forward search' })
        timer:start(1000, 0, close_client)
        if pending then vim.schedule(pending) end
      end)
      client:read_start(function(read_error, chunk)
        if read_error then abandon(); return end
        if chunk then
          size = size + #chunk
          if size > 4096 then
            client:read_stop()
            vim.schedule(function() reply({ ok = false, error = 'inverse request exceeds 4096 bytes' }) end)
          else
            chunks[#chunks + 1] = chunk
          end
          return
        end
        client:read_stop()
        vim.schedule(function()
          if answered or client:is_closing() then return end
          local decoded, request = pcall(vim.json.decode, table.concat(chunks))
          if not decoded then reply({ ok = false, error = 'invalid inverse request JSON' }); return end
          local dispatched, result = pcall(dispatch, request, reply)
          if not dispatched then reply({ ok = false, error = tostring(result) })
          elseif not answered then cancel = result end
        end)
      end)
    end))
  end)
  if not ok then close(); error(reason) end
  return close
end

function M.resolve(project, file, line, byte_column, callback)
  local rpc, socket, timer, temporary, private, close_listener, exited, result
  local completed, stopped, pending, send, published, inputs, root
  local stop
  local function cleanup()
    closing[stop] = nil
    remove_directory(temporary)
    temporary = nil
    remove_directory(private)
    private = nil
    if result then
      local value = result
      result = nil
      vim.schedule(function() callback(value) end)
    end
  end
  local function complete(error, payload)
    if completed then return end
    completed = true
    if timer then timer:stop(); timer:close(); timer = nil end
    result = { code = error and 1 or 0, stdout = payload or '', stderr = error or '' }
    if not error then
      local value = result
      result = nil
      vim.schedule(function() callback(value) end)
    end
  end
  stop = function(error)
    if stopped then return end
    stopped = true
    active[stop] = nil
    if retained[project.pdf] == stop then retained[project.pdf] = nil end
    complete(error or 'Typst source-map service stopped')
    if pending then
      pending({ ok = false, error = error or 'Typst source-map service stopped; repeat forward search' })
      pending = nil
    end
    if close_listener then close_listener(); close_listener = nil end
    -- Tinymist never writes into the socket directory; remove it immediately,
    -- including during VimLeavePre when process-exit callbacks may run late.
    remove_directory(private)
    private = nil
    if socket and not socket:is_closing() then socket:read_stop(); socket:close() end
    -- A cancelled export must not recreate files after cleanup.
    if rpc and not exited then
      closing[stop] = true
      rpc.terminate()
    else
      cleanup()
    end
  end
  local function failure(error)
    stop('Typst source navigation: ' .. tostring(error))
  end
  local function guarded(fn)
    return function(...)
      if stopped then return end
      local ok, reason = pcall(fn, ...)
      if not ok then failure(reason) end
    end
  end
  local function command(name, arguments, next_step)
    if stopped then return end
    assert(rpc.request('workspace/executeCommand', { command = name, arguments = arguments },
      guarded(function(err, result)
        if err then failure(err.message or vim.inspect(err)); return end
        next_step(result)
      end)), 'Tinymist request could not be sent')
  end
  active[stop] = true
  local start = guarded(function()
    assert(vim.fn.executable('tinymist') == 1, 'tinymist is required for Typst cursor navigation')
    -- Tinymist assigns source IDs lexically under its root. Mixing a symlink
    -- spelling for main with a realpath cursor (e.g. /tmp and /private/tmp)
    -- otherwise makes an included source appear to be outside the project.
    project = vim.tbl_extend('force', project, {
      main = canonical(project.main, project.cwd),
      cwd = canonical(project.cwd, project.cwd),
      pdf = canonical(project.pdf, project.cwd),
    })
    file = canonical(file, project.cwd)
    local args
    args, root = compile_args(project)
    local source = vim.fn.readfile(file, '', line)[line]
    assert(source and byte_column >= 0 and byte_column <= #source, 'cursor is outside the saved Typst source')
    -- Preview counts Unicode scalars and looks up the leaf BEFORE its position.
    -- Neovim's normal-mode cursor is ON a character: pass its trailing boundary,
    -- otherwise a word/line start resolves to preceding whitespace, not text.
    local character = vim.fn.strchars(source:sub(1, byte_column))
      + (byte_column < #source and 1 or 0)
    local before = revision(project.pdf)
    temporary = canonical(assert(vim.uv.fs_mkdtemp(vim.fs.dirname(project.pdf) .. '/.pdfterm-XXXXXX')),
      project.cwd)
    local output = temporary .. '/forward.pdf'
    inputs = manifest(root, temporary)
    local rendered, exported = false, false
    local function validate()
      assert(vim.deep_equal(published, revision(project.pdf)),
        'PDF changed since Typst source mapping; repeat forward search')
      assert(vim.deep_equal(inputs, manifest(root, nil)),
        'saved project inputs changed since Typst source mapping; repeat forward search')
    end
    local function inverse(request, reply)
      assert(type(request) == 'table' and integer(request.page, 1) and request.page <= 4294967295
        and type(request.x) == 'number' and type(request.y) == 'number'
        and request.x >= 0 and request.y >= 0 and request.x < math.huge and request.y < math.huge,
        'invalid Typst inverse point')
      assert(not stopped, 'Typst source-map service stopped; repeat forward search')
      assert(vim.deep_equal(request.revision, published), 'stale PDF revision; repeat forward search')
      validate()
      assert(not pending, 'Typst inverse search is busy')
      pending = reply
      local sent, reason = pcall(send, 'src-point ' .. vim.json.encode({
        page_no = request.page, x = request.x, y = request.y,
      }))
      if not sent then
        pending = nil
        stop('Tinymist inverse request failed; repeat forward search')
        error(reason)
      end
      return function() stop('Typst inverse request abandoned; repeat forward search') end
    end
    local function source_position(path, row, column)
      if not pending then return end
      local reply = pending
      pending = nil
      local ok, mapped = pcall(function()
        validate()
        local value = location(path, row, column, inputs)
        validate()
        return value
      end)
      reply(ok and { ok = true, location = mapped } or { ok = false, error = tostring(mapped) })
    end
    local function scroll()
      command('tinymist.scrollPreview', { 'pdfterm', {
        event = 'panelScrollTo', filepath = file, line = line - 1, character = character,
      } }, function() end)
    end
    local function on_message(data)
      if completed or stopped then return end
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
        remove_directory(temporary)
        temporary = nil
        private = canonical(assert(vim.uv.fs_mkdtemp('/tmp/pdfterm-XXXXXX')), '/')
        assert(vim.uv.fs_chmod(private, 448))
        local owner = assert(vim.uv.fs_lstat(private))
        assert(owner.uid == vim.uv.getuid() and bit.band(owner.mode, 511) == 448,
          'Typst inverse socket directory is not private')
        local endpoint = private .. '/inverse.sock'
        close_listener = inverse_listener(endpoint, inverse)
        -- Publishing changes PDF metadata and its parent directory: bind the
        -- retained map to the post-rename, post-cleanup project manifest.
        published = revision(project.pdf)
        inputs = manifest(root, nil)
        local previous = retained[project.pdf]
        retained[project.pdf] = stop
        if previous then previous('Typst source map refreshed; repeat forward search') end
        -- PDF points measured from the page top; keep the exact zero-size point.
        complete(nil, vim.json.encode({ pdf = project.pdf, revision = published,
          inverse_search = endpoint, page = page, h = x, v = y, width = 0, height = 0,
          word = literal_word(source, byte_column) }))
      end
    end
    rpc = vim.lsp.rpc.start({ 'tinymist', 'lsp' }, {
      notification = guarded(function(method, params)
        if method == 'window/showMessage' and params.type <= 2 then
          -- In particular, never silently map a default layout after rejected
          -- typstExtraArgs: Tinymist reports those as configuration warnings.
          failure(params.message)
        end
        if method == 'tinymist/preview/scrollSource' and pending then
          assert(type(params) == 'table' and type(params.start) == 'table',
            'Tinymist returned a missing source position')
          source_position(params.filepath, params.start[1], params.start[2])
        end
      end),
      server_request = function(method, params)
        if method == 'window/showDocument' then
          local ok, reason = pcall(function()
            if pending then
              assert(type(params) == 'table' and type(params.selection) == 'table'
                and type(params.selection.start) == 'table', 'Tinymist returned a missing source position')
              source_position(vim.uri_to_fname(params.uri), params.selection.start.line,
                params.selection.start.character)
            end
          end)
          if not ok then failure(reason) end
          -- Resolve only: never focus a window or move Neovim's cursor.
          return { success = ok }
        end
        return vim.NIL
      end,
      on_error = function(_, err) failure(vim.inspect(err)) end,
      on_exit = function()
        exited = true
        stop('Tinymist source-map service exited; repeat forward search')
        cleanup()
      end,
    }, { cwd = project.cwd, detached = false })
    timer = assert(vim.uv.new_timer())
    timer:start(30000, 0, function() failure('timed out (the cursor may have no rendered position)') end)
    assert(rpc.request('initialize', {
      processId = vim.fn.getpid(), rootUri = vim.uri_from_fname(project.cwd),
      capabilities = { general = { positionEncodings = { 'utf-16' } } },
      initializationOptions = {
        -- Tinymist appends the format extension to outputPath.
        exportPdf = 'never', outputPath = temporary .. '/forward', typstExtraArgs = args,
        customizedShowDocument = true,
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
          socket = websocket(port, function(current) send = current; current() end,
            function(data) vim.schedule(guarded(function() on_message(data) end)) end, failure)
        end)
      end)
    end)), 'Tinymist initialization could not be sent')
  end)
  start()
  return function()
    if not completed then stop('navigation cancelled') end
  end
end

vim.api.nvim_create_autocmd('VimLeavePre', {
  callback = function()
    for finish in pairs(active) do finish('Neovim is exiting') end
    vim.wait(2000, function() return next(closing) == nil end, 10)
  end,
})

return M
