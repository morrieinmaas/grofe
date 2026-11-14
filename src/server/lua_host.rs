use std::path::Path;

use crate::lua::api::Payload;
use crate::protocol::ClientCtx;
use crate::server::State;

pub struct LuaHost {
    _private: (),
}

#[allow(unused_variables)]
pub fn reset(state: &mut State) {
    todo!("T3")
}

#[allow(unused_variables)]
pub fn load_pack(state: &mut State, name: &str, dir: &Path) -> Result<(), String> {
    todo!("T3")
}

#[allow(unused_variables)]
pub fn load_init(state: &mut State, config_dir: &Path) {
    todo!("T3")
}

#[allow(unused_variables)]
pub fn run_chunk(state: &mut State, ctx: &ClientCtx, src: &str) -> Result<String, String> {
    todo!("T3")
}

#[allow(unused_variables)]
pub fn call(state: &mut State, id: u32, payload: Payload) {
    todo!("T3")
}
