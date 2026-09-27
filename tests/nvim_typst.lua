-- Run: nvim --headless -u NONE -l tests/nvim_typst.lua
-- Requires typst and tinymist on PATH; uses real compiler source positions.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local project = require('pdfterm.project')
local typst = require('pdfterm.typst')
local directory = vim.fn.tempname() .. ' typst navigation'
vim.fn.mkdir(directory, 'p')
directory = assert(vim.uv.fs_realpath(directory))
local alias = directory .. ' alias'
local original_root = vim.env.TYPST_ROOT
vim.env.TYPST_ROOT = nil
local start_rpc = vim.lsp.rpc.start
local function resolve(p, file, line, column)
  local result
  local cancel = typst.resolve(p, file, line, column, function(value)
    result = value
  end)
  if not vim.wait(60000, function() return result ~= nil end, 10) then
    cancel()
    error('Typst source resolution timed out')
  end
  assert(result.code == 0, result.stderr)
  return vim.json.decode(result.stdout)
end
local ok, failure = xpcall(function()
  vim.fn.writefile({
    '#set page(width: 300pt, height: 400pt, margin: 30pt)',
    '= First page',
    'First page anchor.',
    '#pagebreak()',
    '= Second page',
    'Unicode café λ anchor on page two.',
    '#pagebreak()',
    '#include "chapter.typ"',
  }, directory .. '/main.typ')
  vim.fn.writefile({ '= Third page', 'Included source anchor on page three.' }, directory .. '/chapter.typ')
  local p = project.describe({ main = 'main.typ', cwd = directory, pdf = 'custom output.pdf' })
  local built
  project.build(p, project.next(), function(result) built = result end)
  assert(vim.wait(30000, function() return built ~= nil end, 10), 'Typst build timed out')
  assert(built.code == 0, built.stderr)
  local second = resolve(p, p.main, 6, #'Unicode café λ ')
  assert(second.page == 2, vim.inspect(second))
  assert(second.pdf == p.pdf and second.h >= 30 and second.v >= 30, vim.inspect(second))
  local stat = assert(vim.uv.fs_stat(p.pdf))
  assert(second.revision.length == stat.size and second.revision.inode == stat.ino)
  local third = resolve(p, directory .. '/chapter.typ', 2, 9)
  assert(third.page == 3, vim.inspect(third))
  assert(third.pdf == p.pdf and third.v >= 30, vim.inspect(third))

  assert(vim.uv.fs_symlink(directory, alias))
  local linked = project.describe({
    main = alias .. '/main.typ', cwd = alias, pdf = alias .. '/custom output.pdf',
    build = { 'typst', 'compile', '--root', alias, directory .. '/main.typ', p.pdf },
  })
  local linked_third = resolve(linked, directory .. '/chapter.typ', 2, 9)
  assert(linked_third.page == 3 and linked_third.pdf == linked.pdf, vim.inspect(linked_third))
  local linked_second = resolve(linked, alias .. '/main.typ', 6, #'Unicode café λ ')
  assert(linked_second.page == 2, vim.inspect(linked_second))

  -- A workspace directory is not Typst's implicit root: absolute imports
  -- default to the entry file's directory, even when the process cwd differs.
  vim.fn.mkdir(directory .. '/sources')
  vim.fn.writefile({ 'First page', '#include "/shared.typ"', 'Nested root anchor.' },
    directory .. '/sources/main.typ')
  vim.fn.writefile({ '#pagebreak()' }, directory .. '/sources/shared.typ')
  vim.fn.writefile({ 'Wrong workspace-root dependency.' }, directory .. '/shared.typ')
  local nested = project.describe({ main = 'sources/main.typ', cwd = directory })
  local nested_build = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
  assert(nested_build.code == 0, nested_build.stderr)
  local nested_position = resolve(nested, nested.main, 3, 7)
  assert(nested_position.page == 2, vim.inspect(nested_position))
  vim.env.TYPST_ROOT = directory
  nested_build = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
  assert(nested_build.code == 0, nested_build.stderr)
  local env_position = resolve(nested, nested.main, 3, 7)
  assert(env_position.page == 1, vim.inspect(env_position))
  nested.build = { 'typst', 'compile', '--root', directory .. '/sources', nested.main, nested.pdf }
  nested_build = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
  assert(nested_build.code == 0, nested_build.stderr)
  local explicit_position = resolve(nested, nested.main, 3, 7)
  assert(explicit_position.page == 2, vim.inspect(explicit_position))

  -- Save an included source after the real PDF export, before the real jump.
  -- The resolver must keep the original PDF instead of publishing mixed state.
  local unchanged = assert(vim.uv.fs_stat(nested.pdf))
  vim.lsp.rpc.start = function(...)
    local rpc = start_rpc(...)
    local request = rpc.request
    rpc.request = function(method, params, ...)
      if method == 'workspace/executeCommand' and params.command == 'tinymist.scrollPreview' then
        vim.fn.writefile({ '#pagebreak()', '#pagebreak()' }, directory .. '/sources/shared.typ')
      end
      return request(method, params, ...)
    end
    return rpc
  end
  local interrupted
  local cancel = typst.resolve(nested, nested.main, 3, 7, function(value) interrupted = value end)
  if not vim.wait(30000, function() return interrupted ~= nil end, 10) then
    cancel()
    error('Source-change navigation did not finish')
  end
  assert(interrupted.code ~= 0 and interrupted.stderr:find('changed', 1, true), vim.inspect(interrupted))
  local retained = assert(vim.uv.fs_stat(nested.pdf))
  assert(retained.ino == unchanged.ino and vim.deep_equal(retained.mtime, unchanged.mtime))
end, debug.traceback)
vim.lsp.rpc.start = start_rpc
vim.env.TYPST_ROOT = original_root
project.close()
vim.uv.fs_unlink(alias)
vim.fn.delete(directory, 'rf')
assert(ok, failure)
print('Typst source-position tests passed')
