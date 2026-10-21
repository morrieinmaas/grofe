-- nvim-first-run.lua: the alias-day test's first nvim, run headless in
-- stage 2 (`nvim --headless -c 'luafile <this>'`). athome's config installs
-- its plugins on first start: vim.pack's clones, then what their setups
-- start (treesitter's parser downloads and builds, markdown-preview's
-- server). This waits until nvim has run no installer child for five
-- seconds, then quits, so stage 3's nvim starts from a machine that has run
-- nvim once. Long-lived children, such as auto-dark-mode's dbus-monitor,
-- are not installers and do not count.
local installers = {
  git = true, curl = true, wget = true, tar = true, sh = true, bash = true,
  cc = true, gcc = true, ["c++"] = true, ld = true, ["tree-sitter"] = true,
}
local pid = vim.fn.getpid()
local quiet = 0
local timer = assert(vim.uv.new_timer())
timer:start(2000, 500, vim.schedule_wrap(function()
  local busy = false
  for _, child in ipairs(vim.api.nvim_get_proc_children(pid)) do
    local p = vim.api.nvim_get_proc(child)
    if p and installers[p.name] then
      busy = true
    end
  end
  quiet = busy and 0 or quiet + 1
  if quiet >= 10 then
    timer:stop()
    vim.cmd("qa!")
  end
end))
