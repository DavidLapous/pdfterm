-- Run: nvim --headless -u NONE -l tests/nvim_typst_inverse.lua
-- Real Tinymist compiler spans, private EOF-framed inverse sockets, no editor effects.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local project = require('pdfterm.project')
local typst = require('pdfterm.typst')
local directories, endpoints = {}, {}
local function directory()
  local path = assert(vim.uv.fs_mkdtemp('/tmp/pdfterm-inverse-test-XXXXXX'))
  path = assert(vim.uv.fs_realpath(path))
  directories[#directories + 1] = path
  return path
end
local function build(p)
  local result = vim.system(p.build, { cwd = p.cwd, text = true }):wait()
  assert(result.code == 0, result.stderr)
end
local function forward(p, file, row, column)
  local result, calls = nil, 0
  local cancel = typst.resolve(p, file, row, column, function(value)
    result, calls = value, calls + 1
  end)
  assert(vim.wait(60000, function() return result ~= nil end, 10), 'forward source map timed out')
  assert(result.code == 0, result.stderr)
  local mapped = vim.json.decode(result.stdout)
  assert(type(mapped.inverse_search) == 'string' and mapped.inverse_search:sub(1, 1) == '/')
  endpoints[#endpoints + 1] = mapped.inverse_search
  local socket = assert(vim.uv.fs_lstat(mapped.inverse_search))
  local parent = assert(vim.uv.fs_lstat(vim.fs.dirname(mapped.inverse_search)))
  assert(socket.type == 'socket' and socket.uid == vim.uv.getuid() and bit.band(socket.mode, 511) == 384)
  assert(parent.type == 'directory' and parent.uid == vim.uv.getuid() and bit.band(parent.mode, 511) == 448)
  -- Forward cancellation is finished: it must not kill a published inverse map.
  cancel()
  return mapped, function() assert(calls == 1, 'forward callback repeated after publication') end
end
local function query(mapped, override, raw)
  local pipe = assert(vim.uv.new_pipe(false))
  local data, finished, failure = '', false, nil
  pipe:connect(mapped.inverse_search, function(err)
    if err then failure, finished = err, true; pipe:close(); return end
    pipe:read_start(function(read_error, chunk)
      if read_error then failure = read_error end
      if not chunk or read_error then
        finished = true
        pipe:read_stop(); pipe:close()
      else
        data = data .. chunk
        assert(#data <= 4096, 'oversized inverse response')
      end
    end)
    local request = vim.tbl_extend('force', {
      revision = mapped.revision, page = mapped.page, x = mapped.h + 0.5, y = mapped.v - 0.5,
    }, override or {})
    pipe:write(raw or vim.json.encode(request), function(write_error)
      assert(not write_error, write_error)
      pipe:shutdown(function(shutdown_error) assert(not shutdown_error, shutdown_error) end)
    end)
  end)
  assert(vim.wait(12000, function() return finished end, 10), 'inverse source map timed out')
  assert(not failure, failure)
  return vim.json.decode(data)
end
local function precise(mapped, file, row, source, anchor)
  local response = query(mapped)
  assert(response.ok, vim.inspect(response))
  local loc = response.location
  assert(loc.precise and loc.file == file and loc.line == row, vim.inspect(response))
  local start = assert(source:find(anchor, 1, true)) - 1
  assert(loc.byte_column >= start and loc.byte_column < start + #anchor, vim.inspect(response))
  local following = source:byte(loc.byte_column + 1)
  assert(not following or following < 128 or following >= 192, 'inverse column splits UTF-8')
  local prefix = source:sub(1, loc.byte_column)
  local scalars = vim.fn.strchars(prefix)
  local _, astral = prefix:gsub('[\240-\244]', '')
  assert(loc.column_char == scalars + 1 and loc.column == scalars + astral + 1, vim.inspect(response))
end
local ok, failure = xpcall(function()
  local root = directory()
  local main_line = 'Plain café λ 𝔸 *anchor* on page two.'
  local included_line = 'λ café 𝔸 *includedanchor* on page three.'
  vim.fn.writefile({
    '#set page(width: 360pt, height: 400pt, margin: 30pt)',
    'First page.', '#pagebreak()', main_line, '#pagebreak()', '#include "chapter.typ"',
  }, root .. '/main.typ')
  -- CRLF is part of the saved source; returned columns never include its CR.
  vim.fn.writefile({ '= Included page\r', included_line .. '\r' }, root .. '/chapter.typ')
  local p = project.describe({ main = 'main.typ', cwd = root })
  build(p)
  local main, main_once = forward(p, p.main, 4, #'Plain café λ 𝔸 *')
  assert(main.page == 2, vim.inspect(main))
  precise(main, p.main, 4, main_line, 'anchor')
  local stale = vim.deepcopy(main.revision)
  stale.length = stale.length + 1
  local rejected = query(main, { revision = stale })
  assert(not rejected.ok and rejected.error:find('revision', 1, true), vim.inspect(rejected))
  rejected = query(main, { page = 0 })
  assert(not rejected.ok and rejected.error:find('point', 1, true), vim.inspect(rejected))
  rejected = query(main, nil, string.rep('x', 4097))
  assert(not rejected.ok and rejected.error:find('4096', 1, true), vim.inspect(rejected))
  precise(main, p.main, 4, main_line, 'anchor')
  local included, included_once = forward(p, root .. '/chapter.typ', 2, #'λ café 𝔸 *')
  assert(included.page == 3, vim.inspect(included))
  assert(not vim.uv.fs_lstat(main.inverse_search), 'same-PDF refresh retained old endpoint')
  precise(included, root .. '/chapter.typ', 2, included_line, 'includedanchor')
  main_once()

  local other_root = directory()
  local independent_line = 'Independent source *anchor*.'
  vim.fn.writefile({ independent_line }, other_root .. '/main.typ')
  local other = project.describe({ main = 'main.typ', cwd = other_root })
  build(other)
  local independent, independent_once = forward(other, other.main, 1, #'Independent source *')
  precise(independent, other.main, 1, independent_line, 'anchor')
  precise(included, root .. '/chapter.typ', 2, included_line, 'includedanchor')

  vim.fn.writefile({ '= Included page\r', included_line .. ' Changed.\r' }, root .. '/chapter.typ')
  rejected = query(included)
  assert(not rejected.ok and rejected.error:find('inputs changed', 1, true), vim.inspect(rejected))
  -- The other PDF map survives a changed source in this project.
  precise(independent, other.main, 1, independent_line, 'anchor')
  local fd = assert(vim.uv.fs_open(other.pdf, 'a', 384))
  assert(vim.uv.fs_write(fd, '\n', -1))
  assert(vim.uv.fs_close(fd))
  rejected = query(independent)
  assert(not rejected.ok and rejected.error:find('PDF changed', 1, true), vim.inspect(rejected))
  included_once(); independent_once()
  local missing, missing_once = forward(other, other.main, 1, #'Independent source *')
  rejected = query(missing, { page = 4294967295 })
  assert(not rejected.ok and rejected.error:find('no rendered source', 1, true), vim.inspect(rejected))
  assert(vim.wait(5000, function() return not vim.uv.fs_lstat(missing.inverse_search) end, 10),
    'ambiguous inverse timeout left a reusable source-map endpoint')
  missing_once()
  local refreshed, refreshed_once = forward(other, other.main, 1, #'Independent source *')
  precise(refreshed, other.main, 1, independent_line, 'anchor')
  refreshed_once()
end, debug.traceback)
vim.api.nvim_exec_autocmds('VimLeavePre', {})
local cleaned = vim.wait(5000, function()
  for _, endpoint in ipairs(endpoints) do
    if vim.uv.fs_lstat(vim.fs.dirname(endpoint)) then return false end
  end
  return true
end, 10)
for _, path in ipairs(directories) do vim.fn.delete(path, 'rf') end
assert(ok, failure)
assert(cleaned, 'retained Typst service did not clean up on Neovim exit')
print('Typst inverse source-map tests passed')
