use super::runtime::{LuaError, Runtime};
use mlua::{Function, Lua, Table, Value};
use std::cell::RefCell;

pub trait Host {
    fn run(&mut self, argv: Vec<String>) -> Result<String, String>;
    fn expand(&mut self, format: &str, target: Option<&str>) -> Result<String, String>;
    fn bind(
        &mut self,
        table: &str,
        key: &str,
        id: u32,
        desc: Option<String>,
        repeat: bool,
    ) -> Result<(), String>;
    fn subscribe(&mut self, event: &str, id: u32) -> Result<(), String>;
    fn every(&mut self, ms: u64, id: u32) -> Result<(), String>;
    fn system(
        &mut self,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: u64,
        id: u32,
    ) -> Result<(), String>;
    fn cancel(&mut self, id: u32);
    fn log(&mut self, msg: &str);
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    None,
    Event(serde_json::Value),
    System {
        code: i32,
        stdout: String,
        stderr: String,
    },
}

const SLOT: &str = "grofe_host";
const CALLBACKS: &str = "grofe_callbacks";
const PLUGINS: &str = "grofe_plugins";
const NEXT_ID: &str = "grofe_next_id";
const MIN_EVERY_MS: u64 = 100;
const DEFAULT_SYSTEM_TIMEOUT_MS: u64 = 10_000;

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

fn setup(e: mlua::Error) -> LuaError {
    LuaError {
        who: "grofe".into(),
        message: e.to_string(),
    }
}

/// The scoped functions `enter` put in place, or an error naming `name`.
fn slot(lua: &Lua, name: &str) -> mlua::Result<Table> {
    match lua.named_registry_value::<Value>(SLOT)? {
        Value::Table(t) => Ok(t),
        _ => Err(err(format!("grofe.{name} outside a grofe call"))),
    }
}

fn host_fn(lua: &Lua, name: &str) -> mlua::Result<Function> {
    slot(lua, name)?.get(name)
}

/// Stores `f` under the next callback id, after `register` accepted that id.
fn add_callback(
    lua: &Lua,
    f: Function,
    register: impl FnOnce(u32) -> mlua::Result<()>,
) -> mlua::Result<u32> {
    let id: u32 = lua.named_registry_value(NEXT_ID)?;
    register(id)?;
    lua.named_registry_value::<Table>(CALLBACKS)?.set(id, f)?;
    lua.set_named_registry_value(NEXT_ID, id + 1)?;
    Ok(id)
}

fn strings(t: Table) -> mlua::Result<Vec<String>> {
    t.sequence_values::<String>().collect()
}

pub fn install(rt: &Runtime) -> Result<(), LuaError> {
    install_in(rt.lua()).map_err(setup)
}

fn install_in(lua: &Lua) -> mlua::Result<()> {
    lua.set_named_registry_value(CALLBACKS, lua.create_table()?)?;
    lua.set_named_registry_value(PLUGINS, lua.create_table()?)?;
    lua.set_named_registry_value(NEXT_ID, 1u32)?;
    let g = lua.create_table()?;
    g.set("api", crate::pack::PLUGIN_API)?;
    g.set(
        "cmd",
        lua.create_function(|lua, arg: Value| {
            let run = host_fn(lua, "cmd")?;
            let commands = match arg {
                Value::String(s) => {
                    crate::cmd::parse::split_command_string(&s.to_str()?).map_err(err)?
                }
                Value::Table(t) => vec![strings(t)?],
                _ => return Err(err("grofe.cmd takes an argv table or a string")),
            };
            let mut out = Vec::new();
            for argv in commands {
                out.push(run.call::<String>(argv)?);
            }
            Ok(out.join("\n"))
        })?,
    )?;
    g.set(
        "fmt",
        lua.create_function(|lua, (format, target): (String, Option<String>)| {
            host_fn(lua, "fmt")?.call::<String>((format, target))
        })?,
    )?;
    g.set(
        "bind",
        lua.create_function(
            |lua, (table, key, f, opts): (String, String, Function, Option<Table>)| {
                let host = host_fn(lua, "bind")?;
                let (desc, repeat) = match opts {
                    Some(o) => (
                        o.get::<Option<String>>("desc")?,
                        o.get::<Option<bool>>("repeat")?.unwrap_or(false),
                    ),
                    None => (None, false),
                };
                add_callback(lua, f, |id| host.call((table, key, id, desc, repeat)))
            },
        )?,
    )?;
    g.set(
        "on",
        lua.create_function(|lua, (event, f): (String, Function)| {
            let host = host_fn(lua, "on")?;
            add_callback(lua, f, |id| host.call((event, id)))
        })?,
    )?;
    g.set(
        "every",
        lua.create_function(|lua, (ms, f): (f64, Function)| {
            let host = host_fn(lua, "every")?;
            let ms = (ms.max(0.0) as u64).max(MIN_EVERY_MS);
            add_callback(lua, f, |id| host.call((ms, id)))
        })?,
    )?;
    g.set(
        "system",
        lua.create_function(|lua, (argv, f, opts): (Table, Function, Option<Table>)| {
            let host = host_fn(lua, "system")?;
            let argv = strings(argv)?;
            let (cwd, timeout) = match opts {
                Some(o) => (
                    o.get::<Option<String>>("cwd")?,
                    o.get::<Option<u64>>("timeout_ms")?
                        .unwrap_or(DEFAULT_SYSTEM_TIMEOUT_MS),
                ),
                None => (None, DEFAULT_SYSTEM_TIMEOUT_MS),
            };
            add_callback(lua, f, |id| host.call((argv, cwd, timeout, id)))
        })?,
    )?;
    g.set(
        "cancel",
        lua.create_function(|lua, id: u32| {
            host_fn(lua, "cancel")?.call::<()>(id)?;
            lua.named_registry_value::<Table>(CALLBACKS)?
                .set(id, Value::Nil)
        })?,
    )?;
    g.set(
        "log",
        lua.create_function(|lua, msg: String| host_fn(lua, "log")?.call::<()>(msg))?,
    )?;
    g.set(
        "plugin",
        lua.create_function(|lua, name: String| {
            lua.named_registry_value::<Table>(PLUGINS)?
                .get::<Value>(name)
        })?,
    )?;
    lua.globals().set("grofe", g)
}

pub fn enter<R>(
    rt: &Runtime,
    host: &mut dyn Host,
    who: &str,
    f: impl FnOnce(&Lua) -> mlua::Result<R>,
) -> Result<R, LuaError> {
    rt.guarded(who, |lua| {
        let host = RefCell::new(host);
        let result = lua.scope(|scope| {
            let t = lua.create_table()?;
            t.set(
                "cmd",
                scope.create_function_mut(|_, argv: Vec<String>| {
                    host.borrow_mut().run(argv).map_err(err)
                })?,
            )?;
            t.set(
                "fmt",
                scope.create_function_mut(|_, (format, target): (String, Option<String>)| {
                    host.borrow_mut()
                        .expand(&format, target.as_deref())
                        .map_err(err)
                })?,
            )?;
            t.set(
                "bind",
                scope.create_function_mut(
                    |_,
                     (table, key, id, desc, repeat): (
                        String,
                        String,
                        u32,
                        Option<String>,
                        bool,
                    )| {
                        host.borrow_mut()
                            .bind(&table, &key, id, desc, repeat)
                            .map_err(err)
                    },
                )?,
            )?;
            t.set(
                "on",
                scope.create_function_mut(|_, (event, id): (String, u32)| {
                    host.borrow_mut().subscribe(&event, id).map_err(err)
                })?,
            )?;
            t.set(
                "every",
                scope.create_function_mut(|_, (ms, id): (u64, u32)| {
                    host.borrow_mut().every(ms, id).map_err(err)
                })?,
            )?;
            t.set(
                "system",
                scope.create_function_mut(
                    |_, (argv, cwd, timeout, id): (Vec<String>, Option<String>, u64, u32)| {
                        host.borrow_mut()
                            .system(argv, cwd, timeout, id)
                            .map_err(err)
                    },
                )?,
            )?;
            t.set(
                "cancel",
                scope.create_function_mut(|_, id: u32| {
                    host.borrow_mut().cancel(id);
                    Ok(())
                })?,
            )?;
            t.set(
                "log",
                scope.create_function_mut(|_, msg: String| {
                    host.borrow_mut().log(&msg);
                    Ok(())
                })?,
            )?;
            lua.set_named_registry_value(SLOT, t)?;
            let r = f(lua);
            lua.set_named_registry_value(SLOT, Value::Nil)?;
            r
        });
        // A failed scope setup must not leave a dead slot behind.
        let _ = lua.set_named_registry_value(SLOT, Value::Nil);
        result
    })
}

pub fn call(rt: &Runtime, host: &mut dyn Host, id: u32, payload: Payload) -> Result<(), LuaError> {
    let who = format!("callback {id}");
    enter(rt, host, &who, |lua| {
        let f: Value = lua.named_registry_value::<Table>(CALLBACKS)?.get(id)?;
        let Value::Function(f) = f else {
            return Ok(());
        };
        match payload {
            Payload::None => f.call::<()>(()),
            Payload::Event(v) => f.call::<()>(json_to_lua(lua, &v)?),
            Payload::System {
                code,
                stdout,
                stderr,
            } => {
                let t = lua.create_table()?;
                t.set("code", code)?;
                t.set("stdout", stdout)?;
                t.set("stderr", stderr)?;
                f.call::<()>(t)
            }
        }
    })
}

fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        J::String(s) => Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table()?;
            for (k, item) in map {
                t.set(k.as_str(), json_to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
    })
}

pub fn set_plugin(rt: &Runtime, name: &str, value: Value) -> Result<(), LuaError> {
    rt.lua()
        .named_registry_value::<Table>(PLUGINS)
        .and_then(|t| t.set(name, value))
        .map_err(setup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::runtime::Limits;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct FakeHost {
        runs: Vec<Vec<String>>,
        reply: String,
        fail: Option<String>,
        binds: Vec<(String, String, u32, Option<String>, bool)>,
        everys: Vec<(u64, u32)>,
        cancelled: Vec<u32>,
    }

    impl Host for FakeHost {
        fn run(&mut self, argv: Vec<String>) -> Result<String, String> {
            self.runs.push(argv);
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(self.reply.clone()),
            }
        }
        fn expand(&mut self, format: &str, _: Option<&str>) -> Result<String, String> {
            Ok(format!("<{format}>"))
        }
        fn bind(
            &mut self,
            table: &str,
            key: &str,
            id: u32,
            desc: Option<String>,
            repeat: bool,
        ) -> Result<(), String> {
            self.binds
                .push((table.into(), key.into(), id, desc, repeat));
            Ok(())
        }
        fn subscribe(&mut self, _: &str, _: u32) -> Result<(), String> {
            Ok(())
        }
        fn every(&mut self, ms: u64, id: u32) -> Result<(), String> {
            self.everys.push((ms, id));
            Ok(())
        }
        fn system(
            &mut self,
            _: Vec<String>,
            _: Option<String>,
            _: u64,
            _: u32,
        ) -> Result<(), String> {
            Ok(())
        }
        fn cancel(&mut self, id: u32) {
            self.cancelled.push(id);
        }
        fn log(&mut self, _: &str) {}
    }

    fn rt() -> Runtime {
        let rt = Runtime::new(Limits::default()).unwrap();
        install(&rt).unwrap();
        rt
    }

    fn eval<R: mlua::FromLuaMulti>(
        rt: &Runtime,
        host: &mut FakeHost,
        src: &str,
    ) -> Result<R, LuaError> {
        enter(rt, host, "t", |l| l.load(src).eval::<R>())
    }

    #[test]
    fn test_cmd_table_reaches_host() {
        let rt = rt();
        let mut h = FakeHost {
            reply: "%3".into(),
            ..Default::default()
        };
        let out: String = eval(&rt, &mut h, "return grofe.cmd{'split-window','-P'}").unwrap();
        assert_eq!(out, "%3");
        assert_eq!(h.runs, vec![vec!["split-window", "-P"]]);
    }

    #[test]
    fn test_cmd_string_splits_each_command() {
        let rt = rt();
        let mut h = FakeHost {
            reply: "x".into(),
            ..Default::default()
        };
        let out: String = eval(
            &rt,
            &mut h,
            "return grofe.cmd('kill-pane -t %1; new-window')",
        )
        .unwrap();
        assert_eq!(
            h.runs,
            vec![vec!["kill-pane", "-t", "%1"], vec!["new-window"]]
        );
        assert_eq!(out, "x\nx");
    }

    #[test]
    fn test_bind_registers_and_returns_id() {
        let rt = rt();
        let mut h = FakeHost::default();
        let id: u32 = eval(
            &rt,
            &mut h,
            "return grofe.bind('prefix', 'g', function() end, {desc = 'Git'})",
        )
        .unwrap();
        assert_eq!(id, 1);
        assert_eq!(
            h.binds,
            vec![("prefix".into(), "g".into(), 1, Some("Git".into()), false)]
        );
    }

    #[test]
    fn test_cmd_error_raises() {
        let rt = rt();
        let mut h = FakeHost {
            fail: Some("no such pane".into()),
            ..Default::default()
        };
        let msg: String = eval(
            &rt,
            &mut h,
            "local ok, e = pcall(grofe.cmd, {'kill-pane'}) return tostring(e)",
        )
        .unwrap();
        assert!(msg.contains("no such pane"), "{msg}");
    }

    #[test]
    fn test_call_unknown_id_is_ok() {
        let rt = rt();
        let mut h = FakeHost::default();
        call(&rt, &mut h, 99, Payload::None).unwrap();
        eval::<()>(
            &rt,
            &mut h,
            "local id = grofe.every(500, function() end) grofe.cancel(id)",
        )
        .unwrap();
        call(&rt, &mut h, 1, Payload::None).unwrap();
        assert_eq!(h.cancelled, vec![1]);
    }

    #[test]
    fn test_grofe_outside_enter_errors() {
        let rt = rt();
        let e = rt.exec("o", "grofe.cmd{'x'}").unwrap_err();
        assert!(e.message.contains("grofe.cmd outside a grofe call"), "{e}");
    }

    #[test]
    fn test_every_below_100ms_clamps() {
        let rt = rt();
        let mut h = FakeHost::default();
        eval::<()>(&rt, &mut h, "grofe.every(5, function() end)").unwrap();
        assert_eq!(h.everys, vec![(100, 1)]);
    }

    #[test]
    fn test_api_is_1() {
        let rt = rt();
        assert_eq!(rt.lua().load("return grofe.api").eval::<u32>().unwrap(), 1);
    }

    #[test]
    fn test_call_passes_payloads() {
        let rt = rt();
        let mut h = FakeHost::default();
        eval::<()>(&rt, &mut h, "grofe.on('e', function(p) seen = p.a[1] end) grofe.system({'x'}, function(r) code = r.code end)").unwrap();
        call(
            &rt,
            &mut h,
            1,
            Payload::Event(serde_json::json!({"a": [7]})),
        )
        .unwrap();
        call(
            &rt,
            &mut h,
            2,
            Payload::System {
                code: 3,
                stdout: String::new(),
                stderr: String::new(),
            },
        )
        .unwrap();
        let g = rt.lua().globals();
        assert_eq!(
            (g.get::<i64>("seen").unwrap(), g.get::<i64>("code").unwrap()),
            (7, 3)
        );
    }

    #[test]
    fn test_plugin_value_round_trips() {
        let rt = rt();
        set_plugin(&rt, "p", Value::Integer(5)).unwrap();
        let mut h = FakeHost::default();
        assert_eq!(
            eval::<i64>(&rt, &mut h, "return grofe.plugin('p')").unwrap(),
            5
        );
        assert!(
            eval::<Value>(&rt, &mut h, "return grofe.plugin('q')")
                .unwrap()
                .is_nil()
        );
    }

    #[test]
    fn test_callback_over_budget_errors() {
        let rt = rt();
        let mut h = FakeHost::default();
        eval::<()>(
            &rt,
            &mut h,
            "grofe.bind('root', 'x', function() while true do end end)",
        )
        .unwrap();
        let t = Instant::now();
        let e = call(&rt, &mut h, 1, Payload::None).unwrap_err();
        assert!(
            e.who.contains("callback 1") && e.message.contains("budget"),
            "{e}"
        );
        assert!(t.elapsed() < Duration::from_millis(150));
    }
}
