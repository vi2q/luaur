use crate::functions::lua_d_rawrunprotected_ldo::luaD_rawrunprotected;
use crate::type_aliases::lua_state::lua_State;
use crate::type_aliases::pfunc::Pfunc;

#[allow(non_camel_case_types)]
pub unsafe fn lua_d_rawrunprotected_mut(
    L: *mut lua_State,
    f: Pfunc,
    ud: *mut core::ffi::c_void,
) -> i32 {
    luaD_rawrunprotected(L, f, ud) as i32
}
