// Adapted from mlua (https://github.com/mlua-rs/mlua), MIT License,
// © 2019 Aleksandr Orlenko / mlua authors. See tests/ATTRIBUTION.md.
//
// Ported from mlua `tests/thread.rs`. luaur-rt implements the core coroutine
// surface: `Lua::create_thread`, `Thread::{resume, resume_error, status,
// is_*, reset, to_pointer}`, `ThreadStatus`, `Value::Thread`, and
// `Lua::current_thread`.
//
// Dropped (deferred to later phases — needs subsystems luaur-rt does not yet
// expose):
//   - Thread event callbacks (`ThreadEvent` / `ThreadTriggers` /
//     `set_thread_event_callback` / `remove_thread_event_callback`):
//     `test_thread_event_*`.
//   - `test_thread_reset` in full: it relies on userdata user-values and the
//     `Error::CallbackError` variant; a trimmed reset exercise is kept below.

use std::panic::catch_unwind;

use luaur_rt::{Error, Function, IntoLua, Lua, Result, Thread, Value, Variadic};

#[test]
fn test_thread() -> Result<()> {
    let lua = Lua::new();

    let thread = lua.create_thread(
        lua.load(
            r#"
            function (s)
                local sum = s
                for i = 1,4 do
                    sum = sum + coroutine.yield(sum)
                end
                return sum
            end
            "#,
        )
        .eval()?,
    )?;

    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(0)?, 0);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(1)?, 1);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(2)?, 3);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(3)?, 6);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(4)?, 10);
    assert!(thread.is_finished());

    let accumulate = lua.create_thread(
        lua.load(
            r#"
            function (sum)
                while true do
                    sum = sum + coroutine.yield(sum)
                end
            end
            "#,
        )
        .eval::<Function>()?,
    )?;

    for i in 0..4 {
        accumulate.resume::<()>(i)?;
    }
    assert_eq!(accumulate.resume::<i64>(4)?, 10);
    assert!(accumulate.is_resumable());
    assert!(accumulate.resume::<()>("error").is_err());
    assert!(accumulate.is_error());

    let thread = lua
        .load(
            r#"
            coroutine.create(function ()
                while true do
                    coroutine.yield(42)
                end
            end)
        "#,
        )
        .eval::<Thread>()?;
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(())?, 42);

    let thread: Thread = lua
        .load(
            r#"
            coroutine.create(function(arg)
                assert(arg == 42)
                local yieldarg = coroutine.yield(123)
                assert(yieldarg == 43)
                return 987
            end)
        "#,
        )
        .eval()?;

    assert_eq!(thread.resume::<u32>(42)?, 123);
    assert_eq!(thread.resume::<u32>(43)?, 987);

    match thread.resume::<u32>(()) {
        Err(Error::CoroutineUnresumable) => {}
        Err(_) => panic!("resuming dead coroutine error is not CoroutineUnresumable kind"),
        _ => panic!("resuming dead coroutine did not return error"),
    }

    Ok(())
}

#[test]
fn test_thread_running() -> Result<()> {
    // The thread that is currently executing a Rust callback is "running" and
    // therefore unresumable. (mlua's `test_thread` checks this with
    // `lua.current_thread()`.)
    let lua = Lua::new();

    let thread = lua.create_thread(lua.create_function(|lua, ()| {
        assert!(lua.current_thread().is_running());
        let result = lua.current_thread().resume::<()>(());
        assert!(
            matches!(result, Err(Error::CoroutineUnresumable)),
            "unexpected result: {result:?}",
        );
        Ok(())
    })?)?;
    let result = thread.resume::<()>(());
    assert!(result.is_ok(), "unexpected result: {result:?}");

    Ok(())
}

#[test]
fn test_thread_normal() -> Result<()> {
    // A thread that has resumed another (still-running) thread is "normal" and
    // cannot itself be resumed. (mlua's `test_thread`.)
    let lua = Lua::new();

    let check_outer = lua.create_function(|lua, ()| {
        let outer: Thread = lua.globals().get("outer")?;
        assert!(outer.is_normal());
        assert!(
            matches!(outer.resume::<()>(()), Err(Error::CoroutineUnresumable)),
            "resuming a `normal` thread must be unresumable",
        );
        Ok(())
    })?;
    lua.globals().set("check_outer", check_outer)?;
    let outer = lua.create_thread(
        lua.load(
            r#"
            function()
                local inner = coroutine.create(function() check_outer() end)
                assert(coroutine.resume(inner))
            end
            "#,
        )
        .eval()?,
    )?;
    lua.globals().set("outer", &outer)?;
    outer.resume::<()>(())?;
    assert!(outer.is_finished());

    Ok(())
}

#[test]
fn test_thread_reset() -> Result<()> {
    // Trimmed from mlua's `test_thread_reset` (the full version uses userdata
    // user-values and `Error::CallbackError`). Luau allows resetting a thread
    // in any non-running state and re-binding its body function.
    let lua = Lua::new();

    let func: Function = lua
        .load(r#"function(x) coroutine.yield(x + 1) end"#)
        .eval()?;
    let thread = lua.create_thread(lua.load("return 0").into_function()?)?;
    assert!(thread.reset(func.clone()).is_ok());

    for _ in 0..2 {
        assert!(thread.is_resumable());
        let yielded = thread.resume::<i64>(10)?;
        assert_eq!(yielded, 11);
        assert!(thread.is_resumable());
        thread.resume::<()>(())?;
        assert!(thread.is_finished());
        thread.reset(func.clone())?;
    }

    // Resetting an errored thread is allowed in Luau.
    let func: Function = lua.load(r#"function() error("test error") end"#).eval()?;
    let thread = lua.create_thread(func.clone())?;
    let _ = thread.resume::<()>(());
    assert!(thread.is_error());
    assert!(thread.reset(func.clone()).is_ok());
    assert!(thread.is_resumable());

    Ok(())
}

#[test]
fn test_coroutine_from_closure() -> Result<()> {
    let lua = Lua::new();

    let thrd_main = lua.create_function(|_, ()| Ok(()))?;
    lua.globals().set("main", thrd_main)?;

    let thrd: Thread = lua.load("coroutine.create(main)").eval()?;
    thrd.resume::<()>(())?;

    Ok(())
}

#[test]
#[cfg(not(panic = "abort"))]
fn test_coroutine_panic() {
    match catch_unwind(|| -> Result<()> {
        // check that coroutines propagate panics correctly
        let lua = Lua::new();
        let thrd_main = lua.create_function(|_, ()| -> Result<()> {
            panic!("test_panic");
        })?;
        lua.globals().set("main", &thrd_main)?;
        let thrd: Thread = lua.create_thread(thrd_main)?;
        thrd.resume::<()>(())
    }) {
        // DEVIATION: luaur-rt catches a Rust panic inside a callback and turns
        // it into a catchable Lua error (so the VM is never left in a
        // half-unwound state), rather than re-propagating the panic across the
        // coroutine boundary. We therefore assert the resume surfaces the panic
        // message as an error instead of unwinding.
        Ok(Err(Error::RuntimeError(msg))) => assert!(msg.contains("test_panic")),
        Ok(other) => panic!("unexpected coroutine result: {other:?}"),
        Err(p) => assert!(*p.downcast::<&str>().unwrap() == "test_panic"),
    }
}

#[test]
fn test_thread_pointer() -> Result<()> {
    let lua = Lua::new();

    let func = lua.load("return 123").into_function()?;
    let thread = lua.create_thread(func.clone())?;

    assert_eq!(thread.to_pointer(), thread.clone().to_pointer());
    assert_ne!(thread.to_pointer(), lua.current_thread().to_pointer());

    Ok(())
}

#[test]
fn test_thread_resume_error() -> Result<()> {
    // Luau-specific `resume_error`: resume a coroutine while raising an error at
    // its current yield point.
    let lua = Lua::new();

    let thread = lua
        .load(
            r#"
        coroutine.create(function()
            local ok, err = pcall(coroutine.yield, 123)
            assert(not ok, "yield should fail")
            assert(err == "myerror", "unexpected error: " .. tostring(err))
            return "success"
        end)
    "#,
        )
        .eval::<Thread>()?;

    assert_eq!(thread.resume::<i64>(())?, 123);
    let status = thread.resume_error::<String>("myerror").unwrap();
    assert_eq!(status, "success");

    Ok(())
}

#[test]
fn test_thread_resume_bad_arg() -> Result<()> {
    let lua = Lua::new();

    struct BadArg;

    impl IntoLua for BadArg {
        fn into_lua(self, _lua: &Lua) -> Result<Value> {
            Err(Error::runtime("bad arg"))
        }
    }

    let f = lua.create_thread(lua.create_function(|_, ()| Ok("okay"))?)?;
    let res = f.resume::<()>((123, BadArg));
    assert!(matches!(res, Err(Error::RuntimeError(msg)) if msg == "bad arg"));
    let res = f.resume::<String>(()).unwrap();
    assert_eq!(res, "okay");

    Ok(())
}

/// A resume that carries more values than the destination frame's allowance.
///
/// `Thread::resume` moves the coroutine's results to the parent state with
/// `lua_xmove`, whose `api_check` requires room on the parent's *current frame*
/// (`ci->top - top >= n`). `lua_checkstack` grows the state's allocation but not
/// that allowance, and the main state's base frame allows `LUA_MINSTACK` (20)
/// values, so a coroutine returning more than that tripped
/// `LUAU_ASSERT ... lua_xmove.rs:17` (SIGTRAP) — hit by pillar's cooperative
/// host-call protocol, whose tool calls resume from Rust and can return a table
/// of any size.
#[test]
fn test_thread_resume_more_values_than_the_frame_allowance() -> Result<()> {
    let lua = Lua::new();
    let thread = lua.create_thread(
        lua.load(
            r#"
            return function()
                local values = {}
                for index = 1, 40 do values[index] = index end
                return table.unpack(values)
            end
            "#,
        )
        .eval::<Function>()?,
    )?;

    let values: Variadic<i64> = thread.resume(())?;
    let collected: Vec<i64> = values.into_iter().collect();
    assert_eq!(collected.len(), 40);
    assert_eq!(collected[0], 1);
    assert_eq!(collected[39], 40);
    Ok(())
}

/// The same move, but the coroutine *yielded first*: the shape pillar's host
/// calls have (a tool suspends on a host call, the host answers, the tool
/// finishes). If the post-yield stack window is what `finish_resume` counts as
/// "results", the move carries the function's live registers, not its return
/// values.
#[test]
fn test_thread_resume_after_a_yield_moves_only_the_results() -> Result<()> {
    let lua = Lua::new();
    let thread = lua.create_thread(
        lua.load(
            r#"
            return function()
                -- A frame with plenty of locals, so a stale window is visible.
                local a1, a2, a3, a4, a5, a6, a7, a8, a9, a10 = 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
                local b1, b2, b3, b4, b5, b6, b7, b8, b9, b10 = 11, 12, 13, 14, 15, 16, 17, 18, 19, 20
                local answer = coroutine.yield("waiting")
                return answer, a1 + b10
            end
            "#,
        )
        .eval::<Function>()?,
    )?;

    let yielded: String = thread.resume(())?;
    assert_eq!(yielded, "waiting");
    let (answer, sum): (i64, i64) = thread.resume((7,))?;
    assert_eq!(answer, 7);
    assert_eq!(sum, 21);
    Ok(())
}

/// An *erroring* coroutine with a large stack window.
///
/// `finish_resume`'s error branch moves `lua_gettop(co)` values out of the
/// coroutine **without** reserving room on the parent frame (the success branch
/// does `lua_checkstack` first). An errored coroutine leaves its whole register
/// window on the stack, so a frame with more locals than the parent's remaining
/// allowance trips `api_check!(from, (*(*to).ci).top - (*to).top >= n)` in
/// `lua_xmove` (SIGTRAP). This is the shape pillar hit: a Lua tool suspends on a
/// host call and then raises.
#[test]
fn test_thread_resume_error_with_a_large_stack_window() -> Result<()> {
    let lua = Lua::new();
    let thread = lua.create_thread(
        lua.load(
            r#"
            return function()
                local a1, a2, a3, a4, a5, a6, a7, a8, a9, a10 = 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
                local b1, b2, b3, b4, b5, b6, b7, b8, b9, b10 = 11, 12, 13, 14, 15, 16, 17, 18, 19, 20
                local c1, c2, c3, c4, c5, c6, c7, c8, c9, c10 = 21, 22, 23, 24, 25, 26, 27, 28, 29, 30
                error("boom " .. tostring(a1 + b10 + c1))
            end
            "#,
        )
        .eval::<Function>()?,
    )?;

    let error = thread.resume::<Value>(()).expect_err("the coroutine errors");
    assert!(error.to_string().contains("boom 42"), "{error}");
    Ok(())
}

/// The shape pillar hit: a tool yields (a host call), the host answers, and then
/// a *Rust* host function raises inside the coroutine. A Rust-raised error
/// unwinds differently from a Lua `error(...)`, and the coroutine is left with
/// its whole register window on the stack.
#[test]
fn test_thread_resume_rust_error_after_a_yield_with_a_large_window() -> Result<()> {
    let lua = Lua::new();
    let host = Function::wrap(|()| Err::<Value, Error>(Error::runtime("host failure")));
    let host: Value = {
        use luaur_rt::IntoLua;
        host.into_lua(&lua)?
    };
    let thread = lua.create_thread(
        lua.load(
            r#"
            return function(host)
                local a1, a2, a3, a4, a5, a6, a7, a8, a9, a10 = 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
                local b1, b2, b3, b4, b5, b6, b7, b8, b9, b10 = 11, 12, 13, 14, 15, 16, 17, 18, 19, 20
                local c1 = coroutine.yield("waiting")
                return host()
            end
            "#,
        )
        .eval::<Function>()?,
    )?;

    let yielded: String = thread.resume((host,))?;
    assert_eq!(yielded, "waiting");
    let error = thread
        .resume::<Value>(())
        .expect_err("the host failure propagates");
    assert!(error.to_string().contains("host failure"), "{error}");
    Ok(())
}
