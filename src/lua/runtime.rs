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
        let t = Instant::now();
        r.guarded("outer", |_| {
            std::thread::sleep(Duration::from_millis(30));
            let inner = r.exec("inner", "while true do end").unwrap_err();
            assert!(inner.message.contains("budget"), "{inner}");
            Ok(())
        })
        .unwrap();
        assert!(t.elapsed() < Duration::from_millis(75), "{:?}", t.elapsed());
    }
}
