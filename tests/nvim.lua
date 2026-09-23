-- Run: PDFTERM_EXECUTABLE="$PWD/target/debug/pdfterm" nvim --headless -u NONE -l tests/nvim.lua
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)
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
        build = {
          '/bin/sh',
          '-c',
          'echo start:$1 >> "$2"; sleep .05; echo end:$1 >> "$2"',
          'test',
          tag,
          log,
        },
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
  -- Compiler output must arrive before exit; late refreshes must not erase completion.
  local original_notify, notices = vim.notify, {}
  vim.notify = function(message, level, options)
    assert(not vim.in_fast_event(), 'notification emitted from process callback')
    notices[#notices + 1] = { message = message, level = level, id = options.id }
  end
  for _, exit_code in ipairs({ 0, 2 }) do
    notices = {}
    local finished = false
    local release = directory .. '/release-' .. exit_code
    project.build(
      {
        cwd = directory,
        pdf = directory .. '/progress.pdf',
        build = {
          '/bin/sh',
          '-c',
          'printf "discarded\\nline2\\nline3\\nline4\\nline5\\npar"; sleep .05; printf "tial\\n"; while ! test -f "$2"; do sleep .01; done; printf "diagnostic\\n" >&2; exit "$1"',
          'test',
          tostring(exit_code),
          release,
        },
      },
      project.next(),
      function(result)
        assert(result.code == exit_code)
        finished = true
      end
    )
    wait(function()
      return #notices > 1 and notices[#notices].message:find('partial', 1, true)
    end)
    assert(not finished, 'compiler output was buffered until exit')
    local progress = notices[#notices].message
    assert(progress:find('partial', 1, true) and not progress:find('discarded', 1, true))
    assert(#vim.split(progress, '\n') == 6, 'progress did not retain five log lines')
    vim.fn.writefile({}, release)
    wait(function()
      return finished
    end)
    local final = notices[#notices]
    assert(
      final.message:match('^[^\n]+')
        == (exit_code == 0 and 'Compilation OK' or 'Compilation failed')
    )
    assert(
      final.message:find('diagnostic', 1, true),
      'stderr omitted from compilation notification'
    )
    assert(final.level == (exit_code == 0 and vim.log.levels.INFO or vim.log.levels.ERROR))
    local count = #notices
    vim.wait(150, function()
      return false
    end, 10)
    assert(#notices == count, 'delayed progress overwrote compilation result')
    for _, notice in ipairs(notices) do
      assert(notice.id == final.id, 'compilation updates created separate notifications')
    end
  end
  vim.notify = original_notify
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
    'if test -f ' .. vim.fn.shellescape(directory .. '/hold-resolution') .. '; then',
    '  touch ' .. vim.fn.shellescape(directory .. '/resolution-started'),
    '  while test -f '
      .. vim.fn.shellescape(directory .. '/hold-resolution')
      .. '; do sleep .01; done',
    'fi',
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
        'if test -f fail-build; then echo "deliberate build failure" >&2; exit 1; fi; '
          .. 'sleep .05; exec pdflatex -interaction=nonstopmode -halt-on-error -synctex=1 -output-directory=artifacts navigation.tex',
      },
    },
  })
  wait(function()
    return vim.fn.filereadable(directory .. '/bootstrap.log') == 1
  end)
  assert(
    vim.fn.readfile(directory .. '/bootstrap.log')[1] == 'selected',
    'bootstrap executable ignored'
  )
  assert(
    vim.fn.exists(':PdfTermForward') == 2
      and vim.fn.exists(':PdfTermForwardSplit') == 2
  )
  assert(vim.fn.exists(':PdfTermBuild') == 2)
  for _, mapping in ipairs(vim.api.nvim_get_keymap('n')) do
    assert(not (mapping.desc or ''):match('^pdfterm'), 'default mappings are not opt-in')
  end
  local config = vim.json.decode(
    vim
      .system({ binary, '--session', 'adapter', '--print-config' }, { text = true })
      :wait(10000).stdout
  )
  assert(config.forward_socket:match('/adapter%-forward.sock$'))
  assert(
    not vim.uv.fs_lstat(config.editor.path),
    'setup opened an inverse listener before first use'
  )
  local requests = {}
  local function receive()
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
  end
  receive()
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
  assert(
    vim.deep_equal(requests[2].revision, expected.revision),
    'PDF revision differs from native metadata'
  )
  assert(
    not vim.uv.fs_stat(directory .. '/artifacts/navigation.synctex.gz'),
    'opening PDF unexpectedly rebuilt TeX'
  )
  -- A missing SyncTeX sidecar must not prevent opening the existing PDF.
  adapter.toggle_compile()
  local navigation_notices = {}
  vim.notify = function(message, level)
    assert(not vim.in_fast_event(), 'navigation warning emitted from process callback')
    navigation_notices[#navigation_notices + 1] = { message = message, level = level }
  end
  adapter.forward()
  wait(function()
    return #requests == 3
  end)
  assert(requests[3].pdf == vim.uv.fs_realpath(pdf) and requests[3].page == 1)
  assert(#navigation_notices == 1 and navigation_notices[1].level == vim.log.levels.WARN)
  -- Superseding an in-flight resolution must suppress its warning and fallback.
  navigation_notices = {}
  vim.fn.writefile({}, directory .. '/hold-resolution')
  adapter.forward()
  wait(function()
    return vim.fn.filereadable(directory .. '/resolution-started') == 1
  end)
  adapter.open(linked_pdf)
  wait(function()
    return #requests == 4
  end)
  vim.wait(150, function()
    return false
  end, 10)
  assert(
    #requests == 4 and #navigation_notices == 0,
    'superseded resolution still opened or warned'
  )
  assert(vim.uv.fs_unlink(directory .. '/hold-resolution'))

  -- Build failure is not a SyncTeX failure: do not open an old PDF.
  adapter.toggle_compile()
  navigation_notices = {}
  vim.fn.writefile({}, directory .. '/fail-build')
  adapter.forward()
  wait(function()
    for _, notice in ipairs(navigation_notices) do
      if notice.level == vim.log.levels.ERROR then
        return true
      end
    end
    return false
  end)
  vim.wait(150, function()
    return false
  end, 10)
  assert(#requests == 4, 'failed build opened stale output')
  vim.notify = original_notify
  local function inverse_jump()
    local inverse = assert(vim.uv.new_pipe(false))
    inverse:connect(config.editor.path, function(error)
      assert(not error, error)
      inverse:write(vim.json.encode({ file = source, line = 4, byte_column = 6 }), function()
        inverse:shutdown(function()
          inverse:close()
        end)
      end)
    end)
  end
  inverse_jump()
  wait(function()
    return vim.api.nvim_win_get_cursor(0)[1] == 4
  end)
  assert(vim.api.nvim_win_get_cursor(0)[2] == 6)
  -- The terminal selected before a slow resolver owns launch/inverse focus.
  vim.env.SSH_CONNECTION = nil
  local foreground, focused = 'A', nil
  terminal.capture_source = function(callback)
    local captured = foreground
    vim.defer_fn(function()
      callback(nil, { kind = 'ghostty', id = captured })
    end, 10)
  end
  terminal.focus = function(handle, callback)
    focused = handle.id
    callback({ code = 0 })
  end
  adapter.toggle_compile()
  vim.uv.fs_unlink(directory .. '/resolution-started')
  vim.fn.writefile({}, directory .. '/hold-resolution')
  adapter.forward()
  wait(function()
    return vim.fn.filereadable(directory .. '/resolution-started') == 1
  end)
  foreground = 'B'
  vim.uv.fs_unlink(directory .. '/hold-resolution')
  wait(function()
    return #requests == 5
  end)
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'A', 'slow navigation retargeted inverse focus to a later terminal')

  -- Explicit source handles win; socket attachment still works without capture.
  terminal.capture_source = function()
    error('supplied source must not be recaptured')
  end
  adapter.forward_search(pdf, vim.json.encode(requests[1]), { kind = 'ghostty', id = 'explicit' })
  wait(function()
    return #requests == 6
  end)
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'explicit')
  terminal.capture_source = function(callback)
    callback('terminal discovery unavailable')
  end
  adapter.open(pdf)
  wait(function()
    return #requests == 7
  end)

  -- A late capture cannot resurrect a superseded navigation.
  local captures = {}
  terminal.capture_source = function(callback)
    captures[#captures + 1] = callback
  end
  adapter.open(pdf)
  adapter.open(pdf)
  captures[2](nil, { kind = 'ghostty', id = 'new' })
  captures[1](nil, { kind = 'ghostty', id = 'old' })
  wait(function()
    return #requests == 8
  end)
  vim.wait(50, function()
    return false
  end, 5)
  assert(#requests == 8, 'superseded terminal capture delivered navigation')
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'new', 'superseded capture changed inverse focus')
  -- Ordinary forward only attaches; explicit split launches and then delivers.
  server:close()
  local launches, notices = 0, {}
  terminal.launch_split = function(_, _, _, callback)
    launches = launches + 1
    receive()
    vim.schedule(function()
      callback({ code = 0 }, { kind = 'ghostty', id = 'viewer' })
    end)
    return { wait = function() end }
  end
  terminal.capture_source = function(callback)
    callback(nil, { kind = 'ghostty', id = 'source' })
  end
  terminal.close = function(split)
    assert(split.id == 'viewer')
  end
  vim.notify = function(message, level)
    notices[#notices + 1] = { message = message, level = level }
  end
  vim.cmd('PdfTermForward')
  wait(function()
    return #notices >= 2
  end)
  assert(launches == 0 and #requests == 8, 'ordinary forward launched a viewer')
  assert(notices[#notices].message:find('PdfTermForwardSplit', 1, true))
  vim.cmd('PdfTermForwardSplit')
  wait(function()
    return #requests == 9
  end)
  assert(
    launches == 1 and requests[9].page == 1,
    'explicit split did not launch and forward'
  )
  print(
    'adapter regressions passed: serialized/latest build, timeout/output cap, bootstrap, named session, forward-only, explicit split, inverse socket; event ticks='
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
