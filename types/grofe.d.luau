-- Luau declarations for the grofe scripting API (version 1).
-- Point luau-lsp at this file; see docs/book/src/lua.md.

export type SystemResult = {
	code: number,
	stdout: string,
	stderr: string,
}

export type BindOptions = {
	desc: string?,
	["repeat"]: boolean?,
}

export type SystemOptions = {
	cwd: string?,
	timeout_ms: number?,
}

declare grofe: {
	api: number,
	cmd: (command: { string } | string) -> string,
	fmt: (format: string, target: string?) -> string,
	bind: (table: string, key: string, fn: () -> (), opts: BindOptions?) -> number,
	on: (event: string, fn: (payload: { [string]: any }) -> ()) -> number,
	every: (ms: number, fn: () -> ()) -> number,
	system: (argv: { string }, fn: (result: SystemResult) -> (), opts: SystemOptions?) -> number,
	cancel: (id: number) -> (),
	log: (msg: string) -> (),
	plugin: (name: string) -> any,
}
