//! Node: `cxx:Function:Luau.VM:VM/src/ldo.cpp:162:lua_d_throw`
//! Source: `VM/src/ldo.cpp` (ldo.cpp:162-165, hand-ported; C++-exceptions build flavor,
//! matching the catch_unwind-based luaD_rawrunprotected)

#[cfg(not(all(target_arch = "wasm32", panic = "abort", feature = "wasm")))]
use crate::records::lua_exception::lua_exception;
use crate::type_aliases::lua_state::lua_State;

/// Under `wasm32` + `panic = "abort"`, Rust cannot unwind across `luaD_throw`
/// via `catch_unwind`. Route `luaD_throw` / `luaD_rawrunprotected` through a
/// minimal JS `try/catch` trampoline using a private Symbol tag instead.
#[cfg(all(target_arch = "wasm32", panic = "abort", feature = "wasm"))]
pub(crate) mod wasm_abort_eh {
    use crate::type_aliases::lua_state::lua_State;
    use crate::type_aliases::pfunc::Pfunc;
    use core::cell::Cell;
    use wasm_bindgen::prelude::*;

    thread_local! {
        static PENDING_THROW: Cell<Option<(*mut lua_State, core::ffi::c_int)>> =
            const { Cell::new(None) };
    }

    #[wasm_bindgen(inline_js = r#"
const LUAUR_THROW_TAG = Symbol("luaur_throw");
export function __luaur_throw() {
    throw LUAUR_THROW_TAG;
}
export function __luaur_try_call(f) {
    try {
        f();
        return 0;
    } catch (e) {
        if (e === LUAUR_THROW_TAG) {
            return 1;
        }
        throw e;
    }
}
"#)]
    extern "C" {
        fn __luaur_throw();
        fn __luaur_try_call(f: &mut dyn FnMut()) -> i32;
    }

    pub(crate) unsafe fn throw(l: *mut lua_State, errcode: core::ffi::c_int) -> ! {
        PENDING_THROW.with(|slot| {
            debug_assert!(
                slot.get().is_none(),
                "nested luaD_throw while throw pending"
            );
            slot.set(Some((l, errcode)));
        });
        __luaur_throw();
        core::arch::wasm32::unreachable()
    }

    /// Runs `f(l, ud)` across the JS `try/catch` boundary in a dedicated
    /// stack-anchored frame so LLVM emits a `$__stack_pointer` prologue and
    /// epilogue, restoring the Wasm shadow stack immediately when JS catches
    /// `LUAUR_THROW_TAG` and skips inner frames' epilogues.
    #[inline(never)]
    unsafe fn invoke_protected_frame(
        f: Pfunc,
        l: *mut lua_State,
        ud: *mut core::ffi::c_void,
    ) -> i32 {
        let mut anchor = [0u8; 16];
        core::hint::black_box(&mut anchor);
        let mut run = || {
            if let Some(func) = f {
                func(l, ud);
            }
        };
        __luaur_try_call(&mut run)
    }

    pub(crate) unsafe fn rawrunprotected(
        l: *mut lua_State,
        f: Pfunc,
        ud: *mut core::ffi::c_void,
    ) -> core::ffi::c_int {
        let threw = invoke_protected_frame(f, l, ud);
        if threw == 0 {
            return 0;
        }
        let Some((thrown_l, status)) = PENDING_THROW.with(Cell::take) else {
            core::arch::wasm32::unreachable();
        };
        luaur_common::macros::luau_assert::LUAU_ASSERT!(
            thrown_l as *const lua_State == l as *const lua_State
        );
        status
    }
}

#[allow(non_snake_case)]
pub unsafe fn luaD_throw(l: *mut lua_State, errcode: core::ffi::c_int) -> ! {
    #[cfg(all(target_arch = "wasm32", panic = "abort", feature = "wasm"))]
    {
        wasm_abort_eh::throw(l, errcode)
    }
    #[cfg(not(all(target_arch = "wasm32", panic = "abort", feature = "wasm")))]
    {
        std::panic::panic_any(lua_exception::lua_exception(l, errcode));
    }
}

#[allow(unused_imports)]
pub use luaD_throw as lua_d_throw;
