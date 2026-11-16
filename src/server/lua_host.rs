//! The `Host` for the server's `State`: Luau's entry points on the actor.
//! `state.lua` is taken out of `State` for every call and put back after,
//! so the host can hold `&mut State`; a call that finds it taken is nested.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cmd;
use crate::lua::api::{self, Host, Payload};
use crate::lua::runtime::{Limits, LuaError, Runtime};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::input::{Binding, BindingSource, parse_key};
use crate::server::lua_async::Async;
use crate::server::{State, clients, dispatch, prompt};

const PLUGIN: &str = "lua";

pub struct LuaHost {
    rt: Runtime,
    async_: Async,
}

thread_local! {
    /// Set while a Luau call runs on this thread (the actor's).
    static IN_CALL: Cell<bool> = const { Cell::new(false) };
    /// A reload that reached us inside a call: what it asked to load, run
    /// as a job once the call has ended.
    static DEFERRED: RefCell<Option<Deferred>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct Deferred {
    packs: Vec<(String, PathBuf)>,
}

/// Whether a reload is being collected for after the current call.
fn deferring() -> bool {
    IN_CALL.with(Cell::get) && DEFERRED.with(|d| d.borrow().is_some())
}

/// The reload sequence over what a nested reload collected.
fn run_deferred(state: &mut State, d: Deferred, config_dir: PathBuf) {
    reset(state);
    for (name, dir) in &d.packs {
        if let Err(e) = load_pack(state, name, dir) {
            log(state, format!("lua: {e}"));
        }
    }
    load_init(state, &config_dir);
}

struct InCall;

impl InCall {
    fn enter() -> Option<InCall> {
        IN_CALL.with(|c| if c.replace(true) { None } else { Some(InCall) })
    }
}

impl Drop for InCall {
    fn drop(&mut self) {
        IN_CALL.with(|c| c.set(false));
    }
}

struct StateHost<'a> {
    state: &'a mut State,
    async_: &'a mut Async,
    ctx: ClientCtx,
}

fn client_less_ctx() -> ClientCtx {
    ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Grofe,
        request: Default::default(),
    }
}

/// Adds `line` to the server's message log (`show-messages`) and stderr.
fn log(state: &mut State, line: String) {
    eprintln!("grofe {line}");
    clients::log_error(state, "lua", &line);
}

impl Host for StateHost<'_> {
    fn run(&mut self, argv: Vec<String>) -> Result<String, String> {
        dispatch::run_split(self.state, &self.ctx, &argv)
    }

    fn expand(&mut self, format: &str, target: Option<&str>) -> Result<String, String> {
        let mut ctx = self.ctx.clone();
        if let Some(t) = target {
            let target = cmd::target::parse_target(t).map_err(|e| e.to_string())?;
            ctx.pane = Some(cmd::Resolve::resolve_pane(
                &self.state.model,
                &target,
                &self.ctx,
            )?);
        }
        Ok(prompt::expand_command(self.state, &ctx, format))
    }

    fn bind(
        &mut self,
        table: &str,
        key: &str,
        id: u32,
        desc: Option<String>,
        repeat: bool,
    ) -> Result<(), String> {
        let key = parse_key(key)?;
        let cmds = vec![vec!["lua-call".to_string(), id.to_string()]];
        let mut b = Binding::new(cmds, repeat, BindingSource::Plugin(PLUGIN.into()));
        b.shown = desc.clone();
        b.desc = desc;
        self.state.registry.key_tables.insert(table, key, b);
        Ok(())
    }

    fn subscribe(&mut self, event: &str, id: u32) -> Result<(), String> {
        self.async_
            .subscribe(&mut self.state.events, &self.state.jobs, event, id)
    }

    fn every(&mut self, ms: u64, id: u32) -> Result<(), String> {
        self.async_.every(&self.state.jobs, ms, id);
        Ok(())
    }

    fn system(
        &mut self,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: u64,
        id: u32,
    ) -> Result<(), String> {
        self.async_.system(
            &self.state.jobs,
            argv,
            cwd.map(PathBuf::from),
            Duration::from_millis(timeout_ms),
            id,
        );
        Ok(())
    }

    fn cancel(&mut self, id: u32) {
        self.async_.cancel(id);
    }

    fn log(&mut self, msg: &str) {
        log(self.state, format!("lua: {msg}"));
    }
}

/// Runs `f` with the runtime (made on first use) and a host over `state`.
/// `Err` when this is a nested call or the runtime cannot be made.
fn with_host<R>(
    state: &mut State,
    ctx: ClientCtx,
    f: impl FnOnce(&Runtime, &mut dyn Host) -> Result<R, LuaError>,
) -> Result<R, LuaError> {
    let fail = |m: &str| LuaError {
        who: "lua".into(),
        message: m.into(),
    };
    let Some(_guard) = InCall::enter() else {
        return Err(fail("nested call"));
    };
    let mut lua = match state.lua.take() {
        Some(l) => l,
        None => {
            let rt = Runtime::new(Limits::default())?;
            api::install(&rt)?;
            LuaHost {
                rt,
                async_: Async::default(),
            }
        }
    };
    let result = f(
        &lua.rt,
        &mut StateHost {
            state,
            async_: &mut lua.async_,
            ctx,
        },
    );
    if state.lua.is_none() {
        state.lua = Some(lua);
    }
    result
}

fn log_error(state: &mut State, e: &LuaError) {
    log(state, format!("lua: {e}"));
}

/// Drops the runtime with its timers and subscriptions, and every key Lua
/// bound.
pub fn reset(state: &mut State) {
    if IN_CALL.with(Cell::get) {
        DEFERRED.with(|d| *d.borrow_mut() = Some(Deferred::default()));
    }
    if let Some(mut lua) = state.lua.take() {
        lua.async_.cancel_all();
    }
    let ours = BindingSource::Plugin(PLUGIN.into());
    let tables = &mut state.registry.key_tables;
    let names: Vec<String> = tables.table_names().map(String::from).collect();
    for table in names {
        for key in tables.keys(&table) {
            if tables.exact(&table, &key).is_some_and(|b| b.source == ours) {
                tables.unbind(&table, &key);
            }
        }
    }
}

/// `init.luau`, else `init.lua`, in `dir`.
fn init_file(dir: &Path) -> Option<PathBuf> {
    ["init.luau", "init.lua"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.is_file())
}

pub fn load_pack(state: &mut State, name: &str, dir: &Path) -> Result<(), String> {
    if deferring() {
        DEFERRED.with(|d| {
            if let Some(d) = d.borrow_mut().as_mut() {
                d.packs.push((name.to_string(), dir.to_path_buf()));
            }
        });
        return Ok(());
    }
    let file = init_file(dir).ok_or_else(|| format!("{name}: no init.luau"))?;
    let src = std::fs::read_to_string(&file).map_err(|e| format!("{name}: {e}"))?;
    with_host(state, client_less_ctx(), |rt, host| {
        api::enter(rt, host, name, |lua| {
            let value = lua.load(&src).set_name(name).eval::<mlua::Value>()?;
            api::set_plugin(rt, name, value).map_err(mlua::Error::external)
        })
    })
    .map_err(|e| e.to_string())
}

pub fn load_init(state: &mut State, config_dir: &Path) {
    if deferring() {
        let Some(d) = DEFERRED.with(|d| d.borrow_mut().take()) else {
            return;
        };
        let dir = config_dir.to_path_buf();
        let job: crate::server::Job = Box::new(move |state| run_deferred(state, d, dir));
        if state.jobs.try_send(job).is_err() {
            log(
                state,
                "lua: reload inside a call could not be queued".into(),
            );
        }
        return;
    }
    let Some(file) = init_file(config_dir) else {
        return;
    };
    let who = file
        .file_name()
        .map_or_else(|| "init".into(), |n| n.to_string_lossy().into_owned());
    let src = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => return log(state, format!("lua: {who}: {e}")),
    };
    let r = with_host(state, client_less_ctx(), |rt, host| {
        api::enter(rt, host, &who, |lua| lua.load(&src).set_name(&who).exec())
    });
    if let Err(e) = r {
        log_error(state, &e);
    }
}

pub fn run_chunk(state: &mut State, ctx: &ClientCtx, src: &str) -> Result<String, String> {
    with_host(state, ctx.clone(), |rt, host| {
        api::enter(rt, host, "lua", |lua| {
            let values = lua.load(src).set_name("lua").eval::<mlua::MultiValue>()?;
            let parts: Result<Vec<String>, _> = values.iter().map(|v| v.to_string()).collect();
            Ok(parts?.join(" "))
        })
    })
    .map_err(|e| e.to_string())
}

pub fn call(state: &mut State, id: u32, payload: Payload) {
    let r = with_host(state, client_less_ctx(), |rt, host| {
        api::call(rt, host, id, payload)
    });
    if let Err(e) = r {
        log_error(state, &e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{test_ctx, test_state};
    use std::time::Instant;

    fn messages(state: &State) -> String {
        clients::messages(state)
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn run_chunk_returns_values() {
        let mut state = test_state();
        let ctx = test_ctx();
        assert_eq!(run_chunk(&mut state, &ctx, "return 1 + 1"), Ok("2".into()));
        assert_eq!(
            run_chunk(&mut state, &ctx, "return 1, 'a'"),
            Ok("1 a".into())
        );
    }

    #[test]
    fn nested_lua_verb_is_an_error() {
        let mut state = test_state();
        let e = run_chunk(&mut state, &test_ctx(), "grofe.cmd{'lua','return 1'}").unwrap_err();
        assert!(e.contains("nested"), "{e}");
        assert_eq!(
            run_chunk(&mut state, &test_ctx(), "return 3"),
            Ok("3".into())
        );
    }

    #[test]
    fn a_spin_returns_a_budget_error_fast() {
        let mut state = test_state();
        let t = Instant::now();
        let e = run_chunk(&mut state, &test_ctx(), "while true do end").unwrap_err();
        assert!(e.contains("budget"), "{e}");
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn test_reset_unbinds_lua_keys() {
        let mut state = test_state();
        let before = state
            .registry
            .key_tables
            .exact("prefix", &parse_key("c").unwrap())
            .cloned();
        assert!(before.is_some());
        run_chunk(
            &mut state,
            &test_ctx(),
            "grofe.bind('prefix','g',function() end)",
        )
        .unwrap();
        let g = parse_key("g").unwrap();
        assert!(state.registry.key_tables.exact("prefix", &g).is_some());
        reset(&mut state);
        assert!(state.registry.key_tables.exact("prefix", &g).is_none());
        let after = state
            .registry
            .key_tables
            .exact("prefix", &parse_key("c").unwrap())
            .cloned();
        assert_eq!(before, after);
        assert!(state.lua.is_none());
    }

    #[test]
    fn test_load_init_error_logs_and_continues() {
        let mut state = test_state();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("init.luau"), "error('boom')").unwrap();
        load_init(&mut state, dir.path());
        assert!(messages(&state).contains("boom"), "{}", messages(&state));
        assert_eq!(
            run_chunk(&mut state, &test_ctx(), "return 1"),
            Ok("1".into())
        );
    }

    #[test]
    fn test_bind_shows_desc_in_list_keys() {
        let mut state = test_state();
        let ctx = test_ctx();
        run_chunk(
            &mut state,
            &ctx,
            "grofe.bind('prefix','g',function() end,{desc='Git popup'})",
        )
        .unwrap();
        let out = dispatch::run_split(&mut state, &ctx, &["list-keys".into()]).unwrap();
        assert!(out.contains("Git popup"), "{out}");
    }

    #[test]
    fn a_failing_callback_names_itself() {
        let mut state = test_state();
        run_chunk(
            &mut state,
            &test_ctx(),
            "grofe.bind('prefix','g',function() while true do end end)",
        )
        .unwrap();
        call(&mut state, 1, Payload::None);
        assert!(
            messages(&state).contains("callback 1"),
            "{}",
            messages(&state)
        );
        call(&mut state, 99, Payload::None);
    }

    #[test]
    fn test_reload_inside_lua_is_deferred() {
        let (mut state, mut rx) =
            crate::server::test_state_at(PathBuf::from("/nonexistent/x.sock"));
        let ctx = test_ctx();
        assert!(run_chunk(&mut state, &ctx, "grofe.cmd{'reload'}").is_ok());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("init.luau"),
            "grofe.bind('prefix','g',function() end)",
        )
        .unwrap();
        {
            let _in_call = InCall::enter().unwrap();
            reset(&mut state);
            load_init(&mut state, dir.path());
        }
        let g = parse_key("g").unwrap();
        assert!(state.registry.key_tables.exact("prefix", &g).is_none());
        while let Ok(job) = rx.try_recv() {
            job(&mut state);
        }
        assert!(state.registry.key_tables.exact("prefix", &g).is_some());
    }

    #[test]
    fn load_pack_stores_the_return_value() {
        let mut state = test_state();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("init.luau"), "return { x = 1 }").unwrap();
        load_pack(&mut state, "p", dir.path()).unwrap();
        let r = run_chunk(&mut state, &test_ctx(), "return grofe.plugin('p').x");
        assert_eq!(r, Ok("1".into()));
    }
}
