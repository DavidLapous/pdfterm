-- Project builds are serialized; navigation generations begin at user intent.
local M = {}
local generation, builds, processes = 0, {}, {}

function M.current(id) return id == generation end
function M.next() generation = generation + 1; return generation end

-- A detached Unix process owns a process group, so timeouts also kill its children.
function M.run(argv, cwd, timeout, callback)
  local chunks, size, failure = { stdout = {}, stderr = {} }, 0, nil
  local process, timer, finished
  local function kill(reason)
    if finished then return end
    failure = reason
    if process then vim.uv.kill(-process.pid, 9) end
  end
  local function consume(stream)
    return function(error, data)
      if error then kill(tostring(error)); return end
      if not data or failure then return end
      size = size + #data
      if size > 1024 * 1024 then kill('helper output exceeds 1 MiB')
      else chunks[stream][#chunks[stream] + 1] = data end
    end
  end
  local ok, spawned = pcall(vim.system, argv, {
    cwd = cwd, detach = true, stdout = consume('stdout'), stderr = consume('stderr'),
  }, function(result)
    finished = true
    if timer then timer:stop(); timer:close() end
    if process then
      processes[process] = nil
      -- A completed parent must not leave descendants holding pipes or builds.
      vim.uv.kill(-process.pid, 9)
    end
    result.stdout, result.stderr = table.concat(chunks.stdout), failure or table.concat(chunks.stderr)
    if failure then result.code = 1 end
    vim.schedule(function() callback(result) end)
  end)
  if not ok then vim.schedule(function() callback({ code = 1, stderr = tostring(spawned), stdout = '' }) end); return function() end end
  process = spawned
  processes[process] = true
  timer = assert(vim.uv.new_timer())
  timer:start(timeout, 0, function() kill('helper timed out') end)
  return function() kill('navigation cancelled') end
end

function M.describe(config, main)
  local p = config or {}
  local source = p.main or main
  assert(type(source) == 'string' and source:match('%.tex$'), 'pdfterm: select a main TeX file with :PdfTermMain or project.main')
  local cwd = vim.fn.fnamemodify(p.cwd or vim.fn.fnamemodify(source, ':p:h'), ':p')
  cwd = assert(vim.uv.fs_realpath(cwd), 'pdfterm: project working directory does not exist')
  if p.cwd and not vim.startswith(source, '/') then source = cwd .. '/' .. source end
  source = vim.fs.normalize(vim.fn.fnamemodify(source, ':p'))
  local pdf = p.pdf or source:gsub('%.tex$', '.pdf')
  if not vim.startswith(pdf, '/') then pdf = cwd .. '/' .. pdf end
  local argv = p.build or { 'latexmk', '-pdf', '-interaction=nonstopmode', '-synctex=1', source }
  assert(type(argv) == 'table' and #argv > 0, 'pdfterm: project.build must be a nonempty argument vector')
  for _, arg in ipairs(argv) do assert(type(arg) == 'string' and not arg:find('%z'), 'pdfterm: invalid build argument') end
  return { main = source, pdf = vim.fs.normalize(pdf), cwd = cwd, build = argv }
end

function M.build(project, id, callback)
  local job = { project = project, id = id, callback = callback }
  local active = builds[project.cwd] or builds[project.pdf]
  if active then active.pending = job; return end
  builds[project.cwd], builds[project.pdf] = job, job
  M.run(project.build, project.cwd, 120000, function(result)
    local pending = job.pending
    builds[project.cwd], builds[project.pdf] = nil, nil
    -- Never navigate from a superseded intent, even if the old build succeeds.
    if M.current(id) then callback(result) end
    if pending then M.build(pending.project, pending.id, pending.callback) end
  end)
end

function M.close()
  M.next()
  for _, job in pairs(builds) do job.pending = nil end
  for process in pairs(processes) do vim.uv.kill(-process.pid, 9) end
end
return M
