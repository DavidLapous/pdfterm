-- Run: nvim --headless -u NONE -i NONE -n -l scripts/nvim_source_fuzz.lua INPUT_JSON OUTPUT_JSON
-- Observations only: expected source ownership belongs to the external history oracle.
local api, uv = vim.api, vim.uv
local started = uv.hrtime()
local input_path, output_path = arg[1], arg[2]
if input_path then input_path = vim.fn.fnamemodify(input_path, ':p') end
if output_path then output_path = vim.fn.fnamemodify(output_path, ':p') end
local result = { states = {}, notifications = {} }
local clipboard_writes = 0
local buffer, guard, document, original, snapshot_bytes, cached_path, cached_bytes
local context = { stage = 'input' }

local function read_bytes(path, optional)
  local fd, reason, code = uv.fs_open(path, 'r', 0)
  if not fd and optional and code == 'ENOENT' then return vim.NIL end
  assert(fd, tostring(reason))
  local stat = assert(uv.fs_fstat(fd))
  assert(stat.type == 'file', 'not a regular file: ' .. path)
  local bytes = assert(uv.fs_read(fd, stat.size, 0))
  assert(uv.fs_close(fd))
  return bytes
end

local function write_bytes(path, bytes)
  local fd = assert(uv.fs_open(path, 'w', 384))
  assert(uv.fs_write(fd, bytes, 0) == #bytes, 'short write: ' .. path)
  assert(uv.fs_close(fd))
end

local function mkdir(path)
  vim.fn.mkdir(path, 'p', 448)
  assert(uv.fs_chmod(path, 448))
end

local function pdf_fixture(path)
  local objects = {
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> >>',
  }
  local bytes, offsets = '%PDF-1.4\n', { 0 }
  for index, object in ipairs(objects) do
    offsets[index + 1] = #bytes
    bytes = bytes .. index .. ' 0 obj\n' .. object .. '\nendobj\n'
  end
  local xref = #bytes
  bytes = bytes .. 'xref\n0 4\n0000000000 65535 f \n'
  for index = 2, #offsets do
    bytes = bytes .. string.format('%010d 00000 n \n', offsets[index])
  end
  write_bytes(path, bytes .. 'trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n' .. xref .. '\n%%EOF\n')
end

local function revision(path)
  local stat = assert(uv.fs_stat(path))
  return {
    device = stat.dev, inode = stat.ino, length = stat.size,
    modified_seconds = stat.mtime.sec, modified_nanoseconds = stat.mtime.nsec,
    changed_seconds = stat.ctime.sec, changed_nanoseconds = stat.ctime.nsec,
  }
end

local function location(point)
  return {
    file = original, line = point[1], byte_column = point[2], pdf = document.pdf,
    revision = vim.deepcopy(document.revision.pdf),
    document_revision = vim.deepcopy(document.revision), source_snapshot = document.source_snapshot,
  }
end

local function observation()
  local live_file = api.nvim_buf_get_name(buffer)
  return {
    lines = api.nvim_buf_get_lines(buffer, 0, -1, true),
    disk = read_bytes(original), live_disk = read_bytes(live_file, true),
    modified = vim.bo[buffer].modified, live_file = live_file,
  }
end

local function warn_since(index)
  local warnings = {}
  for i = index + 1, #result.notifications do
    local notification = result.notifications[i]
    assert(notification.level < vim.log.levels.ERROR, notification.message)
    warnings[#warnings + 1] = notification.message
  end
  return warnings
end

local function deliver(path, point)
  -- The sentinel makes completion observable even if the source cursor is unchanged.
  api.nvim_set_current_buf(guard)
  local before = #result.notifications
  local pipe = assert(uv.new_pipe(false))
  local finished, failure = false, nil
  local function fail(reason)
    failure = tostring(reason)
    if not pipe:is_closing() then pipe:close() end
  end
  pipe:connect(path, function(connect_error)
    if connect_error then fail(connect_error); return end
    local sent, write_error = pipe:write(vim.json.encode(location(point)), function(callback_error)
      if callback_error then fail(callback_error); return end
      local shutdown, shutdown_error = pipe:shutdown(function(callback_shutdown_error)
        if callback_shutdown_error then fail(callback_shutdown_error); return end
        finished = true
        if not pipe:is_closing() then pipe:close() end
      end)
      if not shutdown then fail(shutdown_error) end
    end)
    if not sent then fail(write_error) end
  end)
  local completed = vim.wait(2000, function()
    return failure ~= nil or finished and
      (api.nvim_get_current_buf() ~= guard or #result.notifications > before)
  end, 1)
  if not pipe:is_closing() then pipe:close() end
  assert(completed, 'inverse listener delivery timed out after 2000ms')
  assert(not failure, 'inverse socket failure: ' .. tostring(failure))
  local warnings = warn_since(before)
  assert(api.nvim_get_current_buf() ~= guard, 'inverse listener did not select a source buffer')
  local cursor = api.nvim_win_get_cursor(0)
  return {
    file = api.nvim_buf_get_name(0), line = cursor[1], byte_column = cursor[2], warnings = warnings,
  }
end

local function main()
  assert(input_path and output_path, 'INPUT_JSON and OUTPUT_JSON are required')
  local input = vim.json.decode(read_bytes(input_path))
  assert(type(input.directory) == 'string' and input.directory:sub(1, 1) == '/')
  local directory = assert(uv.fs_realpath(input.directory))
  local stat = assert(uv.fs_lstat(directory))
  assert(stat.type == 'directory' and stat.uid == uv.getuid() and bit.band(stat.mode, 511) == 448,
    'case directory must be current-user-owned mode0700')
  assert(type(input.binary) == 'string' and input.binary:sub(1, 1) == '/')
  assert(type(input.initial) == 'table' and #input.initial >= 1)
  assert(type(input.operations) == 'table' and type(input.states) == 'table')
  assert(#input.states == #input.operations + 1, 'states length must equal operations length + 1')
  for _, line in ipairs(input.initial) do
    assert(type(line) == 'string' and not line:find('[\n%z]'), 'initial lines must be UTF-8 text lines')
  end

  context.stage = 'setup'
  local root = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':p:h:h')
  api.nvim_set_current_dir(directory)
  vim.opt.runtimepath:prepend(root)
  for name, suffix in pairs({ HOME = 'h', XDG_CACHE_HOME = 'c', XDG_CONFIG_HOME = 'x',
    XDG_STATE_HOME = 's', XDG_DATA_HOME = 'd', XDG_RUNTIME_DIR = 'r' }) do
    vim.env[name] = directory .. '/' .. suffix
    mkdir(vim.env[name])
  end
  for _, name in ipairs({ 'SSH_CONNECTION', 'SSH_TTY', 'PDFTERM_LAUNCH_SOCKET', 'PDFTERM_LAUNCH_TOKEN',
    'KITTY_LISTEN_ON', 'KITTY_WINDOW_ID', 'GHOSTTY_RESOURCES_DIR', 'GHOSTTY_WINDOW_ID',
    'PDFTERM_SOURCE_CACHE_REPLAY' }) do
    vim.env[name] = nil
  end
  vim.opt.modeline = false
  vim.opt.fileformats = 'unix'
  vim.opt.swapfile = false
  vim.opt.shada = ''
  vim.opt.undofile = false
  vim.opt.hidden = true
  vim.opt.clipboard = ''
  vim.g.clipboard = {
    name = 'pdfterm-private-fuzz',
    copy = { ['+'] = function() clipboard_writes = clipboard_writes + 1 end,
      ['*'] = function() clipboard_writes = clipboard_writes + 1 end },
    paste = { ['+'] = function() return { {}, 'v' } end,
      ['*'] = function() return { {}, 'v' } end },
    cache_enabled = 0,
  }
  vim.notify = function(message, level)
    result.notifications[#result.notifications + 1] = {
      message = tostring(message), level = level or vim.log.levels.INFO,
    }
  end

  -- Short private endpoints avoid macOS's 103-byte Unix socket limit.
  mkdir(vim.env.XDG_CONFIG_HOME .. '/pdfterm')
  write_bytes(vim.env.XDG_CONFIG_HOME .. '/pdfterm/config.toml',
    'forward_socket = ""\n[editor]\ntransport = "socket"\npath = '
      .. vim.json.encode(directory .. '/r/e') .. '\n')
  local session = 'f'
  local config_process = vim.system({ input.binary, '--print-config', '--session', session }, { text = true })
  local printed = config_process:wait(10000)
  assert(printed.code == 0, 'real binary configuration failed: ' .. (printed.stderr or ''))
  local config = vim.json.decode(printed.stdout)
  assert(config.editor.transport == 'socket', 'real binary did not select socket transport')
  local plugin = require('pdfterm')
  plugin.setup({ executable = input.binary, session = session, attach_only = true,
    focus_on_inverse = false, focus_on_forward = false, compile = false, open_pdf = false,
    keys = { forward = '', build = '', main_file = '', compile = '' } })
  local source = require('pdfterm.source')

  original = directory .. '/main.tex'
  local project = { main = original, pdf = directory .. '/main.pdf', cwd = directory }
  write_bytes(original, table.concat(input.initial, '\n') .. '\n')
  pdf_fixture(project.pdf)
  vim.cmd.edit(vim.fn.fnameescape(original))
  buffer = api.nvim_get_current_buf()
  vim.bo[buffer].bufhidden = 'hide'
  local capture = source.capture(project, { original })
  vim.fn.writefile({ '% revision 1' }, project.pdf, 'a')
  document = assert(source.publish(capture, {
    pdf = assert(uv.fs_realpath(project.pdf)), cwd = directory, main = original, inputs = { original },
    revision = { pdf = revision(project.pdf), companion = vim.NIL },
  }))
  snapshot_bytes = read_bytes(document.source_snapshot)
  local manifest = vim.json.decode(snapshot_bytes)
  cached_path = vim.fs.dirname(document.source_snapshot) .. '/' .. assert(manifest.files[original])
  cached_bytes = read_bytes(cached_path)
  assert(vim.fn.sha256(cached_bytes) == manifest.digests[original], 'published source digest mismatch')
  result.document = { pdf = document.pdf, source_file = original,
    source_snapshot = document.source_snapshot, document_revision = document.revision,
    snapshot_sha256 = vim.fn.sha256(snapshot_bytes), source_sha256 = vim.fn.sha256(cached_bytes) }

  guard = api.nvim_create_buf(true, false)
  api.nvim_buf_set_name(guard, directory .. '/unrelated-dirty.txt')
  api.nvim_buf_set_lines(guard, 0, -1, true, { 'Unrelated unsaved buffer', 'λ sentinel' })
  local guard_lines = api.nvim_buf_get_lines(guard, 0, -1, true)
  local guard_modified = vim.bo[guard].modified
  assert(guard_modified, 'sentinel must be dirty')

  -- Test-only bootstrap: this metadata-only PDF has no rendered SyncTeX/viewer.
  -- Call the real private ready() seam to install the actual plugin listener and
  -- inverse callback; public viewer/navigation coverage belongs to the native smoke.
  -- Do not replace the listener, fabricate a reply, or invoke clipboard-writing pairing.
  local ready
  for index = 1, 32 do
    local name, value = debug.getupvalue(plugin.forward_search, index)
    if not name then break end
    if name == 'ready' then ready = value; break end
  end
  assert(type(ready) == 'function', 'plugin listener bootstrap ready() unavailable')
  local listening = false
  ready(function() listening = true end, true)
  assert(vim.wait(12000, function() return listening or #result.notifications > 0 end, 1),
    'plugin listener setup timed out')
  warn_since(0)
  assert(listening and uv.fs_lstat(config.editor.path), 'plugin listener did not bind its configured endpoint')
  result.bootstrap = 'private ready upvalue; actual listener and inverse callback'

  local renamed = false
  local function probe(spec, index)
    context = { stage = 'state', state_index = index }
    local state = { forward = {}, inverse = {} }
    result.states[#result.states + 1] = state
    local before = observation()
    for query_index, point in ipairs(spec.forward) do
      context.stage, context.query_index = 'forward', query_index
      local mapped, unavailable = source.forward(document, buffer, point[1], point[2])
      assert(mapped, unavailable or 'forward projection unavailable')
      state.forward[#state.forward + 1] = { file = mapped.file, line = mapped.line, column = mapped.column,
        coarse = mapped.coarse, warning = mapped.warning or vim.NIL }
    end
    for query_index, point in ipairs(spec.inverse) do
      context.stage, context.query_index = 'inverse', query_index
      local mapped = source.inverse(location(point))
      state.inverse[#state.inverse + 1] = { file = mapped.file, line = mapped.line,
        byte_column = mapped.byte_column, warning = mapped.warning or vim.NIL }
    end
    context.stage, context.query_index = 'delivery', nil
    state.delivery = deliver(config.editor.path, spec.delivery)
    local after = observation()
    for key, value in pairs(after) do state[key] = value end
    state.buffer_preserved = vim.deep_equal(before.lines, after.lines) and before.modified == after.modified
      and before.live_file == after.live_file
    state.disk_preserved = before.disk == after.disk and before.live_disk == after.live_disk
    state.guard_preserved = api.nvim_buf_is_valid(guard) and api.nvim_buf_is_loaded(guard)
      and vim.deep_equal(guard_lines, api.nvim_buf_get_lines(guard, 0, -1, true))
      and vim.bo[guard].modified == guard_modified
    state.snapshot_preserved = read_bytes(document.source_snapshot) == snapshot_bytes
      and read_bytes(cached_path) == cached_bytes
    state.clipboard_writes = clipboard_writes
    state.complete = true
  end

  probe(input.states[1], 1)
  for index, operation in ipairs(input.operations) do
    context = { stage = 'operation', state_index = index + 1, operation_index = index, operation = operation }
    api.nvim_set_current_buf(buffer)
    if operation.kind == 'text' or operation.kind == 'lines' then
      -- A real editor undo break separates API mutations without resetting history.
      vim.cmd('normal! i' .. api.nvim_replace_termcodes('<C-G>u<Esc>', true, false, true))
      if operation.kind == 'text' then
        api.nvim_buf_set_text(buffer, operation.start[1], operation.start[2],
          operation['end'][1], operation['end'][2], operation.replacement)
      else
        api.nvim_buf_set_lines(buffer, operation.start, operation['end'], true, operation.replacement)
      end
    elseif operation.kind == 'undo' then
      vim.cmd.undo()
    elseif operation.kind == 'redo' then
      vim.cmd.redo()
    elseif operation.kind == 'save' then
      vim.cmd.write()
    elseif operation.kind == 'reload' then
      vim.cmd.write()
      api.nvim_set_current_buf(guard)
      vim.cmd('bunload ' .. buffer)
      assert(api.nvim_buf_is_valid(buffer) and not api.nvim_buf_is_loaded(buffer), 'source buffer did not unload')
      vim.cmd('buffer ' .. buffer)
      assert(api.nvim_get_current_buf() == buffer and api.nvim_buf_is_loaded(buffer), 'source buffer did not reload')
    elseif operation.kind == 'rename' then
      assert(not renamed, 'rename may occur only once')
      api.nvim_buf_set_name(buffer, directory .. '/renamed.tex')
      renamed = true
    else
      error('unknown operation kind: ' .. tostring(operation.kind))
    end
    probe(input.states[index + 1], index + 1)
  end
  context.stage = 'complete'
end

local ok, traceback = xpcall(main, debug.traceback)
result.elapsed_seconds = (uv.hrtime() - started) / 1e9
if not ok then
  result.runtime_error = tostring(traceback):match('^[^\n]+') or tostring(traceback)
  result.traceback = tostring(traceback)
  result.failure_context = context
  if buffer and api.nvim_buf_is_valid(buffer) and api.nvim_buf_is_loaded(buffer) then
    local observed, state = pcall(observation)
    if observed then result.failure_state = state else result.failure_observation_error = tostring(state) end
  end
  result.clipboard_writes = clipboard_writes
end
local written, write_error = pcall(function()
  assert(output_path, 'OUTPUT_JSON is required')
  write_bytes(output_path, vim.json.encode(result) .. '\n')
end)
if not written then
  io.stderr:write('Could not retain worker result: ' .. tostring(write_error) .. '\n')
end
if not ok or not written then
  if traceback then io.stderr:write(tostring(traceback) .. '\n') end
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
