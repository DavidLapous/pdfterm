-- Run: PDFTERM_EXECUTABLE="$PWD/target/release/pdfterm" nvim --headless -u NONE -l tests/nvim_log.lua
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)

local directory = assert(vim.uv.fs_mkdtemp(root .. '/.nvim-log-test-XXXXXX'))
local binary = vim.env.PDFTERM_EXECUTABLE
  or (vim.fn.executable(root .. '/target/debug/pdfterm') == 1 and (root .. '/target/debug/pdfterm'))
  or (vim.fn.executable(root .. '/target/release/pdfterm') == 1 and (root .. '/target/release/pdfterm'))
  or (vim.fn.exepath('pdfterm') ~= '' and vim.fn.exepath('pdfterm'))
  or (root .. '/target/release/pdfterm')
local log_file = directory .. '/logs/pdfterm.log'

local ok, failure = xpcall(function()
  local pdfterm = require('pdfterm')

  -- 1. Test log_entry directly with automatic parent directory creation
  assert(pdfterm.log_path() == nil, 'log_path before setup must be nil')
  pdfterm.log_entry('INFO', 'should not write because no path configured')
  assert(vim.fn.filereadable(log_file) == 0, 'file should not be created if unconfigured')

  -- 2. Setup with ~/log_file
  vim.env.HOME = directory
  pdfterm.setup({
    executable = binary,
    log_file = '~/logs/pdfterm.log',
    session = 'test-log-session',
  })

  assert(pdfterm.log_path() == log_file, 'log_path must expand ~/ to configured path')

  -- 3. Test log_entry writes formatted lines
  pdfterm.log_entry('INFO', 'hello world', 'test_ctx')
  assert(vim.fn.filereadable(log_file) == 1, 'log file must exist after log_entry')
  local lines = vim.fn.readfile(log_file)
  assert(#lines == 1, 'expected 1 log line, got: ' .. #lines)
  assert(lines[1]:match('%[%d%d%d%d%-%d%d%-%d%d %d%d:%d%d:%d%d%] %[INFO%] %[test_ctx%] hello world'),
    'unexpected line format: ' .. lines[1])

  -- 4. Test log entry without context
  pdfterm.log_entry('WARN', 'no context message')
  lines = vim.fn.readfile(log_file)
  assert(#lines == 2, 'expected 2 log lines')
  assert(lines[2]:match('%[%d%d%d%d%-%d%d%-%d%d %d%d:%d%d:%d%d%] %[WARN%] no context message'),
    'unexpected line format: ' .. lines[2])

  local notify, failures = vim.notify, {}
  vim.notify = function(message, level)
    failures[#failures + 1] = { message = message, level = level }
  end
  local saved_lines = vim.fn.readfile(log_file)
  assert(vim.fn.delete(log_file) == 0)
  assert(vim.fn.mkdir(log_file) == 1)
  pdfterm.log_entry('ERROR', 'must report an unopenable log')
  assert(#failures == 1 and failures[1].level == vim.log.levels.ERROR,
    'unopenable log must report one error without recursive logging')
  assert(failures[1].message:find(log_file, 1, true), 'log error must identify its path')
  assert(vim.fn.isdirectory(log_file) == 1, 'logging must not replace a directory')
  assert(vim.fn.delete(vim.fs.dirname(log_file), 'rf') == 0)
  vim.fn.writefile({ 'parent is a regular file' }, vim.fs.dirname(log_file))
  pdfterm.log_entry('ERROR', 'must report a failed parent directory')
  assert(#failures == 2 and failures[2].level == vim.log.levels.ERROR,
    'failed log directory creation must report one error')
  assert(vim.fn.readfile(vim.fs.dirname(log_file))[1] == 'parent is a regular file',
    'logging must not overwrite an obstructing file')
  assert(vim.fn.delete(vim.fs.dirname(log_file)) == 0)
  assert(vim.fn.mkdir(vim.fs.dirname(log_file)) == 1)
  vim.fn.writefile(saved_lines, log_file)
  vim.notify = notify

  -- 5. Test :PdfTermLog command opens a split buffer with the log file
  local initial_windows = vim.api.nvim_tabpage_list_wins(0)
  vim.cmd('PdfTermLog')
  assert(vim.wait(5000, function()
    return #vim.api.nvim_tabpage_list_wins(0) == #initial_windows + 1
  end, 10), 'PdfTermLog must open a split window')
  local current_buf = vim.api.nvim_get_current_buf()
  local current_name = vim.api.nvim_buf_get_name(current_buf)
  assert(current_name == log_file, 'split buffer must be the log file, got: ' .. current_name)
  vim.cmd('close')

  -- 6. Test inverse search logging
  -- Create a 5-line test buffer
  local test_file = directory .. '/test.tex'
  vim.fn.writefile({ 'line 1', 'line 2', 'line 3', 'line 4', 'line 5' }, test_file)
  local buf = vim.fn.bufadd(test_file)
  vim.fn.bufload(buf)
  vim.api.nvim_set_current_buf(buf)

  -- Case A: precise match on line 3
  local prev_count = #vim.fn.readfile(log_file)
  pdfterm._inverse({
    file = test_file,
    line = 3,
    byte_column = 2,
    precise = true,
  })
  lines = vim.fn.readfile(log_file)
  assert(#lines == prev_count + 1, 'expected 1 new log line for precise inverse')
  assert(lines[#lines]:match('%[INFO%] %[inverse: .*test%.tex:3:2 %(precise=true%)] position matched'),
    'unexpected line: ' .. lines[#lines])

  -- Case B: imprecise match (precise = false)
  prev_count = #lines
  pdfterm._inverse({
    file = test_file,
    line = 2,
    byte_column = 0,
    precise = false,
  })
  lines = vim.fn.readfile(log_file)
  assert(#lines == prev_count + 1, 'expected 1 new log line for imprecise inverse')
  assert(lines[#lines]:match('%[INFO%] %[inverse: .*test%.tex:2:0 %(precise=false%)] imprecise/coarse line positioning'),
    'unexpected line: ' .. lines[#lines])

  -- Case B2: routine best-guess diagnostic logs as INFO without notifying WARN
  prev_count = #lines
  pdfterm._inverse({
    file = test_file,
    line = 2,
    byte_column = 0,
    precise = false,
    warning = 'best-guess navigation: no literal source position was qualified; using the compiler line anchor',
  })
  lines = vim.fn.readfile(log_file)
  assert(#lines == prev_count + 1, 'expected 1 new log line for routine diagnostic inverse')
  assert(lines[#lines]:match('%[INFO%] %[inverse: .*test%.tex:2:0 %(precise=false%)] best%-guess navigation: no literal source position was qualified; using the compiler line anchor'),
    'unexpected line: ' .. lines[#lines])

  -- Case C: out of bounds line (line 50 in a 5-line buffer)
  prev_count = #lines
  pdfterm._inverse({
    file = test_file,
    line = 50,
    byte_column = 0,
    precise = true,
  })
  lines = vim.fn.readfile(log_file)
  assert(#lines == prev_count + 1, 'expected 1 new log line for out-of-bounds line')
  assert(lines[#lines]:match('%[WARN%] %[inverse: .*test%.tex:5:0 %(precise=true%)] inverse target line 50 is outside buffer bounds %(1%.%.5%)'),
    'unexpected line: ' .. lines[#lines])

  -- Case D: location with compiler warning
  prev_count = #lines
  pdfterm._inverse({
    file = test_file,
    line = 1,
    byte_column = 0,
    precise = true,
    warning = 'compiler line ambiguity detected',
  })
  lines = vim.fn.readfile(log_file)
  assert(#lines == prev_count + 1, 'expected 1 new log line for warned inverse')
  assert(lines[#lines]:match('%[WARN%] %[inverse: .*test%.tex:1:0 %(precise=true%)] compiler line ambiguity detected'),
    'unexpected line: ' .. lines[#lines])

  -- 7. Test forward search on non-tex buffer logs ERROR
  prev_count = #lines
  vim.cmd('new')
  vim.bo.buftype = 'nofile'
  pdfterm.forward()
  vim.cmd('close')
  lines = vim.fn.readfile(log_file)
  assert(#lines == prev_count + 1, 'expected 1 new log line for non-tex forward search')
  assert(lines[#lines]:match('%[ERROR%] .*select a main TeX or Typst file with :PdfTermMain or project%.main'),
    'unexpected line: ' .. lines[#lines])

  print('log regressions passed: path resolution, entry formatting, user command, precise/imprecise/bounds/warning inverse checks, forward buffer checks')
end, debug.traceback)

-- Cleanup
vim.fn.delete(directory, 'rf')

if not ok then
  io.stderr:write(failure .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
