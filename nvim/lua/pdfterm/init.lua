-- Neovim adapter for pdfterm's editor-neutral JSON socket protocol.
-- Rust owns SyncTeX resolution; this module owns splits, cursor placement, and focus.
local M = {}
local terminal = require 'pdfterm.terminal'
local platform = require 'pdfterm.platform'

local root = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':h:h:h:h')
local options = {
  executable = root .. '/target/release/pdfterm',
}

local owned_splits, split_launch, exiting = {}, nil, false
local function close_owned_splits()
  exiting = true
  -- The launch callback records ownership before scheduling any editor work.
  if split_launch then
    local result = split_launch:wait()
    if result.code ~= 0 then
      vim.notify('pdfterm: split launch failed during exit: ' .. (result.stderr or ''), vim.log.levels.ERROR)
    end
  end
  for _, split in ipairs(owned_splits) do
    local ok, message = pcall(terminal.close, split)
    if not ok then
      vim.notify(message, vim.log.levels.ERROR)
    end
  end
end

local function inverse_search(file, line, column)
  local buffer = vim.fn.bufnr(file)
  local window = buffer >= 0 and vim.fn.win_findbuf(buffer)[1] or nil
  if window then
    vim.api.nvim_set_current_win(window)
  else
    vim.cmd.edit(vim.fn.fnameescape(file))
  end
  line = math.max(1, math.min(line, vim.api.nvim_buf_line_count(0)))
  local text = vim.api.nvim_buf_get_lines(0, line - 1, line, false)[1] or ''
  vim.api.nvim_win_set_cursor(0, { line, math.min(column, #text) })
  vim.cmd 'normal! zvzz'
  if options.focus_on_inverse and M._source_terminal then
    terminal.focus(
      M._source_terminal,
      vim.schedule_wrap(function(result)
        if result.code ~= 0 then
          vim.notify('pdfterm: could not focus source terminal: ' .. (result.stderr or ''), vim.log.levels.ERROR)
        end
      end)
    )
  end
end

local setup_keys

local function handle_line(chunk)
  local ok, location = pcall(vim.json.decode, chunk)
  vim.schedule(function()
    if
      not ok
      or type(location) ~= 'table'
      or type(location.file) ~= 'string'
      or location.file:sub(1, 1) ~= '/'
      or location.file:find '%z'
      or type(location.line) ~= 'number'
      or location.line < 1
      or location.line % 1 ~= 0
      or type(location.byte_column) ~= 'number'
      or location.byte_column < 0
      or location.byte_column % 1 ~= 0
    then
      vim.notify('pdfterm: invalid inverse-search JSON location', vim.log.levels.ERROR)
      return
    end
    inverse_search(location.file, location.line, location.byte_column)
  end)
end

-- Consumes one accepted client connection end-to-end: parses its payload and
-- closes it. uv pipes are byte-stream pipes here (payload arrives as one or
-- more chunks), so accumulate until EOF.
local function serve_client(client)
  local payload, size = {}, 0
  local timer = assert(vim.uv.new_timer())
  local function finish(message, deliver)
    if client:is_closing() then
      return
    end
    timer:stop()
    timer:close()
    client:read_stop()
    client:close()
    if message then
      vim.schedule(function()
        vim.notify('pdfterm inverse search: ' .. message, vim.log.levels.ERROR)
      end)
    elseif deliver then
      handle_line(table.concat(payload))
    end
  end
  timer:start(1000, 0, function()
    finish 'request timed out'
  end)
  client:read_start(function(err, chunk)
    if err then
      finish(err)
    elseif not chunk then
      finish(nil, true)
    else
      size = size + #chunk
      if size > 16384 then
        finish 'request exceeds 16384 bytes'
      else
        payload[#payload + 1] = chunk
      end
    end
  end)
end

local function arm_listen(server)
  server:listen(5, function(listen_err)
    if listen_err then
      vim.schedule(function()
        vim.notify('pdfterm socket listen failed: ' .. tostring(listen_err), vim.log.levels.ERROR)
      end)
      return
    end
    local client = vim.uv.new_pipe(false)
    if not client then
      return
    end
    server:accept(client)
    serve_client(client)
  end)
end

function M.setup()
  platform.check_supported()
  if M._listening then
    return
  end
  local result = vim.system({ options.executable, '--print-config' }, { text = true, timeout = 10000 }):wait()
  if result.code ~= 0 then
    error('pdfterm configuration: ' .. vim.trim(result.stderr or 'viewer failed'))
  end
  local config = vim.json.decode(result.stdout)
  local executable = options.executable
  options = vim.tbl_extend('force', config.nvim, {
    editor = config.editor,
    forward_socket = config.forward_socket,
    executable = config.nvim.executable ~= '' and config.nvim.executable or executable,
  })
  setup_keys(options)
  vim.api.nvim_create_autocmd('VimLeavePre', {
    group = vim.api.nvim_create_augroup('pdfterm_lifetime', { clear = true }),
    once = true,
    callback = close_owned_splits,
  })
  if options.editor.transport ~= 'socket' then
    return
  end

  local path = options.editor.path
  local parent = assert(vim.uv.fs_lstat(vim.fs.dirname(path)))
  if parent.type ~= 'directory' or parent.uid ~= vim.uv.getuid() or bit.band(parent.mode, 63) ~= 0 then
    error 'pdfterm: socket parent must be a current-user-owned mode-0700 directory'
  end
  local pipe = assert(vim.uv.new_pipe(false))
  local ok, bind_err = pipe:bind(path)
  if not ok then
    pipe:close()
    error('pdfterm: cannot bind ' .. path .. ': ' .. tostring(bind_err) .. '; stop the existing editor or explicitly remove its stale socket')
  end
  local identity = assert(vim.uv.fs_lstat(path))
  local function close_listener()
    if not pipe:is_closing() then
      pipe:close()
    end
    local current = vim.uv.fs_lstat(path)
    if current and current.dev == identity.dev and current.ino == identity.ino then
      local removed, unlink_err = vim.uv.fs_unlink(path)
      if not removed then
        vim.notify('pdfterm: socket cleanup failed: ' .. tostring(unlink_err), vim.log.levels.ERROR)
      end
    end
  end
  local secured, chmod_err = vim.uv.fs_chmod(path, 384) -- 0600
  if not secured then
    close_listener()
    error('pdfterm: cannot secure socket: ' .. tostring(chmod_err))
  end
  arm_listen(pipe)
  M._pipe = pipe
  M._listening = true
  vim.api.nvim_create_autocmd('VimLeavePre', { once = true, callback = close_listener })
end

local pending_forward

function M.forward_search(pdf_path, payload, source_terminal)
  if options.forward_socket == '' then
    vim.notify('pdfterm: forward_socket is disabled in config.toml', vim.log.levels.ERROR)
    return
  end
  M._source_terminal = source_terminal
  if pending_forward then
    if pending_forward.pdf ~= pdf_path then
      vim.notify('pdfterm is still opening ' .. pending_forward.pdf, vim.log.levels.ERROR)
      return
    end
    pending_forward.payload = payload
    return
  end
  local request = { pdf = pdf_path, payload = payload }
  pending_forward = request
  local launched, attempts = false, 0
  local attempt
  local function fail(message)
    pending_forward = nil
    vim.notify(message, vim.log.levels.ERROR)
  end
  local function launch()
    launched = true
    local ok, process = pcall(terminal.launch_split, source_terminal, options.executable, pdf_path, function(result, split)
      if split then
        owned_splits[#owned_splits + 1] = split
      end
      split_launch = nil
      vim.schedule(function()
        if exiting then
          return
        end
        if not split then
          fail('pdfterm: terminal split failed: ' .. (result.stderr or 'missing terminal ID'))
          return
        end
        attempt()
      end)
    end)
    if ok then
      split_launch = process
    else
      fail('pdfterm: could not start terminal split command: ' .. tostring(process))
    end
  end
  attempt = function()
    if exiting then
      return
    end
    local pipe = assert(vim.uv.new_pipe(false))
    pipe:connect(options.forward_socket, function(err)
      if err then
        pipe:close()
        vim.schedule(function()
          if not err:match 'ENOENT' and not err:match 'ECONNREFUSED' then
            fail('pdfterm: forward-search connection failed: ' .. err)
          elseif not launched then
            launch()
          else
            attempts = attempts + 1
            if attempts >= 25 then
              fail 'pdfterm: terminal split did not open its forward socket.'
            else
              vim.defer_fn(attempt, 200)
            end
          end
        end)
        return
      end
      -- Send and half-close directly in libuv callbacks: editor prompts must not
      -- delay the payload/EOF beyond the viewer's bounded receive deadline.
      local sent_payload = request.payload
      local timer = assert(vim.uv.new_timer())
      local chunks, size = {}, 0
      local function finish(message)
        if pipe:is_closing() then
          return
        end
        timer:stop()
        timer:close()
        pipe:close()
        vim.schedule(function()
          if message then
            fail('pdfterm: ' .. message)
            return
          end
          local ok, reply = pcall(vim.json.decode, table.concat(chunks))
          if not ok or type(reply) ~= 'table' or reply.ok ~= true then
            fail('pdfterm: ' .. (ok and type(reply) == 'table' and reply.error or 'invalid forward reply'))
          elseif request.payload ~= sent_payload then
            attempt()
          else
            pending_forward = nil
          end
        end)
      end
      timer:start(2000, 0, function()
        finish 'forward reply timed out'
      end)
      pipe:read_start(function(read_err, chunk)
        if read_err then
          finish(read_err)
        elseif not chunk then
          finish()
        else
          size = size + #chunk
          if size > 4096 then
            finish 'forward reply exceeds 4096 bytes'
          else
            chunks[#chunks + 1] = chunk
          end
        end
      end)
      pipe:write(sent_payload, function(write_err)
        if pipe:is_closing() then
          return
        end
        if write_err then
          finish(write_err)
          return
        end
        pipe:shutdown(function(shutdown_err)
          if shutdown_err then
            finish(shutdown_err)
          end
        end)
      end)
    end)
  end
  attempt()
end

setup_keys = function(opts)
  local main_tex_file = nil
  local latex_compile = opts.compile
  local latex_viewer = opts.viewer

  local function set_latex_viewer(viewer)
    latex_viewer = viewer
    vim.notify('PDF viewer is now: ' .. latex_viewer, vim.log.levels.INFO)
  end

  local function toggle_latex_compile()
    latex_compile = not latex_compile
    vim.notify('Compile flag is now: ' .. tostring(latex_compile))
  end

  local function change_main_tex_file_name()
    main_tex_file = nil
    local buf_name = vim.api.nvim_buf_get_name(0)
    local tex_file = buf_name:match '^(.*)%.tex$'
    if tex_file then
      main_tex_file = tex_file
      vim.notify('Set current latex file to ' .. main_tex_file, vim.log.levels.INFO)
    else
      vim.notify('Could not determine TeX filename. Please set main file manually.', vim.log.levels.INFO)
    end
  end

  local function compile_tex(main_tex_path, on_success)
    vim.system(
      { 'latexmk', '-pdf', '-interaction=nonstopmode', '-synctex=1', main_tex_path },
      { text = true, cwd = vim.fn.fnamemodify(main_tex_path, ':h') },
      vim.schedule_wrap(function(result)
        if result.code ~= 0 then
          local output = vim.trim((result.stderr ~= '' and result.stderr or result.stdout) or '')
          vim.notify(output ~= '' and output or 'latexmk exited with code ' .. result.code, vim.log.levels.ERROR, { title = 'latexmk' })
        elseif on_success then
          on_success()
        end
      end)
    )
  end

  local function build_main_tex_file()
    if main_tex_file == nil then
      change_main_tex_file_name()
    end
    if main_tex_file == nil then
      return
    end

    vim.cmd 'w'
    compile_tex(main_tex_file .. '.tex')
  end

  local function build_user_tex_with_skim_forward_search()
    if main_tex_file == nil then
      change_main_tex_file_name()
    end
    if main_tex_file == nil then
      vim.notify('Could not determine TeX filename. Use <leader>csl to set main file manually.', vim.log.levels.INFO)
      return
    end
    local main_tex_path = main_tex_file .. '.tex'
    vim.cmd 'w'

    local pdf_path = main_tex_file .. '.pdf'
    local cursor = vim.api.nvim_win_get_cursor(0)
    local line_number = cursor[1]
    local column_number = vim.fn.strchars(vim.api.nvim_get_current_line():sub(1, cursor[2])) + 1
    local file_path = vim.fn.expand '%:p'

    local viewer = latex_viewer
    local source
    if viewer == 'terminal' then
      source = terminal.capture_source()
    end
    local function forward_search()
      if viewer == 'terminal' then
        -- Resolve against the completed build through the shared Rust parser.
        local result = vim
          .system({
            options.executable,
            pdf_path,
            '--synctex-view',
            file_path,
            '--line',
            tostring(line_number),
            '--column',
            tostring(column_number),
          }, { text = true })
          :wait()
        if result.code ~= 0 then
          vim.notify('pdfterm: ' .. (result.stderr or 'SyncTeX resolution failed'), vim.log.levels.ERROR)
          return
        end
        local payload = result.stdout
        M.forward_search(pdf_path, payload, source)
        return
      end

      platform.skim_forward(line_number, pdf_path, file_path)
    end

    if latex_compile then
      compile_tex(main_tex_path, forward_search)
    else
      forward_search()
    end
  end

  local function map(key, callback, settings)
    if key ~= '' then
      vim.keymap.set('n', key, callback, settings)
    end
  end
  map(opts.keys.forward, build_user_tex_with_skim_forward_search, { desc = 'pdfterm forward search' })
  map(opts.keys.main_file, change_main_tex_file_name, { desc = 'pdfterm set main TeX file' })
  map(opts.keys.compile, toggle_latex_compile, { desc = 'pdfterm toggle compilation' })
  map(opts.keys.skim, function()
    set_latex_viewer 'skim'
  end, { desc = '[C]ode [S]et [L]atex [S]kim viewer' })
  map(opts.keys.terminal, function()
    set_latex_viewer 'terminal'
  end, { desc = '[C]ode [S]et [L]atex [T]erminal viewer' })
  vim.api.nvim_create_autocmd('FileType', {
    group = vim.api.nvim_create_augroup('latex-build-keymap', { clear = true }),
    pattern = { 'latex', 'tex' },
    callback = function(event)
      map(opts.keys.build, build_main_tex_file, { buffer = event.buf, desc = 'pdfterm build TeX' })
    end,
  })
end

return M
