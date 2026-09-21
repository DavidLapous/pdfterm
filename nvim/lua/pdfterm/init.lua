-- Public editor API. Socket framing and project jobs have separate owners.
local M = {}
local socket = require('pdfterm.socket')
local project = require('pdfterm.project')
local terminal = require('pdfterm.terminal')
local root = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':h:h:h:h')
local options, main_file, initialized, exiting
local cancel_forward, cancel_resolution, close_listener
local owned_splits, launch_waiters = {}, nil
local launch_generation, launch_process

local function notify(message) vim.notify('pdfterm: ' .. tostring(message), vim.log.levels.ERROR) end
local function alive(id) return not exiting and project.current(id) end
local function intent()
  local id = project.next()
  if cancel_forward then cancel_forward(); cancel_forward = nil end
  if cancel_resolution then cancel_resolution(); cancel_resolution = nil end
  return id
end
local function command(arguments)
  local argv = { options.executable }
  if options.session then vim.list_extend(argv, { '--session', options.session }) end
  return vim.list_extend(argv, arguments)
end
local function inverse(location)
  if exiting then return end
  local buffer = vim.fn.bufnr(location.file)
  local window = buffer >= 0 and vim.fn.win_findbuf(buffer)[1] or nil
  if window then vim.api.nvim_set_current_win(window) else vim.cmd.edit(vim.fn.fnameescape(location.file)) end
  local line = math.max(1, math.min(location.line, vim.api.nvim_buf_line_count(0)))
  local text = vim.api.nvim_buf_get_lines(0, line - 1, line, false)[1] or ''
  vim.api.nvim_win_set_cursor(0, { line, math.min(location.byte_column, #text) })
  vim.cmd('normal! zvzz')
  if options.focus_on_inverse and M._source_terminal then
    terminal.focus(M._source_terminal, vim.schedule_wrap(function(result)
      if result.code ~= 0 then notify('could not focus source terminal: ' .. (result.stderr or '')) end
    end))
  end
end

local function launch(id, callback)
  if options.attach_only then callback('viewer unavailable (attach_only=true)'); return end
  launch_generation = id
  if launch_waiters then launch_waiters[#launch_waiters + 1] = callback; return end
  launch_waiters = { callback }
  local function complete(error)
    local waiters = launch_waiters or {}
    launch_waiters = nil
    for _, waiter in ipairs(waiters) do waiter(error) end
  end
  terminal.capture_source(function(error, source)
    if error then complete(error); return end
    if not alive(launch_generation) then complete('navigation superseded'); return end
    M._source_terminal = source
    local ok, process = pcall(terminal.launch_split, source, options.executable, M._launch_pdf, function(result, split)
      launch_process = nil
      if split then owned_splits[#owned_splits + 1] = split end
      vim.schedule(function()
        if exiting then
          complete('editor stopped')
        else complete(not split and ('terminal split failed: ' .. (result.stderr or 'missing ID')) or nil) end
      end)
    end, options.session)
    if ok then launch_process = process else complete(tostring(process)) end
  end)
end

local function deliver(pdf, payload, id, source)
  assert(initialized, 'pdfterm: call setup() first')
  if not alive(id) then return end
  if options.forward_socket == '' then notify('forward_socket is disabled'); return end
  if source then M._source_terminal = source end
  local launched, attempts = false, 0
  local attempt
  attempt = function()
    if not alive(id) then return end
    cancel_forward = socket.forward(options.forward_socket, payload, function(error, connection_error)
      if not alive(id) then return end
      cancel_forward = nil
      if not error then
        if options.focus_on_inverse and not M._source_terminal then
          terminal.capture_source(function(capture_error, captured)
            if not alive(id) then return end
            if capture_error then notify('navigation succeeded; focus unavailable: ' .. capture_error)
            else M._source_terminal = captured end
          end)
        end
        return
      end
      if not connection_error or not (error:match('ENOENT') or error:match('ECONNREFUSED')) then notify(error); return end
      if not launched then
        launched = true
        M._launch_pdf = pdf
        launch(id, function(launch_error)
          if not alive(id) then return end
          if launch_error then notify(launch_error) else attempt() end
        end)
      else
        attempts = attempts + 1
        if attempts >= 25 then notify('viewer did not open its forward socket')
        else vim.defer_fn(attempt, 200) end
      end
    end)
  end
  attempt()
end

function M.forward_search(pdf, payload, source)
  deliver(pdf, payload, intent(), source)
end
function M.set_main(file)
  file = file or vim.api.nvim_buf_get_name(0)
  assert(file:match('%.tex$'), 'pdfterm: main document must be a TeX file')
  main_file = vim.fn.fnamemodify(file, ':p')
  options.project = vim.tbl_extend('force', options.project or {}, { main = main_file })
  intent()
  vim.notify('Set current latex file to ' .. main_file)
end
function M.toggle_compile()
  options.compile = not options.compile
  vim.notify('Compile flag is now: ' .. tostring(options.compile))
end
local function describe()
  return project.describe(options.project, main_file or vim.api.nvim_buf_get_name(0))
end
function M.build()
  local id = intent()
  local p = describe()
  vim.cmd('write')
  project.build(p, id, function(result) if result.code ~= 0 then notify(result.stderr ~= '' and result.stderr or result.stdout) end end)
end
function M.forward()
  local id = intent() -- Before save, build, resolution, and socket delivery.
  local p = describe()
  vim.cmd('write')
  local cursor = vim.api.nvim_win_get_cursor(0)
  local file = vim.api.nvim_buf_get_name(0)
  local column = vim.fn.strchars(vim.api.nvim_get_current_line():sub(1, cursor[2])) + 1
  local function resolve()
    if not alive(id) then return end
    cancel_resolution = project.run(command({ p.pdf, '--synctex-view', file, '--line', tostring(cursor[1]), '--column', tostring(column) }), p.cwd, 11000, function(result)
      if not alive(id) then return end
      cancel_resolution = nil
      if result.code ~= 0 then notify(result.stderr); return end
      deliver(p.pdf, result.stdout, id)
    end)
  end
  if options.compile then
    project.build(p, id, function(result)
      if result.code ~= 0 then notify(result.stderr ~= '' and result.stderr or result.stdout) else resolve() end
    end)
  else resolve() end
end

function M.setup(opts)
  if initialized then return end
  opts = opts or {}
  local executable = opts.executable or root .. '/target/release/pdfterm'
  local argv = { executable, '--print-config' }
  if opts.session then vim.list_extend(argv, { '--session', opts.session }) end
  local result = vim.system(argv, { text = true, timeout = 10000 }):wait()
  assert(result.code == 0, 'pdfterm configuration: ' .. (result.stderr or 'viewer failed'))
  local config = vim.json.decode(result.stdout)
  options = vim.tbl_extend('force', config.nvim, opts, {
    executable = opts.executable or (config.nvim.executable ~= '' and config.nvim.executable or executable),
    editor = config.editor, forward_socket = config.forward_socket,
  })
  if options.editor.transport == 'socket' then close_listener = socket.listen(options.editor.path, inverse) end
  initialized, exiting = true, false
  local function map(key, callback, extra)
    if key and key ~= '' then vim.keymap.set('n', key, callback, extra) end
  end
  map(options.keys.forward, M.forward, { desc = 'pdfterm forward search' })
  map(options.keys.main_file, M.set_main, { desc = 'pdfterm set main TeX file' })
  map(options.keys.compile, M.toggle_compile, { desc = 'pdfterm toggle compilation' })
  local group = vim.api.nvim_create_augroup('pdfterm', { clear = true })
  local function build_map(buffer) map(options.keys.build, M.build, { buffer = buffer, desc = 'pdfterm build TeX' }) end
  vim.api.nvim_create_autocmd('FileType', { group = group, pattern = { 'tex', 'latex' }, callback = function(event) build_map(event.buf) end })
  if vim.bo.filetype == 'tex' or vim.bo.filetype == 'latex' then build_map(0) end
  vim.api.nvim_create_user_command('PdfTermForward', M.forward, {})
  vim.api.nvim_create_user_command('PdfTermBuild', M.build, {})
  vim.api.nvim_create_user_command('PdfTermMain', function(args) M.set_main(args.args ~= '' and args.args or nil) end, { nargs = '?', complete = 'file' })
  vim.api.nvim_create_user_command('PdfTermCompile', M.toggle_compile, {})
  vim.api.nvim_create_autocmd('VimLeavePre', { group = group, once = true, callback = function()
    exiting = true
    intent(); project.close()
    if close_listener then close_listener() end
    if launch_process then
      local ok, error = pcall(launch_process.wait, launch_process, 3500)
      if not ok then notify('waiting for terminal launch: ' .. tostring(error)) end
    end
    for _, split in ipairs(owned_splits) do
      local ok, error = pcall(terminal.close, split)
      if not ok then notify(error) end
    end
  end })
end
return M
