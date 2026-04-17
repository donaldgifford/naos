-- .nvim.lua
--
-- Project-local Neovim config for naos. Loaded automatically when Neovim is
-- started inside this repository, provided `vim.o.exrc = true` is set in your
-- user config. See DEVELOPMENT.md "Editor setup" for context.
--
-- What this does:
--   Tells rust-analyzer (via rustaceanvim) to index only the crate that can
--   actually compile on the current host. naos-linux depends on KVM and only
--   compiles on Linux; naos-macos depends on Hypervisor.framework and only
--   compiles on macOS. Without this config, rust-analyzer tries to index the
--   whole workspace and fails loudly on the host-wrong crate.
--
-- What this does not do:
--   Stop you from running `cargo` commands against the wrong crate. Use the
--   Justfile recipes (`just check`, `just build`, etc.) which dispatch to the
--   right crate for the current host automatically.

local sysname = vim.loop.os_uname().sysname
local crate

if sysname == "Linux" then
	crate = "crates/naos-linux/Cargo.toml"
elseif sysname == "Darwin" then
	crate = "crates/naos-macos/Cargo.toml"
else
	-- Windows or something exotic. naos does not target these, but we do not
	-- want to hard-fail the editor — just let rust-analyzer do its default
	-- workspace-wide thing and emit whatever errors make sense.
	crate = nil
end

if crate ~= nil then
	vim.g.rustaceanvim = vim.tbl_deep_extend("force", vim.g.rustaceanvim or {}, {
		server = {
			default_settings = {
				["rust-analyzer"] = {
					linkedProjects = { crate },
				},
			},
		},
	})
end
