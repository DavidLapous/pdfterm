-- Run: nvim --headless -u NONE -l tests/nvim_source.lua
-- Real files, filesystem revision metadata, and native Neovim edits/extmarks only.
local root, api = vim.fn.getcwd(), vim.api
vim.opt.runtimepath:prepend(root)
vim.fn.mkdir(root .. '/target', 'p')
local replay = vim.env.PDFTERM_SOURCE_CACHE_REPLAY
replay = replay and vim.json.decode(replay) or nil
local directory = replay and replay.directory or assert(vim.uv.fs_mkdtemp(root .. '/target/nvim-source-XXXXXX'))
vim.env.XDG_CACHE_HOME = directory .. '/cache'
vim.opt.swapfile = false
vim.opt.hidden = true
local source = require('pdfterm.source')
source.setup(api.nvim_create_augroup('pdfterm-source-test', { clear = true }))
local revision_number = 0

local function write_bytes(path, bytes)
  local fd = assert(vim.uv.fs_open(path, 'w', 384))
  assert(vim.uv.fs_write(fd, bytes, 0) == #bytes)
  assert(vim.uv.fs_close(fd))
end

local function read_bytes(path)
  local fd = assert(vim.uv.fs_open(path, 'r', 0))
  local bytes = assert(vim.uv.fs_read(fd, assert(vim.uv.fs_fstat(fd)).size, 0))
  assert(vim.uv.fs_close(fd))
  return bytes
end

-- A valid one-page PDF fixture. Anchor tests need metadata, not a renderer.
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
  for index = 2, #offsets do bytes = bytes .. string.format('%010d 00000 n \n', offsets[index]) end
  write_bytes(path, bytes .. 'trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n' .. xref .. '\n%%EOF\n')
end

local function pdf_revision(path)
  local stat = assert(vim.uv.fs_stat(path))
  return {
    device = stat.dev, inode = stat.ino, length = stat.size,
    modified_seconds = stat.mtime.sec, modified_nanoseconds = stat.mtime.nsec,
    changed_seconds = stat.ctime.sec, changed_nanoseconds = stat.ctime.nsec,
  }
end

local function info(project, inputs)
  local companion = project.pdf:gsub('%.pdf$', '.synctex')
  return {
    pdf = assert(vim.uv.fs_realpath(project.pdf)), cwd = directory, main = project.main,
    inputs = inputs or { project.main },
    revision = { pdf = pdf_revision(project.pdf),
      companion = vim.uv.fs_stat(companion) and { false, pdf_revision(companion) } or vim.NIL },
  }
end

local function finish(project, capture, inputs)
  revision_number = revision_number + 1
  vim.fn.writefile({ '% revision ' .. revision_number }, project.pdf, 'a')
  return assert(source.publish(capture, info(project, inputs)))
end

local function fresh(name, lines)
  local project = { main = directory .. '/' .. name .. '.tex',
    pdf = directory .. '/' .. name .. '.pdf', cwd = directory }
  vim.fn.writefile(lines, project.main)
  pdf_fixture(project.pdf)
  vim.cmd.edit(vim.fn.fnameescape(project.main))
  local buffer = api.nvim_get_current_buf()
  vim.bo[buffer].bufhidden = 'hide'
  return project, buffer, finish(project, source.capture(project, { project.main }))
end

local function location(document, file, line, byte)
  return { file = file, line = line, byte_column = byte, pdf = document.pdf,
    revision = vim.deepcopy(document.revision.pdf), document_revision = vim.deepcopy(document.revision),
    source_snapshot = document.source_snapshot }
end

local function implicit_location(document, file, line, byte)
  local value = location(document, file, line, byte)
  value.source_snapshot = nil
  return value
end

local function precise(result, line, column)
  assert(result.coarse == false and result.warning == nil, vim.inspect(result))
  assert(result.line == line and result.column == column, vim.inspect(result))
end

local function warned(result)
  assert(type(result.warning) == 'string' and #result.warning > 0, vim.inspect(result))
  if result.coarse ~= nil then assert(result.coarse, vim.inspect(result)) end
  return result
end

local function operational(callback, expected)
  local success, reason = pcall(callback)
  assert(not success, tostring(reason))
  local message = tostring(reason):match('pdfterm:.*') or tostring(reason)
  assert(not expected or message:find(expected, 1, true), message)
  return message
end

local function cached_text(document, path)
  local manifest = vim.json.decode(read_bytes(document.source_snapshot))
  assert(manifest.pdf == document.pdf and manifest.main:sub(1, 1) == '/')
  assert(bit.band(assert(vim.uv.fs_stat(vim.fs.dirname(document.source_snapshot))).mode, 511) == 448)
  local cached = vim.fs.dirname(document.source_snapshot) .. '/' .. assert(manifest.files[path])
  assert(bit.band(assert(vim.uv.fs_stat(cached)).mode, 511) == 384)
  local bytes = read_bytes(cached)
  assert(type(manifest.digests) == 'table' and manifest.digests[path] == vim.fn.sha256(bytes))
  for original in pairs(manifest.files) do
    local digest = manifest.digests[original]
    assert(type(digest) == 'string' and #digest == 64 and not digest:find('[^0-9a-f]'))
  end
  for original in pairs(manifest.digests) do assert(manifest.files[original] ~= nil) end
  return bytes, manifest
end

local function snapshot_request(document)
  return { action = 'source_snapshot', pdf = document.pdf, document_revision = vim.deepcopy(document.revision) }
end

local function snapshot_query(request, callback)
  local socket, delivered = require('pdfterm.socket'), 0
  local path = directory .. '/snapshot.sock'
  local close = socket.listen(path, function() delivered = delivered + 1 end,
    callback ~= false and (callback or source.snapshot) or nil)
  local done, failure, result = false
  socket.request(path, vim.json.encode(request), function(error, connection_error, reply)
    assert(not connection_error, tostring(error))
    done, failure, result = true, error, reply
  end, 3000)
  local completed = vim.wait(4000, function() return done end, 5)
  close()
  assert(completed and delivered == 0, 'snapshot query failed to complete without navigation')
  return failure, result
end

local function delayed_socket_dispatch(request, query, shutdown)
  local socket, calls, queued = require('pdfterm.socket'), 0, {}
  local path = directory .. '/delayed-snapshot.sock'
  local close = socket.listen(path, function() calls = calls + 1 end, function()
    calls = calls + 1
    return nil
  end)
  local client, chunks, complete = assert(vim.uv.new_pipe(false)), {}, false
  local schedule = vim.schedule
  vim.schedule = function(callback) queued[#queued + 1] = callback end
  client:connect(path, function(error)
    assert(not error, tostring(error))
    client:read_start(function(read_error, chunk)
      assert(not read_error, tostring(read_error))
      if chunk then chunks[#chunks + 1] = chunk
      else complete = true; client:close() end
    end)
    client:write(vim.json.encode(request), function(write_error)
      assert(not write_error, tostring(write_error))
      client:shutdown(function(shutdown_error) assert(not shutdown_error, tostring(shutdown_error)) end)
    end)
  end)
  local pending = vim.wait(1000, function() return #queued > 0 end, 5)
  vim.wait(1100, function() return false end, 10)
  vim.schedule = schedule
  if shutdown then close() end
  for _, callback in ipairs(queued) do callback() end
  local completed = vim.wait(2000, function() return complete end, 5)
  close()
  if not client:is_closing() then client:close() end
  assert(pending and completed, 'accepted EOF did not survive delayed main-loop dispatch')
  assert(calls == (shutdown and 0 or 1), 'delayed socket request was lost or dispatched after shutdown')
  if query and not shutdown then
    local reply = vim.json.decode(table.concat(chunks))
    assert(reply.ok == true and reply.source_snapshot == vim.NIL, vim.inspect(reply))
  else
    assert(#chunks == 0, 'ordinary inverse delivery unexpectedly replied')
  end
end

if replay then
  local child_ok, child_failure = xpcall(function()
    if replay.mode == 'fifo' then
      local started = vim.uv.hrtime()
      local text, reason = source.read_input(replay.fifo)
      assert(text == nil and type(reason) == 'string' and #reason > 0, tostring(reason))
      local capture = source.capture(replay.project, { replay.project.main, replay.fifo })
      assert(capture.warnings[replay.fifo])
      source.publish(capture, nil)
      operational(function() source.inverse(replay.fifo_location) end)
      assert((vim.uv.hrtime() - started) / 1e9 < 2, 'FIFO source operation blocked the event loop')
      print('nonblocking FIFO source replay passed')
      return
    end
    if replay.mode == 'legacy-snapshot' then
      assert(source.snapshot(replay.info.pdf, replay.info.revision) == nil)
      assert(source.ensure(replay.project, replay.info) == nil)
      local query_error, query_reply = snapshot_query(snapshot_request(replay.info))
      assert(not query_error and query_reply.source_snapshot == vim.NIL, tostring(query_error))
      return
    end
    local query_error, query_reply = snapshot_query(snapshot_request(replay.info))
    assert(not query_error and type(query_reply.source_snapshot) == 'string', tostring(query_error))
    vim.cmd.edit(vim.fn.fnameescape(replay.project.main))
    local buffer = api.nvim_get_current_buf()
    local document = assert(source.ensure(replay.project, replay.info))
    if replay.mode == 'cold-tamper' then
      local text, manifest = cached_text(document, replay.cold_file)
      local path = vim.fs.dirname(document.source_snapshot) .. '/' .. manifest.files[replay.cold_file]
      local changed = text:gsub('Cold', 'Gold', 1)
      assert(changed ~= text and #changed == #text)
      write_bytes(path, changed)
      local implicit = vim.deepcopy(replay.cold_location)
      implicit.source_snapshot = nil
      local explicit_error = operational(function() source.inverse(replay.cold_location) end,
        'cached source changed after publication')
      assert(operational(function() source.inverse(implicit) end) == explicit_error)
      local cold_buffer = api.nvim_create_buf(true, false)
      api.nvim_buf_set_name(cold_buffer, replay.cold_file)
      api.nvim_buf_set_lines(cold_buffer, 0, -1, false, vim.split(text:sub(1, -2), '\n', { plain = true }))
      operational(function() source.forward(document, cold_buffer, 0, 5) end,
        'cached source changed after publication')
      write_bytes(path, text)
      precise(source.forward(document, cold_buffer, 0, 5), 1, 6)
      local restored = source.inverse(implicit)
      assert(restored.buffer == cold_buffer and restored.line == 1
        and restored.byte_column == 5 and not restored.warning)
      return
    end
    if replay.mode == 'cold-cache' then
      vim.cmd.edit(vim.fn.fnameescape(replay.cold_file))
      local cold_buffer = api.nvim_get_current_buf()
      precise(source.forward(document, cold_buffer, 0, 5), 1, 6)
      local before = source.inverse(replay.cold_location)
      assert(before.buffer == cold_buffer and before.line == 1 and before.byte_column == 5 and not before.warning)
      local text, manifest = cached_text(document, replay.cold_file)
      local path = vim.fs.dirname(document.source_snapshot) .. '/' .. manifest.files[replay.cold_file]
      write_bytes(path, text .. 'Changed immutable source\n')
      operational(function() source.inverse(replay.cold_location) end)
      write_bytes(path, text)
      local restored = source.inverse(replay.cold_location)
      assert(restored.buffer == cold_buffer and restored.line == 1 and restored.byte_column == 5 and not restored.warning)
      return
    end
    precise(source.forward(document, buffer, 0, replay.live_byte), 1, replay.expected_column)
    local inverse = source.inverse(location(document, replay.project.main, 1, replay.live_byte))
    assert(inverse.buffer == buffer and inverse.byte_column == replay.live_byte and not inverse.warning)
    warned(source.inverse(replay.old_location))
    print('persisted source cache replay passed')
  end, debug.traceback)
  if not child_ok then io.stderr:write(child_failure .. '\n'); vim.cmd('cquit 1')
  else vim.cmd('qa!') end
  return
end

local ok, failure = xpcall(function()
  -- Native line-range gravity preserves line shifts and prefix column shifts.
  local p, buffer, document = fresh('shift', { 'Header', 'Alpha needle Omega', 'Footer' })
  precise(source.forward(document, buffer, 1, 6), 2, 7)
  for name, namespace in pairs(api.nvim_get_namespaces()) do
    if name:match('^pdfterm%.source%.') then
      assert(#api.nvim_buf_get_extmarks(buffer, namespace, 0, -1, {}) <= 3,
        'source range density exceeded one mark per compiled line')
    end
  end
  api.nvim_buf_set_text(buffer, 1, 0, 1, 0, { 'wide ' })
  precise(source.forward(document, buffer, 1, 11), 2, 7)
  api.nvim_buf_set_lines(buffer, 0, 0, false, { 'Inserted source only' })
  precise(source.forward(document, buffer, 2, 11), 2, 7)
  local inverse = source.inverse(location(document, p.main, 2, 6))
  assert(inverse.buffer == buffer and inverse.line == 3 and inverse.byte_column == 11 and not inverse.warning)
  inverse = source.inverse(implicit_location(document, p.main, 2, 6))
  assert(inverse.buffer == buffer and inverse.line == 3 and inverse.byte_column == 11 and not inverse.warning)
  warned(source.forward(document, buffer, 0, 1))
  api.nvim_buf_set_text(buffer, 2, 13, 2, 13, { '++' })
  warned(source.forward(document, buffer, 2, 13))
  warned(source.forward(document, buffer, 2, 15))
  api.nvim_buf_set_text(buffer, 2, 13, 2, 15, { '' })
  precise(source.forward(document, buffer, 2, 13), 2, 9)
  assert(cached_text(document, p.main) == 'Header\nAlpha needle Omega\nFooter\n')

  -- Saved live edits do not mutate the compiled snapshot. Unload/reload restores
  -- existing ranges, not raw old line numbers or freshly inferred precision.
  vim.cmd.write()
  vim.cmd.enew()
  api.nvim_buf_delete(buffer, { unload = true })
  assert(not api.nvim_buf_is_loaded(buffer))
  local missing, unavailable = source.forward(document, buffer, 2, 13)
  assert(missing == nil and unavailable ~= nil)
  vim.fn.bufload(buffer)
  precise(source.forward(document, buffer, 2, 13), 2, 9)
  inverse = source.inverse(location(document, p.main, 2, 8))
  assert(inverse.buffer == buffer and inverse.line == 3 and inverse.byte_column == 13 and not inverse.warning)
  api.nvim_set_current_buf(buffer)
  vim.fn.writefile({ 'Repeated needle', 'Repeated needle', 'Alpha needle Omega' }, p.main)
  vim.cmd('edit!')
  warned(source.forward(document, buffer, 2, 6))
  warned(source.inverse(location(document, p.main, 2, 6)))
  assert(cached_text(document, p.main) == 'Header\nAlpha needle Omega\nFooter\n')

  local untracked_file = directory .. '/not-compiled.tex'
  vim.fn.writefile({ 'Only in this live source', 'Detached word is not part of the PDF' }, untracked_file)
  vim.cmd.edit(vim.fn.fnameescape(untracked_file))
  local detached = warned(source.forward(document, api.nvim_get_current_buf(), 1, 9))
  assert(detached.file == untracked_file and detached.line == 2 and detached.column == 10)
  api.nvim_set_current_buf(buffer)

  -- Interior edits are coarse. Entire-line equality permits a byte mapping
  -- again, but deleting/reinserting the whole line cannot revive invalid marks.
  local replaced, replaced_buffer, replaced_document = fresh('replace', { 'Before', 'Copper needle Silver', 'After' })
  api.nvim_buf_set_text(replaced_buffer, 1, 7, 1, 13, { 'fresh' })
  warned(source.forward(replaced_document, replaced_buffer, 1, 8))
  inverse = warned(source.inverse(location(replaced_document, replaced.main, 2, 8)))
  assert(inverse.buffer == replaced_buffer and inverse.line == 2 and inverse.byte_column == 0)
  api.nvim_buf_set_text(replaced_buffer, 1, 7, 1, 12, { 'needle' })
  precise(source.forward(replaced_document, replaced_buffer, 1, 8), 2, 9)
  api.nvim_buf_set_lines(replaced_buffer, 1, 2, false, {})
  api.nvim_buf_set_lines(replaced_buffer, 1, 1, false, { 'Copper needle Silver' })
  warned(source.forward(replaced_document, replaced_buffer, 1, 8))
  precise(source.forward(replaced_document, replaced_buffer, 0, 1), 1, 2)

  -- Actual editor undo/redo restores native invalidation, not a Lua delta map.
  local deleted, deleted_buffer, deleted_document = fresh('delete', { 'Deleted target', 'Surviving boundary', 'Last' })
  vim.cmd('normal! ggdd')
  inverse = warned(source.inverse(location(deleted_document, deleted.main, 1, 3)))
  assert(inverse.line == 1 and inverse.byte_column == 0, vim.inspect(inverse))
  vim.cmd.undo()
  precise(source.forward(deleted_document, deleted_buffer, 0, 3), 1, 4)
  inverse = source.inverse(location(deleted_document, deleted.main, 1, 3))
  assert(inverse.line == 1 and inverse.byte_column == 3 and not inverse.warning)
  inverse = source.inverse(implicit_location(deleted_document, deleted.main, 1, 3))
  assert(inverse.line == 1 and inverse.byte_column == 3 and not inverse.warning)
  vim.cmd.redo()
  inverse = warned(source.inverse(location(deleted_document, deleted.main, 1, 3)))
  assert(inverse.line == 1 and inverse.byte_column == 0)

  -- UTF-8 uses byte boundaries in buffers and Unicode-scalar columns in SyncTeX.
  local unicode, unicode_buffer, unicode_document = fresh('unicode', { 'Aé𝑠é Z', '終点' })
  precise(source.forward(unicode_document, unicode_buffer, 0, 3), 1, 3)
  precise(source.forward(unicode_document, unicode_buffer, 0, 8), 1, 5)
  api.nvim_buf_set_text(unicode_buffer, 0, 0, 0, 0, { 'λ ' })
  precise(source.forward(unicode_document, unicode_buffer, 0, 6), 1, 3)
  precise(source.forward(unicode_document, unicode_buffer, 0, 11), 1, 5)
  inverse = source.inverse(location(unicode_document, unicode.main, 1, 3))
  assert(inverse.byte_column == 6 and not inverse.warning)
  inverse = warned(source.inverse(location(unicode_document, unicode.main, 1, 4)))
  assert(inverse.byte_column == 6)
  warned(source.forward(unicode_document, unicode_buffer, 0, 7))
  local fallback = warned(source.inverse({ file = unicode.main, line = 999, byte_column = 2 }))
  assert(fallback.line == 2 and fallback.byte_column == 0)

  -- Known renamed buffers keep the compiler's original identity and never open
  -- the stale pathname (which has actually disappeared from the filesystem).
  local renamed, renamed_buffer, renamed_document = fresh('rename', { 'Stable needle' })
  local old_name, new_name = renamed.main, directory .. '/renamed-live.tex'
  assert(vim.uv.fs_rename(old_name, new_name))
  api.nvim_buf_set_name(renamed_buffer, new_name)
  local forward = source.forward(renamed_document, renamed_buffer, 0, 7)
  precise(forward, 1, 8)
  assert(forward.file == old_name)
  inverse = source.inverse(location(renamed_document, old_name, 1, 7))
  assert(inverse.buffer == renamed_buffer and inverse.file == new_name and not inverse.warning)
  inverse = source.inverse(implicit_location(renamed_document, old_name, 1, 7))
  assert(inverse.buffer == renamed_buffer and inverse.file == new_name and not inverse.warning)
  assert(not vim.uv.fs_stat(old_name))
  api.nvim_buf_delete(renamed_buffer, { force = true })
  missing, unavailable = source.forward(renamed_document, renamed_buffer, 0, 7)
  assert(missing == nil and unavailable ~= nil)
  inverse = source.inverse(location(renamed_document, old_name, 1, 7))
  assert(inverse.buffer and inverse.buffer ~= renamed_buffer and inverse.file == new_name and not inverse.warning)
  assert(not vim.uv.fs_stat(old_name))

  -- The same live file can belong to independent PDFs and historical revisions.
  local multiple, multiple_buffer, old_document = fresh('multiple', { 'Alpha needle Omega' })
  api.nvim_buf_set_text(multiple_buffer, 0, 0, 0, 0, { 'Current ' })
  vim.cmd.write()
  local second_project = { main = multiple.main, cwd = directory, pdf = directory .. '/second.pdf' }
  pdf_fixture(second_project.pdf)
  local second_document = finish(second_project, source.capture(second_project, { multiple.main }))
  assert(old_document.source_snapshot ~= second_document.source_snapshot)
  precise(source.forward(old_document, multiple_buffer, 0, 14), 1, 7)
  precise(source.forward(second_document, multiple_buffer, 0, 14), 1, 15)
  local new_document = finish(multiple, source.capture(multiple, { multiple.main }))
  assert(old_document.source_snapshot ~= new_document.source_snapshot)
  precise(source.forward(new_document, multiple_buffer, 0, 14), 1, 15)
  inverse = source.inverse(location(old_document, multiple.main, 1, 6))
  assert(inverse.byte_column == 14 and not inverse.warning)
  assert(cached_text(old_document, multiple.main) == 'Alpha needle Omega\n')
  assert(cached_text(new_document, multiple.main) == 'Current Alpha needle Omega\n')
  local exact_epoch = location(old_document, multiple.main, 1, 6)
  exact_epoch.source_snapshot = nil
  inverse = source.inverse(exact_epoch)
  assert(inverse.byte_column == 14 and not inverse.warning)
  exact_epoch.document_revision = nil
  warned(source.inverse(exact_epoch)) -- PdfRevision alone must not select a source epoch.
  local companion = multiple.pdf:gsub('%.pdf$', '.synctex')
  write_bytes(companion, 'SyncTeX Version:1\nInput:1:' .. multiple.main .. '\n')
  local sidecar_document = assert(source.publish(source.capture(multiple, { multiple.main }), info(multiple)))
  assert(sidecar_document.source_snapshot ~= new_document.source_snapshot)
  local wrong_epoch = location(new_document, multiple.main, 1, 14)
  wrong_epoch.document_revision = sidecar_document.revision
  operational(function() source.inverse(wrong_epoch) end)
  local query_error, query_reply
  local before_query = {
    buffer = api.nvim_get_current_buf(), cursor = api.nvim_win_get_cursor(0),
    text = api.nvim_buf_get_lines(multiple_buffer, 0, -1, false),
    tick = api.nvim_buf_get_changedtick(multiple_buffer), modified = vim.bo[multiple_buffer].modified,
    disk = read_bytes(multiple.main), register = vim.fn.getreg('"'),
  }
  for _, selected in ipairs({ old_document, new_document, sidecar_document }) do
    assert(source.snapshot(multiple.pdf, selected.revision) == selected.source_snapshot)
    query_error, query_reply = snapshot_query(snapshot_request(selected))
    assert(not query_error and query_reply.source_snapshot == selected.source_snapshot, tostring(query_error))
  end
  local unavailable = snapshot_request(sidecar_document)
  unavailable.document_revision.companion[2].inode = unavailable.document_revision.companion[2].inode + 1
  query_error, query_reply = snapshot_query(unavailable)
  assert(not query_error and query_reply.source_snapshot == vim.NIL, tostring(query_error))
  for _, invalidate in ipairs({
    function(value) value.pdf = 'relative.pdf' end,
    function(value) value.document_revision = nil end,
    function(value) value.document_revision.companion = { 'invalid', value.document_revision.pdf } end,
  }) do
    local invalid = snapshot_request(sidecar_document)
    invalidate(invalid)
    query_error = snapshot_query(invalid)
    assert(query_error == 'invalid source snapshot request', tostring(query_error))
  end
  query_error = snapshot_query(snapshot_request(sidecar_document), function() error('required authority failed') end)
  assert(query_error and query_error:find('required authority failed', 1, true), tostring(query_error))
  query_error = snapshot_query(snapshot_request(sidecar_document), false)
  assert(query_error == 'source snapshot lookup is unavailable', tostring(query_error))
  delayed_socket_dispatch({ file = multiple.main, line = 1, byte_column = 0 }, false, false)
  delayed_socket_dispatch(snapshot_request(sidecar_document), true, false)
  delayed_socket_dispatch(snapshot_request(sidecar_document), true, true)
  assert(api.nvim_get_current_buf() == before_query.buffer
    and vim.deep_equal(api.nvim_win_get_cursor(0), before_query.cursor)
    and vim.deep_equal(api.nvim_buf_get_lines(multiple_buffer, 0, -1, false), before_query.text)
    and api.nvim_buf_get_changedtick(multiple_buffer) == before_query.tick
    and vim.bo[multiple_buffer].modified == before_query.modified
    and read_bytes(multiple.main) == before_query.disk and vim.fn.getreg('"') == before_query.register,
    'source snapshot lookup changed live editor state')

  -- Failed builds, unchanged output and superseded callbacks cannot rebase or
  -- overwrite previously published source bytes, even after saved edits.
  local safety, safety_buffer, safety_document = fresh('safety', { 'Alpha needle Omega' })
  api.nvim_buf_set_text(safety_buffer, 0, 0, 0, 0, { 'Saved ' })
  vim.cmd.write()
  assert(source.publish(source.capture(safety, { safety.main }), nil) == safety_document)
  assert(source.ensure(safety, info(safety)) == safety_document)
  assert(source.publish(source.capture(safety, { safety.main }), info(safety)) == safety_document)
  assert(cached_text(safety_document, safety.main) == 'Alpha needle Omega\n')
  local stale_capture = source.capture(safety, { safety.main })
  local stale_info = info(safety)
  local active_capture = source.capture(safety, { safety.main })
  local active_document = finish(safety, active_capture)
  assert(source.publish(stale_capture, stale_info) == active_document)
  assert(source.ensure(safety, info(safety)) == active_document)
  precise(source.forward(active_document, safety_buffer, 0, 12), 1, 13)
  assert(cached_text(safety_document, safety.main) == 'Alpha needle Omega\n')

  -- Unsaved edits during compilation retain precise old anchors. A saved input
  -- changing while the compiler runs has genuinely unknown provenance and WARN.
  local during, during_buffer = fresh('during', { 'Alpha needle Omega' })
  local during_capture = source.capture(during, { during.main })
  api.nvim_buf_set_text(during_buffer, 0, 0, 0, 0, { 'Typed ' })
  local during_document = finish(during, during_capture)
  precise(source.forward(during_document, during_buffer, 0, 12), 1, 7)
  warned(source.forward(during_document, during_buffer, 0, 1))
  vim.cmd.write()
  local changed_capture = source.capture(during, { during.main })
  api.nvim_buf_set_text(during_buffer, 0, 0, 0, 0, { 'Later ' })
  vim.cmd.write()
  local changed_document = finish(during, changed_capture)
  warned(source.forward(changed_document, during_buffer, 0, 18))
  local text, manifest = cached_text(changed_document, during.main)
  assert(text == 'Typed Alpha needle Omega\n' and manifest.warnings[during.main])
  local same_capture = source.capture(during, { during.main })
  local same_text = read_bytes(during.main)
  local replacement_file = directory .. '/same-content.tex'
  write_bytes(replacement_file, same_text)
  assert(vim.uv.fs_rename(replacement_file, during.main))
  local same_document = finish(during, same_capture)
  warned(source.forward(same_document, during_buffer, 0, 18))
  assert(cached_text(same_document, during.main) == same_text)

  -- An unavailable sibling input cannot downgrade a stable captured root.
  local scoped, scoped_buffer = fresh('scoped-warning', { 'Root stable needle' })
  local ephemeral = directory .. '/ephemeral-input.tex'
  vim.fn.writefile({ 'Ephemeral body needle' }, ephemeral)
  vim.cmd.edit(vim.fn.fnameescape(ephemeral))
  local ephemeral_buffer = api.nvim_get_current_buf()
  local scoped_capture = source.capture(scoped, { scoped.main, ephemeral })
  local uncaptured = directory .. '/never-captured.tex'
  assert(vim.uv.fs_unlink(ephemeral))
  vim.fn.writefile({ '% scoped-warning revision' }, scoped.pdf, 'a')
  local scoped_info = info(scoped, { scoped.main, ephemeral, uncaptured })
  scoped_info.warning = 'compiled input is unavailable: ' .. ephemeral
  local scoped_document = assert(source.publish(scoped_capture, scoped_info))
  precise(source.forward(scoped_document, scoped_buffer, 0, 12), 1, 13)
  warned(source.forward(scoped_document, ephemeral_buffer, 0, 15))
  local _, scoped_manifest = cached_text(scoped_document, scoped.main)
  assert(scoped_manifest.warnings[scoped.main] == nil and scoped_manifest.warnings[ephemeral])
  assert(scoped_manifest.warnings[uncaptured] and not scoped_manifest.files[uncaptured]
    and not scoped_manifest.digests[uncaptured])
  local no_map_capture = source.capture(scoped, { scoped.main })
  vim.fn.writefile({ '% no-input-map revision' }, scoped.pdf, 'a')
  local no_map_info = info(scoped, {})
  no_map_info.warning = 'compiled source list unavailable: no SyncTeX sidecar'
  warned(source.forward(assert(source.publish(no_map_capture, no_map_info)), scoped_buffer, 0, 12))

  -- External builds have no captured authority: ordinary saved-source guessing
  -- must not create an unverified snapshot that suppresses word refinement.
  local unknown = { main = directory .. '/unknown.tex', pdf = directory .. '/unknown.pdf', cwd = directory }
  vim.fn.writefile({ 'Old needle context' }, unknown.main)
  pdf_fixture(unknown.pdf)
  vim.cmd.edit(vim.fn.fnameescape(unknown.main))
  local unknown_buffer = api.nvim_get_current_buf()
  api.nvim_buf_set_text(unknown_buffer, 0, 0, 0, 0, { 'New ' })
  vim.cmd.write()
  assert(source.snapshot(unknown.pdf, info(unknown).revision) == nil)
  query_error, query_reply = snapshot_query(snapshot_request(info(unknown)))
  assert(not query_error and query_reply.source_snapshot == vim.NIL, tostring(query_error))
  assert(source.ensure(unknown, info(unknown)) == nil)
  api.nvim_buf_set_text(unknown_buffer, 0, 0, 0, 0, { 'Another ' })
  vim.cmd.write()
  assert(source.ensure(unknown, info(unknown)) == nil)
  local unknown_document = finish(unknown, source.capture(unknown, { unknown.main }))
  assert(source.ensure(unknown, info(unknown)) == unknown_document)
  assert(cached_text(unknown_document, unknown.main) == 'Another New Old needle context\n')

  -- Excluded inverse inputs and missing metadata remain explicit best guesses.
  local orphan = directory .. '/orphan.tex'
  vim.fn.writefile({ 'Not in the compiled PDF' }, orphan)
  vim.cmd.edit(vim.fn.fnameescape(orphan))
  inverse = warned(source.inverse(location(unknown_document, orphan, 1, 4)))
  assert(inverse.buffer == unknown_buffer and inverse.file == unknown.main and inverse.line == 1)
  local no_sidecar = { main = directory .. '/no-sidecar.tex', pdf = directory .. '/no-sidecar.pdf', cwd = directory }
  vim.fn.writefile({ 'Source without historical metadata' }, no_sidecar.main)
  pdf_fixture(no_sidecar.pdf)
  vim.cmd.edit(vim.fn.fnameescape(no_sidecar.main))
  local no_sidecar_info = info(no_sidecar, {})
  no_sidecar_info.main = vim.NIL
  no_sidecar_info.warning = 'compiled source list unavailable: no SyncTeX sidecar'
  assert(source.ensure(no_sidecar, no_sidecar_info) == nil)
  local all_deleted, all_buffer, all_document = fresh('all-deleted', { 'First target', 'Second target' })
  api.nvim_buf_set_lines(all_buffer, 0, -1, false, {})
  inverse = warned(source.inverse(location(all_document, all_deleted.main, 2, 3)))
  assert(inverse.line == 1 and inverse.byte_column == 0)
  warned(source.forward(all_document, all_buffer, 0, 0))
  local broken = directory .. '/invalid-manifest.json'
  write_bytes(broken, '{"files":{}}')
  local broken_location = location(unknown_document, unknown.main, 99, 999)
  broken_location.source_snapshot = broken
  operational(function() source.inverse(broken_location) end)
  local missing_location = location(unknown_document, unknown.main, 1, 0)
  missing_location.source_snapshot = directory .. '/missing-manifest.json'
  operational(function() source.inverse(missing_location) end)
  local manifest_link = directory .. '/manifest-link.json'
  assert(vim.uv.fs_symlink(unknown_document.source_snapshot, manifest_link))
  local linked_location = location(unknown_document, unknown.main, 1, 0)
  linked_location.source_snapshot = manifest_link
  operational(function() source.inverse(linked_location) end)
  local authority_location = location(unknown_document, unknown.main, 1, 0)
  -- The same warm anchor can back later revisions. Each selected entry must
  -- still validate its own cache, not the last cache attached to that anchor.
  local shared_document = finish(unknown, source.capture(unknown, { unknown.main }))
  assert(shared_document.source_snapshot ~= unknown_document.source_snapshot)
  local implicit_authority = implicit_location(unknown_document, unknown.main, 1, 0)
  assert(source.inverse(implicit_authority).buffer == unknown_buffer)
  local source_alias = directory .. '/unknown-alias.tex'
  assert(vim.uv.fs_symlink(unknown.main, source_alias))
  local alias_inverse = source.inverse(location(unknown_document, source_alias, 1, 0))
  assert(alias_inverse.buffer == unknown_buffer and alias_inverse.file == unknown.main and alias_inverse.warning == nil)
  local manifest_backup = unknown_document.source_snapshot .. '.saved'
  assert(vim.uv.fs_rename(unknown_document.source_snapshot, manifest_backup))
  local explicit_error = operational(function() source.inverse(authority_location) end)
  assert(operational(function() source.inverse(implicit_authority) end) == explicit_error)
  query_error = snapshot_query(snapshot_request(unknown_document))
  assert(query_error and query_error:find('source snapshot manifest is not a regular file', 1, true),
    tostring(query_error))
  operational(function() source.forward(unknown_document, unknown_buffer, 0, 0) end)
  local missing_identity = vim.deepcopy(implicit_authority)
  missing_identity.document_revision = nil
  assert(warned(source.inverse(missing_identity)).buffer == unknown_buffer)
  assert(vim.uv.fs_rename(manifest_backup, unknown_document.source_snapshot))
  local manifest_bytes = read_bytes(unknown_document.source_snapshot)
  write_bytes(unknown_document.source_snapshot, '{')
  query_error = snapshot_query(snapshot_request(unknown_document))
  assert(query_error and query_error:find('source snapshot manifest changed after publication', 1, true),
    tostring(query_error))
  write_bytes(unknown_document.source_snapshot, manifest_bytes)
  local cache_text, cache_manifest = cached_text(unknown_document, unknown.main)
  local cache_file = vim.fs.dirname(unknown_document.source_snapshot) .. '/' .. cache_manifest.files[unknown.main]
  local cache_backup = cache_file .. '.saved'
  assert(vim.uv.fs_rename(cache_file, cache_backup))
  explicit_error = operational(function() source.inverse(authority_location) end)
  assert(operational(function() source.inverse(implicit_authority) end) == explicit_error)
  assert(vim.uv.fs_symlink(cache_backup, cache_file))
  explicit_error = operational(function() source.inverse(authority_location) end)
  assert(operational(function() source.inverse(implicit_authority) end) == explicit_error)
  assert(vim.uv.fs_unlink(cache_file))
  assert(vim.uv.fs_rename(cache_backup, cache_file))
  local changed_cache_text = cache_text:gsub('Old', 'Bad', 1)
  assert(changed_cache_text ~= cache_text and #changed_cache_text == #cache_text)
  write_bytes(cache_file, changed_cache_text)
  explicit_error = operational(function() source.inverse(authority_location) end,
    'cached source changed after publication')
  assert(operational(function() source.inverse(implicit_authority) end) == explicit_error)
  operational(function() source.forward(unknown_document, unknown_buffer, 0, 0) end,
    'cached source changed after publication')
  write_bytes(cache_file, cache_text)
  assert(source.inverse(implicit_authority).buffer == unknown_buffer)
  local shared_text, shared_manifest = cached_text(shared_document, unknown.main)
  local shared_file = vim.fs.dirname(shared_document.source_snapshot) .. '/' .. shared_manifest.files[unknown.main]
  write_bytes(shared_file, changed_cache_text)
  operational(function() source.inverse(implicit_location(shared_document, unknown.main, 1, 0)) end,
    'cached source changed after publication')
  assert(source.inverse(implicit_authority).buffer == unknown_buffer)
  write_bytes(shared_file, shared_text)
  local invalid_digests = {
    function(value) value.digests = nil end,
    function(value) value.digests = vim.NIL end,
    function(value) value.digests = {} end,
    function(value) value.digests = vim.empty_dict() end,
    function(value) value.digests[unknown.main] = nil end,
    function(value) value.digests[directory .. '/uncaptured-digest.tex'] = string.rep('0', 64) end,
    function(value) value.digests[unknown.main] = 0 end,
    function(value) value.digests[unknown.main] = string.rep('0', 63) end,
    function(value) value.digests[unknown.main] = string.rep('0', 65) end,
    function(value) value.digests[unknown.main] = string.rep('A', 64) end,
    function(value) value.digests[unknown.main] = string.rep('g', 64) end,
  }
  for index, invalidate in ipairs(invalid_digests) do
    local invalid = vim.deepcopy(cache_manifest)
    invalidate(invalid)
    local path = vim.fs.dirname(unknown_document.source_snapshot) .. '/invalid-digests-' .. index .. '.json'
    write_bytes(path, vim.json.encode(invalid))
    local query = location(unknown_document, unknown.main, 1, 0)
    query.source_snapshot = path
    operational(function() source.inverse(query) end)
  end
  assert(source.inverse(implicit_authority).buffer == unknown_buffer)
  local cache_directory_link = directory .. '/snapshot-directory-link'
  assert(vim.uv.fs_symlink(vim.fs.dirname(unknown_document.source_snapshot), cache_directory_link))
  local directory_alias_location = location(unknown_document, unknown.main, 1, 0)
  directory_alias_location.source_snapshot = cache_directory_link .. '/manifest.json'
  assert(source.inverse(directory_alias_location).buffer == unknown_buffer)
  local first_output = directory .. '/real-first-output'
  local linked_output = directory .. '/linked-first-output'
  vim.fn.mkdir(first_output)
  assert(vim.uv.fs_symlink(first_output, linked_output))
  local first = { main = directory .. '/first-build.tex', cwd = directory, pdf = linked_output .. '/deep/missing/first.pdf' }
  vim.fn.writefile({ 'First successful output' }, first.main)
  assert(not vim.uv.fs_stat(first.pdf))
  local first_capture = source.capture(first, { first.main })
  vim.fn.mkdir(vim.fs.dirname(first.pdf), 'p')
  pdf_fixture(first.pdf)
  local first_document = assert(source.publish(first_capture, info(first)))
  assert(first_document.pdf == first_output .. '/deep/missing/first.pdf')
  local fifo = directory .. '/source-fifo.tex'
  local mkfifo = vim.system({ 'mkfifo', fifo }, { text = true }):wait(2000)
  assert(mkfifo.code == 0, mkfifo.stderr)
  local fifo_data = { mode = 'fifo', directory = directory, project = safety, fifo = fifo,
    fifo_location = location(safety_document, safety.main, 1, 0) }
  fifo_data.fifo_location.source_snapshot = fifo
  local fifo_result = vim.system({ vim.v.progpath, '--headless', '-u', 'NONE', '-l', root .. '/tests/nvim_source.lua' }, {
    cwd = root, env = { PDFTERM_SOURCE_CACHE_REPLAY = vim.json.encode(fifo_data) }, text = true,
  }):wait(5000)
  assert(fifo_result.code == 0, (fifo_result.stderr or '') .. (fifo_result.stdout or ''))
  local replay_data = { directory = directory, project = safety, info = info(safety),
    live_byte = 12, expected_column = 13, old_location = location(safety_document, safety.main, 1, 6) }
  local replay_result = vim.system({ vim.v.progpath, '--headless', '-u', 'NONE', '-l', root .. '/tests/nvim_source.lua' }, {
    cwd = root, env = { PDFTERM_SOURCE_CACHE_REPLAY = vim.json.encode(replay_data) }, text = true,
  }):wait(10000)
  assert(replay_result.code == 0, (replay_result.stderr or '') .. (replay_result.stdout or ''))
  local cold_project = { main = directory .. '/cold-main.tex', pdf = directory .. '/cold-main.pdf', cwd = directory }
  local cold_file = directory .. '/cold-body.tex'
  vim.fn.writefile({ 'Main stable needle' }, cold_project.main)
  vim.fn.writefile({ 'Cold copper needle' }, cold_file)
  pdf_fixture(cold_project.pdf)
  local cold_document = finish(cold_project, source.capture(cold_project, { cold_project.main, cold_file }),
    { cold_project.main, cold_file })
  local cold_data = { directory = directory, project = cold_project,
    info = info(cold_project, { cold_project.main, cold_file }), cold_file = cold_file,
    cold_location = location(cold_document, cold_file, 1, 5) }
  for _, mode in ipairs({ 'cold-tamper', 'cold-cache' }) do
    cold_data.mode = mode
    local cold_result = vim.system({ vim.v.progpath, '--headless', '-u', 'NONE', '-l', root .. '/tests/nvim_source.lua' }, {
      cwd = root, env = { PDFTERM_SOURCE_CACHE_REPLAY = vim.json.encode(cold_data) }, text = true,
    }):wait(10000)
    assert(cold_result.code == 0, (cold_result.stderr or '') .. (cold_result.stdout or ''))
  end
  local legacy_manifest = vim.json.decode(manifest_bytes)
  legacy_manifest.warnings[unknown.main] = 'compiled source baseline is unverified; external build'
  write_bytes(unknown_document.source_snapshot, vim.json.encode(legacy_manifest))
  local legacy_info = info(unknown)
  legacy_info.revision = unknown_document.revision
  local legacy_result = vim.system({ vim.v.progpath, '--headless', '-u', 'NONE', '-l', root .. '/tests/nvim_source.lua' }, {
    cwd = root, text = true, env = { PDFTERM_SOURCE_CACHE_REPLAY = vim.json.encode({
      mode = 'legacy-snapshot', directory = directory, project = unknown, info = legacy_info,
    }) },
  }):wait(10000)
  write_bytes(unknown_document.source_snapshot, manifest_bytes)
  assert(legacy_result.code == 0, (legacy_result.stderr or '') .. (legacy_result.stdout or ''))
  print('source anchor regressions passed: inline/line shifts, insert/replace/delete, undo/redo, UTF-8, reload/unload/rename, PDF/revision identity, compiler races, immutable cache, unknown inputs')
end, debug.traceback)

api.nvim_create_autocmd('VimLeavePre', { once = true, callback = function() vim.fn.delete(directory, 'rf') end })
if not ok then
  io.stderr:write(failure .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
