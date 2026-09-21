-- Run: PDFTERM_EXECUTABLE="$PWD/target/debug/pdfterm" nvim --headless -u NONE -l tests/nvim.lua
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root .. '/nvim')
local directory = assert(vim.uv.fs_mkdtemp(root .. '/.nvim-test-XXXXXX'))
vim.fn.mkdir(directory .. '/pdfterm', 'p', 448)
vim.env.XDG_CONFIG_HOME = directory
vim.env.KITTY_WINDOW_ID, vim.env.TERM_PROGRAM = nil, nil
vim.env.SSH_CONNECTION = '127.0.0.1 50000 127.0.0.1 22'
local server
local function wait(predicate)
  assert(vim.wait(10000, predicate, 5), 'asynchronous operation did not finish')
end
local ok, failure = xpcall(function()
  local project = require('pdfterm.project')
  local terminal = require('pdfterm.terminal')
  terminal.capture_source = function()
    error('attachment must not require terminal capture')
  end
  local log = directory .. '/build.log'
  local completed = {}
  vim.fn.mkdir(directory .. '/other')
  local function build(tag, id)
    project.build(
      {
        cwd = tag == 'A' and directory or directory .. '/other',
        pdf = directory .. '/shared.pdf',
        build = { '/bin/sh', '-c', 'echo start:$1 >> "$2"; sleep .05; echo end:$1 >> "$2"', 'test', tag, log },
      },
      id,
      function(result)
        assert(result.code == 0, result.stderr)
        completed[#completed + 1] = tag
      end
    )
  end
  build('A', project.next())
  build('B', project.next())
  build('C', project.next())
  wait(function()
    return #completed == 1
  end)
  assert(completed[1] == 'C', 'obsolete build completion navigated')
  assert(
    table.concat(vim.fn.readfile(log), ',') == 'start:A,end:A,start:C,end:C',
    'builds overlapped or obsolete pending build ran'
  )
  local result
  project.run({ '/bin/sh', '-c', 'sleep 30 & wait' }, directory, 40, function(value)
    result = value
  end)
  wait(function()
    return result ~= nil
  end)
  assert(result.code ~= 0 and result.stderr:find('timed out'))
  result = nil
  project.run({ '/bin/sh', '-c', 'yes flood' }, directory, 1000, function(value)
    result = value
  end)
  wait(function()
    return result ~= nil
  end)
  assert(result.code ~= 0 and result.stderr:find('1 MiB'))

  vim.fn.writefile(
    { 'forward_socket="forward.sock"', '[editor]', 'transport="socket"', 'path="editor.sock"' },
    directory .. '/pdfterm/config.toml'
  )
  local binary = assert(vim.env.PDFTERM_EXECUTABLE, 'set PDFTERM_EXECUTABLE to a built viewer')
  local wrapper = directory .. '/selected-viewer'
  vim.fn.writefile({
    '#!/bin/sh',
    'echo selected >> ' .. vim.fn.shellescape(directory .. '/bootstrap.log'),
    'exec ' .. vim.fn.shellescape(binary) .. ' "$@"',
  }, wrapper)
  assert(vim.uv.fs_chmod(wrapper, 448))
  local source = directory .. '/navigation.tex'
  vim.fn.writefile(vim.fn.readfile(root .. '/tests/fixtures/navigation.tex'), source)
  vim.fn.mkdir(directory .. '/artifacts')
  local pdf = directory .. '/artifacts/navigation.pdf'
  local adapter = require('pdfterm')
  adapter.setup({
    executable = wrapper,
    session = 'adapter',
    attach_only = false,
    focus_on_inverse = true,
    compile = true,
    project = {
      main = 'navigation.tex',
      pdf = 'artifacts/navigation.pdf',
      cwd = directory,
      build = {
        '/bin/sh',
        '-c',
        'sleep .05; exec pdflatex -interaction=nonstopmode -halt-on-error -synctex=1 -output-directory=artifacts navigation.tex',
      },
    },
  })
  wait(function()
    return vim.fn.filereadable(directory .. '/bootstrap.log') == 1
  end)
  assert(vim.fn.readfile(directory .. '/bootstrap.log')[1] == 'selected', 'bootstrap executable ignored')
  assert(vim.fn.exists(':PdfTermForward') == 2 and vim.fn.exists(':PdfTermBuild') == 2)
  for _, mapping in ipairs(vim.api.nvim_get_keymap('n')) do
    assert(not (mapping.desc or ''):match('^pdfterm'), 'default mappings are not opt-in')
  end
  local config = vim.json.decode(
    vim.system({ binary, '--session', 'adapter', '--print-config' }, { text = true }):wait(10000).stdout
  )
  assert(config.forward_socket:match('/adapter%-forward.sock$'))
  assert(not vim.uv.fs_lstat(config.editor.path), 'setup opened an inverse listener before first use')
  local requests = {}
  server = assert(vim.uv.new_pipe(false))
  assert(server:bind(config.forward_socket))
  server:listen(16, function(error)
    assert(not error, error)
    local client = assert(vim.uv.new_pipe(false))
    server:accept(client)
    local chunks = {}
    client:read_start(function(read_error, chunk)
      assert(not read_error, read_error)
      if chunk then
        chunks[#chunks + 1] = chunk
      else
        requests[#requests + 1] = vim.json.decode(table.concat(chunks))
        client:write('{"ok":true,"error":null}', function()
          client:shutdown(function()
            client:close()
          end)
        end)
      end
    end)
  end)
  vim.cmd.edit(vim.fn.fnameescape(source))
  vim.api.nvim_win_set_cursor(0, { 4, 0 })
  adapter.forward()
  vim.api.nvim_win_set_cursor(0, { 6, 0 })
  adapter.forward()
  local ticks = 0
  local timer = assert(vim.uv.new_timer())
  timer:start(1, 5, function()
    ticks = ticks + 1
  end)
  wait(function()
    return #requests == 1
  end)
  timer:stop()
  timer:close()
  assert(ticks > 2, 'build/resolution blocked Neovim events')
  local resolved = vim
    .system({ binary, pdf, '--synctex-view', source, '--line', '6', '--column', '1' }, { text = true })
    :wait(10000)
  assert(resolved.code == 0, resolved.stderr)
  local expected = vim.json.decode(resolved.stdout)
  assert(requests[1].v == expected.v and requests[1].h == expected.h, 'latest forward intent lost')
  assert(#vim.fn.readfile(directory .. '/bootstrap.log') == 2, 'obsolete build reached resolution')
  -- Opening an existing PDF must not invoke TeX/SyncTeX, even with compile enabled.
  assert(vim.uv.fs_unlink(directory .. '/artifacts/navigation.synctex.gz'))
  local linked_pdf = directory .. '/linked PDF.PDF'
  assert(vim.uv.fs_symlink(pdf, linked_pdf))
  adapter.open(linked_pdf)
  wait(function()
    return #requests == 2
  end)
  assert(requests[2].pdf == vim.uv.fs_realpath(pdf) and requests[2].page == 1)
  assert(vim.deep_equal(requests[2].revision, expected.revision), 'PDF revision differs from native metadata')
  assert(not vim.uv.fs_stat(directory .. '/artifacts/navigation.synctex.gz'), 'opening PDF unexpectedly rebuilt TeX')
  local inverse = assert(vim.uv.new_pipe(false))
  inverse:connect(config.editor.path, function(error)
    assert(not error, error)
    inverse:write(vim.json.encode({ file = source, line = 4, byte_column = 6 }), function()
      inverse:shutdown(function()
        inverse:close()
      end)
    end)
  end)
  wait(function()
    return vim.api.nvim_win_get_cursor(0)[1] == 4
  end)
  assert(vim.api.nvim_win_get_cursor(0)[2] == 6)
  print(
    'adapter regressions passed: serialized/latest build, timeout/output cap, bootstrap, named session, attach-only, inverse socket; event ticks='
      .. ticks
  )
end, debug.traceback)
if server and not server:is_closing() then
  server:close()
end
-- VimLeavePre cleans the owned inverse socket and active process groups first.
vim.api.nvim_create_autocmd('VimLeavePre', {
  once = true,
  callback = function()
    vim.fn.delete(directory, 'rf')
  end,
})
if not ok then
  io.stderr:write(failure .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
