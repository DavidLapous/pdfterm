-- Saved compiler inputs are immutable; Neovim range extmarks own every live edit.
local M = {}
local MAX_SOURCE, MAX_MANIFEST, MAX_WARNING = 2 * 1024 * 1024, 4 * 1024 * 1024, 4096
local documents, manifests, latest, pending, warm, tracked, buffers, document_order = {}, {}, {}, {}, {}, {}, {}, {}
local parsed_by_digest, parsed_count = {}, 0
local serial, installed = 0, false
local namespace_prefix = 'pdfterm.source.' .. tostring(vim.uv.hrtime()) .. '.'

local function absolute(path, cwd, real)
  if type(path) ~= 'string' or path == '' or path:find('%z') then return nil end
  if path:sub(1, 1) ~= '/' then path = (cwd or vim.fn.getcwd()) .. '/' .. path end
  local components = {}
  for component in path:gmatch('[^/]+') do
    if component ~= '.' then components[#components + 1] = component end
  end
  -- Missing original identities retain symlink-sensitive `..`, as the native
  -- snapshot reader does. Existing paths are canonicalized by the caller.
  path = '/' .. table.concat(components, '/')
  return real and (vim.uv.fs_realpath(path) or path) or path
end

local function output_identity(path, cwd)
  path = absolute(path, cwd)
  if not path then return nil end
  local existing = vim.uv.fs_realpath(path)
  if existing then return existing end
  local suffix, parent = { vim.fs.basename(path) }, vim.fs.dirname(path)
  while parent do
    local resolved = vim.uv.fs_realpath(parent)
    if resolved then return (resolved == '/' and '' or resolved) .. '/' .. table.concat(suffix, '/') end
    table.insert(suffix, 1, vim.fs.basename(parent))
    local ancestor = vim.fs.dirname(parent)
    if ancestor == parent then break end
    parent = ancestor
  end
  return path
end

local function snapshot_identity(path)
  path = absolute(path)
  if not path then return nil end
  local parent = vim.uv.fs_realpath(vim.fs.dirname(path))
  return parent and parent .. '/' .. vim.fs.basename(path) or path
end

local function open_flags(no_follow)
  local constants = vim.uv.constants
  local read_only = assert(constants.O_RDONLY, 'pdfterm: luv does not expose O_RDONLY')
  local nonblock = assert(constants.O_NONBLOCK, 'pdfterm: luv does not expose O_NONBLOCK')
  local flags = bit.bor(read_only, nonblock)
  if no_follow then
    -- luv omits O_NOFOLLOW. These are fcntl.h ABI bits; ARM Linux deliberately
    -- differs from asm-generic/x86. Never guess an unknown platform's bits.
    local system = vim.uv.os_uname()
    local nofollow = constants.O_NOFOLLOW
    if not nofollow and system.sysname == 'Darwin' then nofollow = 0x100 end
    if not nofollow and system.sysname == 'Linux' then
      if system.machine == 'x86_64' or system.machine:match('^i[3-6]86$') then nofollow = 0x20000
      elseif system.machine == 'aarch64' or system.machine == 'arm64'
        or system.machine:match('^armv[6-8]l$') then nofollow = 0x8000 end
    end
    assert(nofollow, 'pdfterm: O_NOFOLLOW is unavailable for ' .. system.sysname .. '/' .. system.machine)
    flags = bit.bor(flags, nofollow)
  end
  return flags
end

local function stable(value)
  if type(value) ~= 'table' then return vim.json.encode(value) end
  if vim.islist(value) and #value > 0 then
    local items = {}
    for _, item in ipairs(value) do items[#items + 1] = stable(item) end
    return '[' .. table.concat(items, ',') .. ']'
  end
  local keys, items = {}, {}
  for key in pairs(value) do keys[#keys + 1] = key end
  table.sort(keys)
  for _, key in ipairs(keys) do items[#items + 1] = vim.json.encode(key) .. ':' .. stable(value[key]) end
  return '{' .. table.concat(items, ',') .. '}'
end

local function document_key(pdf, revision)
  return vim.fn.sha256(pdf .. '\n' .. stable(revision))
end

-- Return the next UTF-8 byte boundary, rejecting overlongs, surrogates and NUL.
local function next_scalar(text, index)
  local first = text:byte(index)
  if not first or first == 0 then return nil end
  if first < 128 then return index + 1 end
  local length = first >= 194 and first <= 223 and 2
    or first >= 224 and first <= 239 and 3
    or first >= 240 and first <= 244 and 4
  if not length then return nil end
  for at = index + 1, index + length - 1 do
    local byte = text:byte(at)
    if not byte or byte < 128 or byte > 191 then return nil end
  end
  local second = text:byte(index + 1)
  if first == 224 and second < 160 or first == 237 and second >= 160
    or first == 240 and second < 144 or first == 244 and second >= 144 then return nil end
  return index + length
end

local function coordinate(value, default)
  value = tonumber(value)
  if not value or value ~= value or value == math.huge or value == -math.huge then return default end
  return value
end

local function byte_boundary(text, column)
  column = math.max(0, math.min(coordinate(column, 0), #text))
  column = math.floor(column)
  while column > 0 and column < #text do
    local byte = text:byte(column + 1)
    if byte < 128 or byte >= 192 then break end
    column = column - 1
  end
  return column
end

local function scalar_column(text, byte)
  local index, count = 1, 0
  byte = byte_boundary(text, byte)
  while index <= byte do
    index = next_scalar(text, index)
    if not index then return count + 1 end
    count = count + 1
  end
  return count + 1
end

local function scalar_byte(text, column)
  local index = 1
  for _ = 1, math.max(0, math.floor(coordinate(column, 1)) - 1) do
    local following = next_scalar(text, index)
    if not following then break end
    index = following
  end
  return math.min(index - 1, #text)
end

local function warning(first, second)
  if first == vim.NIL then first = nil end
  if second == vim.NIL then second = nil end
  first = first and first:gsub('%c', ' '):gsub('\194[\128-\159]', ' ')
  second = second and second:gsub('%c', ' '):gsub('\194[\128-\159]', ' ')
  local combined
  if not first or first == '' then combined = second
  elseif not second or second == '' or first == second then combined = first
  else combined = first .. '; ' .. second end
  return combined and combined:sub(1, byte_boundary(combined, MAX_WARNING)) or nil
end

local function saved_revision(stat)
  if not stat then return nil end
  return { device = stat.dev, inode = stat.ino, length = stat.size,
    modified_seconds = stat.mtime.sec, modified_nanoseconds = stat.mtime.nsec,
    changed_seconds = stat.ctime.sec, changed_nanoseconds = stat.ctime.nsec }
end

local function same_stat(first, second)
  return first ~= nil and second ~= nil
    and first.dev == second.dev and first.ino == second.ino and first.size == second.size
    and first.mtime.sec == second.mtime.sec and first.mtime.nsec == second.mtime.nsec
    and first.ctime.sec == second.ctime.sec and first.ctime.nsec == second.ctime.nsec
end

local function read_bytes(path, bound, no_follow)
  local fd, reason = vim.uv.fs_open(path, open_flags(no_follow), 0)
  if not fd then return nil, reason end
  local stat = vim.uv.fs_fstat(fd)
  local bytes
  if not stat or stat.type ~= 'file' then reason = 'not a regular file'
  elseif stat.size > bound then reason = 'source/cache file exceeds ' .. bound .. ' bytes'
  else bytes, reason = vim.uv.fs_read(fd, stat.size, 0) end
  local after = vim.uv.fs_fstat(fd)
  local closed, close_error = vim.uv.fs_close(fd)
  if not closed then return nil, close_error end
  if not bytes then return nil, reason end
  if #bytes ~= stat.size or not same_stat(stat, after) then
    return nil, 'file changed while reading'
  end
  return bytes, nil, saved_revision(stat)
end

local function source_bytes(path, no_follow)
  local bytes, reason, revision = read_bytes(path, MAX_SOURCE, no_follow)
  if not bytes then return nil, reason end
  local index = 1
  while index <= #bytes do
    index = next_scalar(bytes, index)
    if not index then return nil, 'source is not valid UTF-8 text' end
  end
  return bytes, nil, revision
end

function M.read_input(path)
  local text, reason = source_bytes(path)
  return text, reason
end

local function split_lines(bytes)
  local lines = vim.split(bytes, '\n', { plain = true })
  local terminated = bytes:sub(-1) == '\n'
  if terminated then table.remove(lines) end
  for index, line in ipairs(lines) do
    if (terminated or index < #lines) and line:sub(-1) == '\r' then
      lines[index] = line:sub(1, -2)
    end
  end
  if #lines == 0 then lines[1] = '' end
  return lines
end

local function same_lines(first, second)
  if #first ~= #second then return false end
  for index, line in ipairs(first) do if line ~= second[index] then return false end end
  return true
end

local function normal_buffer(buffer)
  return vim.api.nvim_buf_is_valid(buffer) and vim.bo[buffer].buftype == ''
    and absolute(vim.api.nvim_buf_get_name(buffer)) ~= nil
end

local function loaded_buffer(path)
  for _, buffer in ipairs(vim.api.nvim_list_bufs()) do
    if normal_buffer(buffer) and absolute(vim.api.nvim_buf_get_name(buffer), nil, true) == path then
      return buffer
    end
  end
end

local function cached_contents(cache_file, digest)
  local bytes, reason = source_bytes(cache_file, true)
  assert(bytes, 'pdfterm: required cached source is unavailable: ' .. tostring(reason))
  assert(vim.fn.sha256(bytes) == digest, 'pdfterm: cached source changed after publication')
  return bytes
end

local function contents(source)
  if source.cache_file then return cached_contents(source.cache_file, source.digest) end
  return source.text
end

local function lines_for(source)
  if source.lines then return source.lines end
  local bytes, reason = contents(source)
  assert(bytes, 'pdfterm: could not read captured source: ' .. tostring(reason))
  local cached = source.digest and parsed_by_digest[source.digest]
  if cached then
    source.lines, source.starts = cached.lines, cached.starts
    return source.lines
  end
  source.lines, source.starts = split_lines(bytes), {}
  local offset = 0
  for index, line in ipairs(source.lines) do
    source.starts[index], offset = offset, offset + #line + 1
  end
  if source.digest then
    if parsed_count >= 64 then parsed_by_digest, parsed_count = {}, 0 end
    parsed_by_digest[source.digest], parsed_count = { lines = source.lines, starts = source.starts }, parsed_count + 1
  end
  return source.lines
end

local function original_position(source, offset)
  local lines = lines_for(source)
  local low, high = 1, #lines
  while low < high do
    local middle = math.floor((low + high + 1) / 2)
    if source.starts[middle] <= offset then low = middle else high = middle - 1 end
  end
  return low - 1, offset - source.starts[low]
end

local function clamp_position(lines, row, column)
  row = math.max(0, math.min(math.floor(coordinate(row, 0)), #lines - 1))
  return row, byte_boundary(lines[row + 1], column)
end

local function buffer_position(buffer, row, column)
  row = math.max(0, math.min(math.floor(coordinate(row, 0)), vim.api.nvim_buf_line_count(buffer) - 1))
  local text = vim.api.nvim_buf_get_lines(buffer, row, row + 1, false)[1] or ''
  return row, byte_boundary(text, column), text
end

-- Diff is used only at initial attachment/reload, never to certify an occurrence.
local function row_alignment(old, current)
  local hunks = vim.diff(table.concat(old, '\n') .. '\n', table.concat(current, '\n') .. '\n', {
    result_type = 'indices', algorithm = 'patience',
  })
  return function(row)
    local delta = 0
    for _, hunk in ipairs(hunks) do
      local first = hunk[2] == 0 and hunk[1] or hunk[1] - 1
      if row < first then break end
      if hunk[2] > 0 and row < first + hunk[2] then
        local target = hunk[4] == 0 and hunk[3] or hunk[3] - 1
        return math.max(0, math.min(target + math.min(row - first, math.max(0, hunk[4] - 1)), #current - 1))
      end
      delta = delta + hunk[4] - hunk[2]
    end
    return math.max(0, math.min(row + delta, #current - 1))
  end
end

local function put_anchor(source, buffer, offset, row, column, end_row, end_column, retired)
  local original_row, original_column = original_position(source, offset)
  local boundary = original_column == #source.lines[original_row + 1]
  local id = vim.api.nvim_buf_set_extmark(buffer, source.namespace, row, column, {
    end_row = end_row, end_col = end_column,
    right_gravity = not boundary, end_right_gravity = false,
    undo_restore = true, invalidate = true,
  })
  source.by_id[id], source.by_offset[offset] = offset, id
  if retired then source.retired[id] = true end
end

local function attach(source, buffer)
  if not normal_buffer(buffer) or not vim.api.nvim_buf_is_loaded(buffer) then return false end
  if source.buffer ~= buffer and absolute(vim.api.nvim_buf_get_name(buffer), nil, true) ~= source.live_file then
    return false
  end
  if source.loaded and source.buffer == buffer then return true end
  if source.buffer and source.buffer ~= buffer and vim.api.nvim_buf_is_valid(source.buffer) then
    return false -- A renamed original is not a different buffer reusing its old pathname.
  end
  local current = vim.api.nvim_buf_get_lines(buffer, 0, -1, false)
  local original = lines_for(source)
  source.namespace = source.namespace or vim.api.nvim_create_namespace(namespace_prefix .. source.id)
  vim.api.nvim_buf_clear_namespace(buffer, source.namespace, 0, -1)
  source.by_id, source.by_offset, source.retired = {}, {}, {}
  local checkpoint = source.checkpoint
  if checkpoint then
    local unchanged = same_lines(checkpoint.lines, current)
    local align = not unchanged and row_alignment(checkpoint.lines, current) or nil
    if align then
      source.inferred = warning(source.inferred, 'buffer reloaded with changed text; using inferred source anchors')
    end
    for _, mark in ipairs(checkpoint.marks) do
      local row, column = clamp_position(current, align and align(mark[2]) or mark[2], mark[3])
      local end_row, end_column = clamp_position(current, align and align(mark[4]) or mark[4], mark[5])
      if end_row < row or end_row == row and end_column < column then end_row, end_column = row, column end
      put_anchor(source, buffer, mark[1], row, column, end_row, end_column, mark[6])
    end
    source.checkpoint = nil
  else
    local unchanged = same_lines(original, current)
    local align = not unchanged and row_alignment(original, current) or function(row) return row end
    if not unchanged then
      source.inferred = 'source buffer differs from captured text; using inferred source anchors'
    end
    -- One native range per compiled line. Interior edits deliberately lose
    -- precision; comparing the entire surviving span proves byte correspondence.
    for row, line in ipairs(original) do
      local live_row = align(row - 1)
      local end_column = byte_boundary(current[live_row + 1], #line)
      put_anchor(source, buffer, source.starts[row], live_row, 0, live_row, end_column)
    end
  end
  source.buffer, source.loaded = buffer, true
  source.live_file = absolute(vim.api.nvim_buf_get_name(buffer), nil, true)
  buffers[buffer] = buffers[buffer] or { sources = {} }
  buffers[buffer].sources[source] = true
  return true
end

local function new_source(path, bytes, buffer, revision)
  serial = serial + 1
  local source = { id = serial, original = path, live_file = path, text = bytes,
    digest = bytes and vim.fn.sha256(bytes) or nil, saved_revision = revision }
  tracked[source] = true
  if buffer then attach(source, buffer) end
  return source
end

local function freeze(source)
  local buffer = source.buffer
  if not source.loaded or not buffer or not vim.api.nvim_buf_is_loaded(buffer) then return end
  local checkpoint = { lines = vim.api.nvim_buf_get_lines(buffer, 0, -1, false), marks = {} }
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(buffer, source.namespace, 0, -1, { details = true })) do
    local details = mark[4]
    checkpoint.marks[#checkpoint.marks + 1] = {
      source.by_id[mark[1]], mark[2], mark[3], details.end_row, details.end_col,
      details.invalid or source.retired[mark[1]] or false,
    }
  end
  source.checkpoint, source.loaded = checkpoint, false
end

local function source_buffer(path)
  return loaded_buffer(path)
end

local function warm_buffer(buffer)
  if not normal_buffer(buffer) or not vim.api.nvim_buf_is_loaded(buffer) then return end
  local path = absolute(vim.api.nvim_buf_get_name(buffer), nil, true)
  for source in pairs(tracked) do
    if source.buffer == buffer or not source.buffer and source.live_file == path then attach(source, buffer) end
  end
  local filetype = vim.bo[buffer].filetype
  if not path:match('%.tex$') and not path:match('%.sty$') and not path:match('%.cls$')
    and not path:match('%.ltx$') and not path:match('%.bib$') and not path:match('%.tikz$')
    and not path:match('%.typ$') and filetype ~= 'tex' and filetype ~= 'plaintex'
    and filetype ~= 'latex' and filetype ~= 'bib' and filetype ~= 'typst' then return end
  if buffers[buffer] and next(buffers[buffer].sources) then return end
  if warm[path] then attach(warm[path], buffer); return end
  local bytes = source_bytes(path)
  if bytes then
    local source = new_source(path, bytes, buffer)
    source.warm, warm[path] = true, source
  end
end

local function detach_source(source)
  if source.buffer and vim.api.nvim_buf_is_valid(source.buffer) and source.namespace then
    if vim.api.nvim_buf_is_loaded(source.buffer) then
      vim.api.nvim_buf_clear_namespace(source.buffer, source.namespace, 0, -1)
    end
    local state = buffers[source.buffer]
    if state then state.sources[source] = nil end
  end
  tracked[source] = nil
end

function M.setup(group)
  if installed then return end
  installed = true
  vim.api.nvim_create_autocmd({ 'BufReadPost', 'BufNewFile', 'BufEnter', 'FileType', 'BufWritePre' }, {
    group = group, callback = function(event) warm_buffer(event.buf) end,
  })
  vim.api.nvim_create_autocmd({ 'BufReadPre', 'BufUnload', 'BufDelete', 'BufWipeout' }, {
    group = group,
    callback = function(event)
      local state = buffers[event.buf]
      if not state then return end
      for source in pairs(state.sources) do
        freeze(source)
        if event.event == 'BufWipeout' then
          source.buffer = nil
          if source.warm and not source.document then
            if warm[source.original] == source then warm[source.original] = nil end
            tracked[source] = nil
          end
        end
      end
      if event.event == 'BufWipeout' then buffers[event.buf] = nil end
    end,
  })
  vim.api.nvim_create_autocmd('BufFilePost', {
    group = group,
    callback = function(event)
      local state = buffers[event.buf]
      if not state then return end
      local path = absolute(vim.api.nvim_buf_get_name(event.buf), nil, true)
      for source in pairs(state.sources) do source.live_file = path or source.live_file end
    end,
  })
  for _, buffer in ipairs(vim.api.nvim_list_bufs()) do warm_buffer(buffer) end
end

local function release(capture)
  if not capture or capture.released then return end
  capture.released = true
  for _, source in pairs(capture.sources) do
    if not source.document and not source.warm then detach_source(source) end
  end
end

function M.capture(project, extra_inputs)
  local pdf = assert(output_identity(project.pdf, project.cwd), 'pdfterm: missing project PDF')
  release(pending[pdf])
  serial = serial + 1
  local capture = { id = serial, pdf = pdf, project = vim.deepcopy(project), sources = {}, warnings = {} }
  local paths = {}
  paths[assert(absolute(project.main, project.cwd, true), 'pdfterm: missing project main source')] = true
  for _, path in ipairs(extra_inputs or {}) do
    path = absolute(path, project.cwd, true)
    assert(path, 'pdfterm: invalid captured source input')
    paths[path] = true
  end
  for path in pairs(paths) do
    local bytes, reason, revision = source_bytes(path)
    if bytes then capture.sources[path] = new_source(path, bytes, source_buffer(path), revision)
    else capture.warnings[path] = 'source was unavailable before compilation: ' .. tostring(reason) end
  end
  pending[pdf] = capture
  return capture
end

local function cache_root()
  local path = absolute(vim.fn.stdpath('cache') .. '/pdfterm/sources')
  vim.fn.mkdir(path, 'p', 448)
  local stat = vim.uv.fs_stat(path)
  assert(stat and stat.type == 'directory', 'pdfterm: source cache directory is unavailable')
  assert(vim.uv.fs_chmod(path, 448))
  return assert(vim.uv.fs_realpath(path))
end

local function write_private(path, bytes)
  local fd, reason = vim.uv.fs_open(path, 'wx', 384)
  assert(fd, 'pdfterm: could not create source cache: ' .. tostring(reason))
  local count, write_error = vim.uv.fs_write(fd, bytes, 0)
  local synced, sync_error
  if count == #bytes then synced, sync_error = vim.uv.fs_fsync(fd) end
  local closed, close_error = vim.uv.fs_close(fd)
  assert(count == #bytes and synced and closed,
    'pdfterm: could not save source cache: ' .. tostring(write_error or sync_error or close_error))
end

local function sorted_keys(values)
  local keys = {}
  for key in pairs(values) do keys[#keys + 1] = key end
  table.sort(keys)
  return keys
end

local function remember(document)
  if not documents[document.key] then document_order[#document_order + 1] = document.key end
  documents[document.key], manifests[document.source_snapshot] = document, document
  if #document_order > 64 then
    for index, key in ipairs(document_order) do
      local evicted = documents[key]
      if evicted and latest[evicted.pdf] ~= evicted then
        table.remove(document_order, index)
        documents[key], manifests[evicted.source_snapshot] = nil, nil
        for _, entry in pairs(evicted.files) do
          local shared = entry.source.warm
          for _, other in pairs(documents) do
            for _, other_entry in pairs(other.files) do
              if other_entry.source == entry.source then shared = true; break end
            end
            if shared then break end
          end
          if not shared then
            entry.source.document = nil
            detach_source(entry.source)
          end
        end
        break
      end
    end
  end
  return document
end

local function load_document(path, expected_pdf, expected_revision)
  local directory = vim.uv.fs_lstat(vim.fs.dirname(path))
  local manifest_stat = vim.uv.fs_lstat(path)
  assert(directory and directory.type == 'directory' and directory.uid == vim.uv.getuid()
    and bit.band(directory.mode, 63) == 0, 'pdfterm: source snapshot directory is not private/current-user-owned')
  assert(manifest_stat and manifest_stat.type == 'file', 'pdfterm: source snapshot manifest is not a regular file')
  local bytes, reason = read_bytes(path, MAX_MANIFEST, true)
  assert(bytes, 'pdfterm: source snapshot is unavailable: ' .. tostring(reason))
  local known = manifests[path]
  if known then
    assert(known.manifest_digest == vim.fn.sha256(bytes), 'pdfterm: source snapshot manifest changed after publication')
    assert(not expected_pdf or known.pdf == expected_pdf, 'pdfterm: source snapshot PDF identity mismatch')
    assert(not expected_revision or stable(known.revision) == stable(expected_revision),
      'pdfterm: source snapshot revision mismatch')
    return known
  end
  local manifest = vim.json.decode(bytes)
  assert(type(manifest) == 'table' and absolute(manifest.pdf) == manifest.pdf
    and absolute(manifest.cwd) == manifest.cwd and absolute(manifest.main) == manifest.main
    and type(manifest.revision) == 'table' and type(manifest.revision.pdf) == 'table'
    and type(manifest.files) == 'table' and not vim.islist(manifest.files)
    and type(manifest.digests) == 'table' and not vim.islist(manifest.digests)
    and type(manifest.warnings) == 'table' and not vim.islist(manifest.warnings),
    'pdfterm: invalid source snapshot manifest')
  assert(not expected_pdf or manifest.pdf == expected_pdf, 'pdfterm: source snapshot PDF identity mismatch')
  assert(not expected_revision or stable(manifest.revision) == stable(expected_revision),
    'pdfterm: source snapshot revision mismatch')
  local files = {}
  for original, filename in pairs(manifest.files) do
    assert(absolute(original) == original and type(filename) == 'string' and filename ~= ''
      and filename ~= '.' and filename ~= '..' and not filename:find('/', 1, true)
      and not filename:find('\\', 1, true) and not filename:find('%z'), 'pdfterm: invalid source snapshot filename')
    local digest = manifest.digests[original]
    assert(type(digest) == 'string' and #digest == 64 and not digest:find('[^0-9a-f]'),
      'pdfterm: invalid source snapshot digest')
    local cached = vim.fs.dirname(path) .. '/' .. filename
    local stat = vim.uv.fs_lstat(cached)
    assert(stat and stat.type == 'file' and stat.size <= MAX_SOURCE, 'pdfterm: invalid cached source file')
    files[original] = cached
  end
  for original in pairs(manifest.digests) do
    assert(manifest.files[original] ~= nil, 'pdfterm: source snapshot digest has no captured input')
  end
  for original, reason in pairs(manifest.warnings) do
    assert(absolute(original) == original and type(reason) == 'string' and reason ~= ''
      and #reason <= MAX_WARNING and not reason:find('%c') and not reason:find('\194[\128-\159]'),
      'pdfterm: invalid source provenance warning')
  end
  local document = { pdf = manifest.pdf, revision = manifest.revision, cwd = manifest.cwd,
    main = manifest.main, files = {}, source_snapshot = path,
    key = document_key(manifest.pdf, manifest.revision), manifest_digest = vim.fn.sha256(bytes) }
  local staged = {}
  local ok, failure = pcall(function()
    for original, cached in pairs(files) do
      local buffer = source_buffer(original)
      local source = warm[original]
      if not source or source.digest ~= manifest.digests[original] then
        source = new_source(original, nil)
        source.digest = manifest.digests[original]
      end
      staged[original] = source
      if buffer or warm[original] then
        source.text = cached_contents(cached, manifest.digests[original])
        if buffer then attach(source, buffer) end
      end
      document.files[original] = { source = source, digest = manifest.digests[original],
        warning = manifest.warnings[original] }
    end
  end)
  if not ok then release({ sources = staged }); error(failure) end
  for original, entry in pairs(document.files) do
    entry.cache_file = files[original]
    entry.source.cache_file, entry.source.document = entry.cache_file, true
    if not entry.source.buffer then entry.source.text = nil end
  end
  return remember(document)
end

local function persist(project, info, sources, warnings)
  local pdf = assert(absolute(info.pdf, info.cwd), 'pdfterm: missing source metadata PDF')
  assert(type(info.revision) == 'table' and type(info.revision.pdf) == 'table',
    'pdfterm: missing source metadata revision')
  local key = document_key(pdf, info.revision)
  if documents[key] then return documents[key] end
  local directory = cache_root() .. '/' .. key
  local path = directory .. '/manifest.json'
  if vim.uv.fs_stat(directory) then return load_document(path, pdf, info.revision) end
  local temporary = assert(vim.uv.fs_mkdtemp(cache_root() .. '/pending-XXXXXX'))
  local manifest = { pdf = pdf, revision = vim.deepcopy(info.revision), cwd = absolute(info.cwd),
    main = absolute(type(info.main) == 'string' and info.main or project.main, info.cwd), files = vim.empty_dict(),
    digests = vim.empty_dict(), warnings = vim.empty_dict() }
  assert(manifest.cwd and manifest.main, 'pdfterm: missing source metadata cwd/main')
  local document = { pdf = pdf, revision = manifest.revision, cwd = manifest.cwd,
    main = manifest.main, files = {}, source_snapshot = path, key = key }
  local existing
  local ok, failure = pcall(function()
    for index, original in ipairs(sorted_keys(sources)) do
      local source = sources[original]
      local text, read_error = contents(source)
      assert(text, 'pdfterm: captured source is unavailable: ' .. tostring(read_error))
      local filename = index .. '.txt'
      write_private(temporary .. '/' .. filename, text)
      manifest.files[original] = filename
      manifest.digests[original] = vim.fn.sha256(text)
      document.files[original] = { source = source, digest = manifest.digests[original], warning = warnings[original] }
    end
    for original, reason in pairs(warnings) do
      manifest.warnings[original] = warning(reason)
    end
    local encoded = vim.json.encode(manifest)
    document.manifest_digest = vim.fn.sha256(encoded)
    assert(#encoded <= MAX_MANIFEST, 'pdfterm: source snapshot manifest exceeds 4 MiB')
    write_private(temporary .. '/manifest.json', encoded)
    local renamed, rename_error = vim.uv.fs_rename(temporary, directory)
    if not renamed then
      assert(vim.uv.fs_lstat(directory), 'pdfterm: could not publish source snapshot: ' .. tostring(rename_error))
      existing = load_document(path, pdf, info.revision) -- Another editor won immutable publication.
    end
  end)
  if not ok or existing then
    local removed = vim.fn.delete(temporary, 'rf')
    if removed ~= 0 then
      error('pdfterm: could not remove incomplete source snapshot ' .. temporary
        .. (not ok and ': ' .. tostring(failure) or ''))
    end
    if not ok then error(failure) end
    return existing
  end
  for original, entry in pairs(document.files) do
    entry.cache_file = directory .. '/' .. manifest.files[original]
    entry.source.cache_file, entry.source.document = entry.cache_file, true
    if not entry.source.buffer then entry.source.text = nil end
  end
  return remember(document)
end

local function metadata(info)
  assert(type(info) == 'table' and absolute(info.pdf) == info.pdf and absolute(info.cwd) == info.cwd
    and type(info.revision) == 'table' and type(info.revision.pdf) == 'table'
    and type(info.inputs) == 'table' and vim.islist(info.inputs),
    'pdfterm: invalid compiled source metadata')
  assert(info.main == nil or info.main == vim.NIL or absolute(info.main) == info.main,
    'pdfterm: invalid compiled main source identity')
  for _, path in ipairs(info.inputs) do
    assert(absolute(path) == path, 'pdfterm: invalid compiled input identity')
  end
end

local function input_paths(project, info)
  local paths = {}
  for _, path in ipairs(info.inputs) do
    paths[path] = true
  end
  if not next(paths) then
    local main = absolute(type(info.main) == 'string' and info.main or project.main, info.cwd)
    if main then paths[main] = true end
  end
  return paths
end

function M.publish(capture, info)
  assert(type(capture) == 'table' and capture.pdf, 'pdfterm: invalid source capture')
  if pending[capture.pdf] ~= capture or capture.released or not info then
    release(capture)
    if pending[capture.pdf] == capture then pending[capture.pdf] = nil end
    return latest[capture.pdf]
  end
  metadata(info)
  local pdf = assert(absolute(info.pdf, info.cwd))
  assert(pdf == capture.pdf, 'pdfterm: build output does not match captured PDF')
  local prior = latest[pdf]
  if prior and stable(prior.revision) == stable(info.revision) then
    release(capture)
    pending[pdf] = nil
    return prior -- A no-output build cannot rebase the existing compiled source.
  end
  local sources, warnings = {}, {}
  for path in pairs(input_paths(capture.project, info)) do
    local source = capture.sources[path]
    if source then
      local after, read_error, revision = source_bytes(path)
      if not after or vim.fn.sha256(after) ~= source.digest
        or stable(revision) ~= stable(source.saved_revision) then
        warnings[path] = 'source changed or became unavailable during compilation; using its pre-build anchor'
          .. (read_error and ': ' .. read_error or '')
      end
    else
      local text, read_error = source_bytes(path)
      source = text and new_source(path, text, source_buffer(path)) or warm[path]
      if source then capture.sources[path] = source end
      warnings[path] = capture.warnings[path]
        or 'compiler input was not captured before compilation; compiled source provenance is unknown'
      if not source then warnings[path] = warning(warnings[path], read_error) end
    end
    if #info.inputs == 0 then
      warnings[path] = warning(warnings[path], warning(info.warning,
        'compiled input list is unavailable; using a source anchor'))
    end
    if source then sources[path] = source end
  end
  if not next(sources) then release(capture); pending[pdf] = nil; return nil end
  local document = persist(capture.project, info, sources, warnings)
  if not next(document.files) then release(capture); pending[pdf] = nil; return nil end
  for path in pairs(document.files) do
    if warm[path] and not warm[path].document then
      detach_source(warm[path])
      warm[path] = nil
    end
  end
  latest[pdf] = document
  release(capture)
  pending[pdf] = nil
  return document
end

local function captured_document(document)
  if document then
    for _, entry in pairs(document.files) do
      if entry.warning and entry.warning:find('compiled source baseline is unverified', 1, true) == 1 then
        return nil
      end
    end
  end
  return document
end

function M.ensure(project, info)
  metadata(info)
  local pdf = assert(absolute(info.pdf, info.cwd), 'pdfterm: missing source metadata PDF')
  assert(type(info.revision) == 'table' and type(info.revision.pdf) == 'table',
    'pdfterm: missing source metadata revision')
  local key = document_key(pdf, info.revision)
  local document = documents[key]
  if not document then
    local path = cache_root() .. '/' .. key .. '/manifest.json'
    if vim.uv.fs_stat(vim.fs.dirname(path)) then
      document = load_document(path, pdf, info.revision)
    else
      return nil
    end
  end
  document = captured_document(document)
  if not document then return nil end
  if not next(document.files) then return nil end
  latest[pdf] = document
  return document
end

local function attach_known(source, requested)
  if requested and attach(source, requested) then return requested end
  local buffer = source.buffer
  if buffer and vim.api.nvim_buf_is_valid(buffer) then
    if not vim.api.nvim_buf_is_loaded(buffer) then
      local ok = pcall(vim.fn.bufload, buffer)
      if not ok then return nil end
    end
    if attach(source, buffer) then return buffer end
  end
  buffer = loaded_buffer(source.live_file)
  if not buffer and source.live_file and vim.uv.fs_stat(source.live_file) then
    buffer = vim.fn.bufadd(source.live_file)
    local ok = pcall(vim.fn.bufload, buffer)
    if not ok then return nil end
  end
  if buffer and attach(source, buffer) then return buffer end
end

local function unchanged_anchor(source, mark)
  local offset = source.by_id[mark[1]]
  if offset == nil or source.inferred or source.retired[mark[1]] or mark[4].invalid then return false end
  local row = original_position(source, offset)
  local original = source.lines[row + 1]
  if original == '' or mark[2] ~= mark[4].end_row then return false end
  if mark[4].end_col - mark[3] ~= #original then return false end
  local text = vim.api.nvim_buf_get_text(source.buffer, mark[2], mark[3], mark[4].end_row, mark[4].end_col, {})
  return #text == 1 and text[1] == original
end

local function source_for(document, buffer)
  local path = absolute(vim.api.nvim_buf_get_name(buffer), nil, true)
  local direct = path and document.files[path]
  if direct and direct.source.buffer == buffer then
    attach(direct.source, buffer)
    return path, direct
  end
  for _, key in ipairs(sorted_keys(document.files)) do
    local entry = document.files[key]
    if entry.source.buffer == buffer then attach(entry.source, buffer); return key, entry end
  end
  if direct and attach(direct.source, buffer) then return path, direct end
end

local function default_entry(document)
  if document.files[document.main] then return document.main, document.files[document.main] end
  local path = sorted_keys(document.files)[1]
  return path, path and document.files[path]
end

local function mapped_forward(document, path, entry, offset, coarse, reason)
  local row, byte = original_position(entry.source, offset)
  local provenance = warning(entry.warning, entry.source.inferred)
  return { file = path, line = row + 1,
    column = scalar_column(entry.source.lines[row + 1], byte),
    source_snapshot = document.source_snapshot,
    coarse = coarse or provenance ~= nil,
    warning = warning(provenance, reason) }
end

function M.forward(document, buffer, row_zero, byte_column)
  assert(type(document) == 'table' and document.source_snapshot, 'pdfterm: missing source document')
  document = load_document(snapshot_identity(document.source_snapshot), document.pdf, document.revision)
  if buffer == 0 then buffer = vim.api.nvim_get_current_buf() end
  if type(buffer) ~= 'number' or not normal_buffer(buffer) or not vim.api.nvim_buf_is_loaded(buffer) then
    return nil, 'Captured source buffer was unloaded or wiped; PDF position unchanged.'
  end
  local path, entry = source_for(document, buffer)
  if not entry then
    local row, column, text = buffer_position(buffer, row_zero, byte_column)
    return {
      file = assert(absolute(vim.api.nvim_buf_get_name(buffer), nil, true), 'pdfterm: missing source identity'),
      line = row + 1, column = scalar_column(text, column),
      source_snapshot = document.source_snapshot, coarse = true,
      warning = 'Source is not tracked among this PDF\'s compiled inputs; using its compiler line if available.',
    }
  end
  local source = entry.source
  cached_contents(entry.cache_file, entry.digest)
  local row, column, text = buffer_position(buffer, row_zero, byte_column)
  local clamped = row ~= row_zero or column ~= byte_column
  local marks = vim.api.nvim_buf_get_extmarks(buffer, source.namespace, { row, 0 }, { row, #text }, {
    details = true, overlap = true,
  })
  for _, mark in ipairs(marks) do
    local details = mark[4]
    if mark[2] == row and mark[3] <= column and details.end_row == row and column < details.end_col
      and unchanged_anchor(source, mark) then
      return mapped_forward(document, path, entry, source.by_id[mark[1]] + column - mark[3], clamped,
        clamped and 'cursor was outside a UTF-8 source boundary; using a clamped source anchor' or nil)
    end
  end
  local before = vim.api.nvim_buf_get_extmarks(buffer, source.namespace, { row, column }, 0, { details = true, limit = 1 })
  local after = vim.api.nvim_buf_get_extmarks(buffer, source.namespace, { row, column }, -1, { details = true, limit = 1 })
  if before[1] then marks[#marks + 1] = before[1] end
  if after[1] then marks[#marks + 1] = after[1] end
  local best, score
  for _, mark in ipairs(marks) do
    local offset = source.by_id[mark[1]]
    if offset ~= nil then
      local details = mark[4]
      local target_row, target_column = mark[2], mark[3]
      if row == details.end_row and (row > target_row or column > target_column) then
        target_row, target_column = details.end_row, details.end_col
      end
      local containing = (row > mark[2] or row == mark[2] and column >= mark[3])
        and (row < details.end_row or row == details.end_row and column < details.end_col)
      local distance = containing and 0
        or math.abs(row - target_row) * (MAX_SOURCE + 1) + math.abs(column - target_column)
      if not score or distance < score or distance == score and offset < source.by_id[best[1]] then
        best, score = mark, distance
      end
    end
  end
  local offset = best and source.by_id[best[1]] or 0
  return mapped_forward(document, path, entry, offset, true,
    'text is inserted, replaced, deleted, or otherwise untracked; using the nearest compiled source anchor')
end

local function live_identity(path)
  local buffer = loaded_buffer(path)
  if buffer then return buffer, absolute(vim.api.nvim_buf_get_name(buffer), nil, true) end
  local candidate
  for source in pairs(tracked) do
    if source.original == path and source.buffer and vim.api.nvim_buf_is_valid(source.buffer)
      and (not candidate or source.id < candidate.id) then candidate = source end
  end
  if candidate then return candidate.buffer, candidate.live_file end
  return nil, path
end

local function approximate_inverse(location, reason, source)
  local original = absolute(location.file) or (source and source.original)
  local buffer, path
  if source then buffer, path = attach_known(source), source.live_file
  else buffer, path = live_identity(original) end
  local row, byte
  if buffer and vim.api.nvim_buf_is_loaded(buffer) then
    row, byte = buffer_position(buffer, coordinate(location.line, 1) - 1, location.byte_column or 0)
  else
    local bytes = path and source_bytes(path)
    local lines = bytes and split_lines(bytes) or { '' }
    row, byte = clamp_position(lines, coordinate(location.line, 1) - 1, location.byte_column or 0)
  end
  return { buffer = buffer, file = path or original, line = row + 1, byte_column = byte,
    warning = warning(location.warning, reason) }
end

local function matching_document(pdf, full_revision)
  pdf = absolute(pdf)
  if not pdf or type(full_revision) ~= 'table' or type(full_revision.pdf) ~= 'table' then return nil end
  local key = document_key(pdf, full_revision)
  local known = documents[key]
  local path = known and known.source_snapshot
  if not path then
    local root = absolute(vim.fn.stdpath('cache') .. '/pdfterm/sources', nil, true)
    local candidate = root .. '/' .. key .. '/manifest.json'
    local stat, reason, code = vim.uv.fs_lstat(vim.fs.dirname(candidate))
    assert(stat or code == 'ENOENT', 'pdfterm: source snapshot lookup failed: ' .. tostring(reason))
    if stat then path = candidate end
  end
  if path then return captured_document(load_document(path, pdf, full_revision)) end
end

function M.snapshot(pdf, full_revision)
  local document = matching_document(pdf, full_revision)
  return document and next(document.files) and document.source_snapshot or nil
end

function M.inverse(location)
  assert(type(location) == 'table' and absolute(location.file), 'pdfterm: invalid inverse source location')
  local explicit = location.source_snapshot ~= nil and location.source_snapshot ~= vim.NIL
  assert(not explicit or type(location.source_snapshot) == 'string' and location.source_snapshot:sub(1, 1) == '/',
    'pdfterm: source snapshot path must be absolute')
  local snapshot = explicit and snapshot_identity(location.source_snapshot) or nil
  assert(not explicit or snapshot, 'pdfterm: invalid source snapshot path')
  local pdf = absolute(location.pdf)
  local full_revision = location.document_revision ~= vim.NIL and location.document_revision or nil
  local document
  if snapshot then document = load_document(snapshot, pdf, full_revision)
  else document = matching_document(pdf, full_revision) end
  local original = document and absolute(location.file, document.cwd)
  local entry = document and document.files[original]
  if document and not entry then
    local resolved = absolute(location.file, document.cwd, true)
    if resolved and document.files[resolved] then
      original, entry = resolved, document.files[resolved]
    end
  end
  local main
  if document and not entry then main = select(2, default_entry(document)) end
  local selected = entry or main
  if selected then cached_contents(selected.cache_file, selected.digest) end
  local pdf_revision = location.revision ~= vim.NIL and location.revision or nil
  pdf_revision = pdf_revision or (type(full_revision) == 'table' and full_revision.pdf)
  local identity = document and pdf and pdf == document.pdf and type(pdf_revision) == 'table'
    and stable(pdf_revision) == stable(document.revision.pdf)
    and (not full_revision or stable(full_revision) == stable(document.revision))
  assert(not explicit or identity, 'pdfterm: inverse source snapshot document identity mismatch')
  if not identity then
    return approximate_inverse(location, 'compiled source snapshot/identity is unavailable; using a clamped current position')
  end
  if not entry then
    if main then
      return approximate_inverse({ file = main.source.original, line = 1, byte_column = 0, warning = location.warning },
        'inverse source was not captured among compiled inputs; using the main/document anchor', main.source)
    end
    return approximate_inverse(location, 'inverse source was not captured; using a clamped current position')
  end
  local source, provenance = entry.source, warning(location.warning, entry.warning)
  local original_lines = lines_for(source)
  local requested_row = coordinate(location.line, 1) - 1
  local row = math.max(0, math.min(math.floor(requested_row), #original_lines - 1))
  local requested_byte = location.byte_column
  if requested_byte == nil then requested_byte = scalar_byte(original_lines[row + 1], location.column or location.column_char or 1) end
  local byte = byte_boundary(original_lines[row + 1], requested_byte)
  if row ~= requested_row or byte ~= requested_byte then
    provenance = warning(provenance, 'compiled location was outside a UTF-8 source boundary; using a clamped anchor')
  end
  local buffer = attach_known(source)
  if not buffer then
    return approximate_inverse(location, warning(provenance, 'live source is unavailable; using a clamped current position'), source)
  end
  local id = source.by_offset[source.starts[row + 1]]
  local position = id and vim.api.nvim_buf_get_extmark_by_id(buffer, source.namespace, id, { details = true }) or {}
  if #position == 0 then
    return approximate_inverse(location, warning(provenance, 'compiled source anchor is unavailable; using a clamped current position'), source)
  end
  local mark = { id, position[1], position[2], position[3] }
  local mapped_byte = position[2]
  if unchanged_anchor(source, mark) then
    mapped_byte = mapped_byte + byte
    if byte == #original_lines[row + 1] then
      provenance = warning(provenance, 'compiled location is a line boundary; using its current source anchor')
    end
  else
    provenance = warning(provenance, source.inferred
      or 'compiled line was deleted, replaced, or changed; using its current source range boundary')
  end
  local live_row, live_byte = buffer_position(buffer, position[1], mapped_byte)
  return { buffer = buffer, file = source.live_file, line = live_row + 1, byte_column = live_byte, warning = provenance }
end

return M
