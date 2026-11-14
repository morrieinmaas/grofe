use super::runtime::{LuaError, Runtime};

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

#[allow(unused_variables)]
pub fn install(rt: &Runtime) -> Result<(), LuaError> {
    todo!("T2")
}

#[allow(unused_variables)]
pub fn enter<R>(
    rt: &Runtime,
    host: &mut dyn Host,
    who: &str,
    f: impl FnOnce(&mlua::Lua) -> mlua::Result<R>,
) -> Result<R, LuaError> {
    todo!("T2")
}

#[allow(unused_variables)]
pub fn call(rt: &Runtime, host: &mut dyn Host, id: u32, payload: Payload) -> Result<(), LuaError> {
    todo!("T2")
}

#[allow(unused_variables)]
pub fn set_plugin(rt: &Runtime, name: &str, value: mlua::Value) -> Result<(), LuaError> {
    todo!("T2")
}
