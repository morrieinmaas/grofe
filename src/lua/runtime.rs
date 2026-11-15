use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub budget: Duration,
    pub memory: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            budget: Duration::from_millis(50),
            memory: 64 << 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaError {
    pub who: String,
    pub message: String,
}

impl std::fmt::Display for LuaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.who, self.message)
    }
}

impl std::error::Error for LuaError {}

pub struct Runtime {
    lua: mlua::Lua,
    budget: Duration,
    deadline: Arc<Mutex<Option<Instant>>>,
}

fn lock(d: &Mutex<Option<Instant>>) -> std::sync::MutexGuard<'_, Option<Instant>> {
    d.lock().unwrap_or_else(|e| e.into_inner())
}

impl Runtime {
    pub fn new(limits: Limits) -> Result<Runtime, LuaError> {
        let setup = |e: mlua::Error| LuaError {
            who: "runtime".into(),
            message: e.to_string(),
        };
        let lua = mlua::Lua::new();
        let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let d = deadline.clone();
        let budget = limits.budget;
        lua.set_interrupt(move |_| match *lock(&d) {
            Some(at) if Instant::now() >= at => Err(mlua::Error::runtime(format!(
                "over its {} ms budget",
                budget.as_millis()
            ))),
            _ => Ok(mlua::VmState::Continue),
        });
        lua.globals()
            .set("loadstring", mlua::Value::Nil)
            .map_err(setup)?;
        lua.sandbox(true).map_err(setup)?;
        lua.set_memory_limit(limits.memory).map_err(setup)?;
        Ok(Runtime {
            lua,
            budget,
            deadline,
        })
    }

    /// Runs `f` under the time budget. A nested call keeps the earlier deadline.
    pub fn guarded<R>(
        &self,
        who: &str,
        f: impl FnOnce(&mlua::Lua) -> mlua::Result<R>,
    ) -> Result<R, LuaError> {
        let outer = *lock(&self.deadline);
        let mine = Instant::now() + self.budget;
        *lock(&self.deadline) = Some(outer.map_or(mine, |o| o.min(mine)));
        let result = f(&self.lua);
        *lock(&self.deadline) = outer;
        result.map_err(|e| LuaError {
            who: who.to_string(),
            message: clean(&e),
        })
    }

    pub fn exec(&self, who: &str, src: &str) -> Result<(), LuaError> {
        self.guarded(who, |lua| lua.load(src).set_name(who).exec())
    }

    pub fn lua(&self) -> &mlua::Lua {
        &self.lua
    }
}

/// The innermost error text, without mlua's wrapper and traceback.
fn clean(e: &mlua::Error) -> String {
    match e {
        mlua::Error::CallbackError { cause, .. } => clean(cause),
        mlua::Error::RuntimeError(m) => m.lines().next().unwrap_or(m).to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn rt() -> Runtime {
        Runtime::new(Limits::default()).unwrap()
    }
    #[test]
    fn a_spin_ends_within_twice_the_budget() {
        let r = rt();
        let t = Instant::now();
        let e = r.exec("spin", "while true do end").unwrap_err();
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(e.who, "spin");
        assert!(e.message.contains("budget"), "{e}");
    }
    #[test]
    fn pcall_cannot_swallow_the_budget() {
        let r = rt();
        let t = Instant::now();
        assert!(
            r.exec(
                "p",
                "while true do pcall(function() while true do end end) end"
            )
            .is_err()
        );
        assert!(t.elapsed() < Duration::from_millis(100));
    }
    #[test]
    fn the_next_call_after_an_abort_runs() {
        let r = rt();
        let _ = r.exec("a", "while true do end");
        r.exec("b", "y = 2").unwrap();
        assert_eq!(r.lua().globals().get::<i64>("y").unwrap(), 2);
    }
    #[test]
    fn no_blocking_stdlib() {
        rt().exec(
            "s",
            "assert(io == nil and os.execute == nil and loadstring == nil)",
        )
        .unwrap();
    }
    #[test]
    fn memory_is_capped() {
        let r = Runtime::new(Limits {
            memory: 8 << 20,
            budget: Duration::from_secs(5),
        })
        .unwrap();
        let e = r
            .exec(
                "m",
                "local t = {} for i = 1, 1e6 do t[i] = string.rep('x', 65536) .. i end",
            )
            .unwrap_err();
        assert!(e.message.contains("memory"), "{e}");
    }
    #[test]
    fn a_syntax_error_names_the_chunk() {
        assert_eq!(rt().exec("init.luau", "x =").unwrap_err().who, "init.luau");
    }
    #[test]
    fn test_nested_guarded_keeps_outer_deadline() {
        let r = rt();
        r.guarded("outer", |_| {
            // Past the outer 50 ms deadline: a fresh inner budget would spin for another 50 ms.
            std::thread::sleep(Duration::from_millis(60));
            let t = Instant::now();
            let inner = r.exec("inner", "while true do end").unwrap_err();
            assert!(t.elapsed() < Duration::from_millis(25), "{:?}", t.elapsed());
            assert!(inner.message.contains("budget"), "{inner}");
            Ok(())
        })
        .unwrap();
    }

    /// Runs `src`, checks it ended fast with an error containing `want`, then that the VM still runs.
    fn aborts(src: &str, want: &str) {
        let r = Runtime::new(Limits::default()).unwrap();
        let t = Instant::now();
        let e = r.exec("p", src).unwrap_err();
        assert!(
            t.elapsed() < Duration::from_millis(150),
            "{src}: {:?}",
            t.elapsed()
        );
        assert!(e.message.contains(want), "{src}: {e}");
        r.exec("next", "z = 1").unwrap();
        assert_eq!(r.lua().globals().get::<i64>("z").unwrap(), 1);
    }
    #[test]
    fn deep_recursion_is_a_lua_error() {
        aborts(
            "local function f(n) return 1 + f(n + 1) end f(1)",
            "stack overflow",
        );
    }
    #[test]
    fn huge_allocations_are_errors() {
        // Not 2^31: Luau casts the count to int, which wraps negative on x86_64 and yields "".
        aborts("string.rep('x', 2^30 + 1)", "too large");
        aborts("string.rep('x', 1e9)", "memory");
        aborts("string.format('%999999999d', 1)", "invalid format");
        aborts("buffer.create(1e9)", "memory");
    }
    #[test]
    fn the_interrupt_fires_inside_the_pattern_matcher() {
        aborts("string.rep('a', 2e4):match('(.-)(.-)(.-)(.-)x')", "budget");
        aborts("string.rep('a', 2e7):find('%b()')", "budget");
        aborts(
            "string.gsub(string.rep('a', 1e6), 'a', 'bbbbbbbbbbbbbbbbbbbbbbbbbbb')",
            "budget",
        );
    }
    #[test]
    fn spins_in_coroutines_sorts_and_metamethods_abort() {
        aborts(
            "coroutine.wrap(function() while true do end end)()",
            "budget",
        );
        aborts(
            "local c = coroutine.wrap(function() while true do coroutine.yield() end end) while true do c() end",
            "budget",
        );
        aborts(
            "table.sort({3,2,1,5,4}, function(a, b) while true do end end)",
            "budget",
        );
        aborts(
            "local t = setmetatable({}, {__index = function() while true do end end}) return t.x",
            "budget",
        );
        aborts(
            "xpcall(function() error('x') end, function() while true do end end)",
            "budget",
        );
    }
    #[test]
    fn the_memory_limit_still_applies_after_an_oom() {
        let r = Runtime::new(Limits {
            memory: 8 << 20,
            budget: Duration::from_secs(5),
        })
        .unwrap();
        for _ in 0..2 {
            let e = r.exec("m", "string.rep('x', 1e9)").unwrap_err();
            assert!(e.message.contains("memory"), "{e}");
            r.exec("ok", "local s = string.rep('x', 1e6)").unwrap();
        }
    }
    #[test]
    fn the_sandbox_is_closed() {
        let r = rt();
        for src in [
            "rawset(_G, 'zz', 1)",
            "string.zz = 1",
            "require('/etc/passwd')",
            "require('../../etc/passwd')",
            "dofile('/etc/passwd')",
            "collectgarbage('stop')",
            "collectgarbage('setpause', 1000000)",
            "assert(package == nil and io == nil and os.getenv == nil and os.exit == nil and os.remove == nil)",
        ] {
            let res = r.exec("s", src);
            if src.starts_with("assert") {
                res.unwrap();
            } else {
                assert!(res.is_err(), "{src} was allowed");
            }
        }
        // getfenv/setfenv only reach the sandbox env, never the shared read-only globals
        r.exec("f", "getfenv(1).leak = 1 setfenv(1, {})").unwrap();
        assert!(r.exec("g", "string.leak = 1").is_err());
    }
    // ponytail: one C call (a plain find or upper on a string up to the memory cap) is not
    // interruptible; its cost is bounded by the 64 MiB memory limit, measured at ~40 ms.
    #[test]
    fn a_single_uninterruptible_c_call_is_bounded_by_the_memory_cap() {
        let r = rt();
        let t = Instant::now();
        let _ = r.exec("c", "string.rep('a', 5e7):find('b')");
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
    }
}
