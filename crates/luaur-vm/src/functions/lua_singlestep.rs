use crate::type_aliases::lua_state::lua_State;

pub unsafe fn lua_singlestep(L: *mut lua_State, enabled: core::ffi::c_int) {
    (*L).singlestep = enabled != 0;
}
