use std::{mem, ptr};
use std::sync::Arc;
use std::ffi::CStr;
use std::any::Any;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, resume_unwind, UnwindSafe};

use ffi;
use error::{Error, Result};
use safe;

// Checks that Lua has enough free stack space for future stack operations.  On failure, this will
// clear the stack and panic.
pub unsafe fn check_stack(state: *mut ffi::lua_State, amount: c_int) {
    lua_internal_assert!(
        state,
        ffi::lua_checkstack(state, amount) != 0,
        "out of stack space"
    );
}

// Similar to `check_stack`, but returns `Error::StackError` on failure.  Useful for user controlled
// sizes, which should not cause a panic.
pub unsafe fn check_stack_err(state: *mut ffi::lua_State, amount: c_int) -> Result<()> {
    if ffi::lua_checkstack(state, amount) == 0 {
        Err(Error::StackError)
    } else {
        Ok(())
    }
}

// Run an operation on a lua_State and check that the stack change is what is
// expected.  If the stack change does not match, clears the stack and panics.
pub unsafe fn stack_guard<F, R>(state: *mut ffi::lua_State, change: c_int, op: F) -> R
where
    F: FnOnce() -> R,
{
    let expected = ffi::lua_gettop(state) + change;
    lua_internal_assert!(
        state,
        expected >= 0,
        "too many stack values would be popped"
    );

    let res = op();

    let top = ffi::lua_gettop(state);
    lua_internal_assert!(
        state,
        ffi::lua_gettop(state) == expected,
        "expected stack to be {}, got {}",
        expected,
        top
    );

    res
}

// Run an operation on a lua_State and automatically clean up the stack before
// returning.  Takes the lua_State, the expected stack size change, and an
// operation to run.  If the operation results in success, then the stack is
// inspected to make sure the change in stack size matches the expected change
// and otherwise this is a logic error and will panic.  If the operation results
// in an error, the stack is shrunk to the value before the call.  If the
// operation results in an error and the stack is smaller than the value before
// the call, then this is unrecoverable and this will panic.  If this function
// panics, it will clear the stack before panicking.
pub unsafe fn stack_err_guard<F, R>(state: *mut ffi::lua_State, change: c_int, op: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    let expected = ffi::lua_gettop(state) + change;
    lua_internal_assert!(
        state,
        expected >= 0,
        "too many stack values would be popped"
    );

    let res = op();

    let top = ffi::lua_gettop(state);
    if res.is_ok() {
        lua_internal_assert!(
            state,
            ffi::lua_gettop(state) == expected,
            "expected stack to be {}, got {}",
            expected,
            top
        );
    } else {
        lua_internal_assert!(
            state,
            top >= expected,
            "{} too many stack values popped",
            top - expected
        );
        if top > expected {
            ffi::lua_settop(state, expected);
        }
    }
    res
}

// Pops an error off of the stack and returns it. If the error is actually a WrappedPanic, clears
// the current lua stack and continues the panic.  If the error on the top of the stack is actually
// a WrappedError, just returns it.  Otherwise, interprets the error as the appropriate lua error.
// Uses 2 stack spaces, does not call lua_checkstack.
pub unsafe fn pop_error(state: *mut ffi::lua_State, err_code: c_int) -> Error {
    lua_internal_assert!(
        state,
        err_code != ffi::LUA_OK && err_code != ffi::LUA_YIELD,
        "pop_error called with non-error return code"
    );

    if let Some(err) = pop_wrapped_error(state) {
        err
    } else if is_wrapped_panic(state, -1) {
        let panic = get_userdata::<WrappedPanic>(state, -1);
        if let Some(p) = (*panic).0.take() {
            ffi::lua_settop(state, 0);
            resume_unwind(p);
        } else {
            lua_internal_panic!(state, "panic was resumed twice")
        }
    } else {
        let err_string = gc_guard(state, || {
            if let Some(s) = ffi::lua_tostring(state, -1).as_ref() {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            } else {
                "<unprintable error>".to_owned()
            }
        });
        ffi::lua_pop(state, 1);

        match err_code {
            ffi::LUA_ERRRUN => Error::RuntimeError(err_string),
            ffi::LUA_ERRSYNTAX => {
                Error::SyntaxError {
                    // This seems terrible, but as far as I can tell, this is exactly what the
                    // stock Lua REPL does.
                    incomplete_input: err_string.ends_with("<eof>"),
                    message: err_string,
                }
            }
            ffi::LUA_ERRERR => {
                // The Lua manual documents this error wrongly: It is not raised when a message
                // handler errors, but rather when some specific situations regarding stack
                // overflow handling occurs. Since it is not very useful do differentiate
                // between that and "ordinary" runtime errors, we handle them the same way.
                Error::RuntimeError(err_string)
            }
            ffi::LUA_ERRMEM => {
                // This should be impossible, as we set the lua allocator to one that aborts
                // instead of failing.
                lua_internal_abort!("impossible Lua allocation error, aborting!")
            }
            ffi::LUA_ERRGCMM => Error::GarbageCollectorError(err_string),
            _ => lua_internal_panic!(state, "unrecognized lua error code"),
        }
    }
}

// Internally uses 2 stack spaces, does not call checkstack
pub unsafe fn push_string(state: *mut ffi::lua_State, s: &str) -> Result<()> {
    safe::lua_pushlstring(state, s.as_ptr() as *const c_char, s.len())
}

// Internally uses 4 stack spaces, does not call checkstack
pub unsafe fn push_userdata<T>(state: *mut ffi::lua_State, t: T) -> Result<()> {
    let ud = safe::lua_newuserdata(state, mem::size_of::<T>())? as *mut T;
    ptr::write(ud, t);
    Ok(())
}

pub unsafe fn get_userdata<T>(state: *mut ffi::lua_State, index: c_int) -> *mut T {
    let ud = ffi::lua_touserdata(state, index) as *mut T;
    lua_internal_assert!(state, !ud.is_null(), "userdata pointer is null");
    ud
}

// Pops the userdata off of the top of the stack and returns it to rust, invalidating the lua
// userdata.
pub unsafe fn take_userdata<T>(state: *mut ffi::lua_State) -> T {
    // We set the metatable of userdata on __gc to a special table with no __gc method and with
    // metamethods that trigger an error on access.  We do this so that it will not be double
    // dropped, and also so that it cannot be used or identified as any particular userdata type
    // after the first call to __gc.
    get_destructed_userdata_metatable(state);
    ffi::lua_setmetatable(state, -2);
    let ud = ffi::lua_touserdata(state, -1) as *mut T;
    lua_internal_assert!(state, !ud.is_null(), "userdata pointer is null");
    ffi::lua_pop(state, 1);
    ptr::read(ud)
}

#[cfg_attr(unwind, unwind)]
pub unsafe extern "C" fn userdata_destructor<T>(state: *mut ffi::lua_State) -> c_int {
    callback_error(state, || {
        take_userdata::<T>(state);
        Ok(0)
    })
}

// In the context of a lua callback, this will call the given function and if the given function
// returns an error, *or if the given function panics*, this will result in a call to lua_error (a
// longjmp).  The error or panic is wrapped in such a way that when calling pop_error back on
// the rust side, it will resume the panic.
pub unsafe fn callback_error<R, F>(state: *mut ffi::lua_State, f: F) -> R
where
    F: FnOnce() -> Result<R> + UnwindSafe,
{
    match catch_unwind(f) {
        Ok(Ok(r)) => r,
        Ok(Err(err)) => {
            ffi::lua_settop(state, 0);
            ffi::luaL_checkstack(state, 2, ptr::null());
            push_wrapped_error(state, err);
            ffi::lua_error(state)
        }
        Err(p) => {
            ffi::lua_settop(state, 0);
            if ffi::lua_checkstack(state, 2) == 0 {
                lua_internal_abort!("not enough stack space to propagate panic");
            }
            push_wrapped_panic(state, p);
            ffi::lua_error(state)
        }
    }
}

/// Wraps a function conforming to the Lua CFunction protocol, with the addition of being able to
/// panic or return Err, into one conforming to the "Rust Function Protocol", usable with
/// lua_pushrclosure.
pub unsafe fn rust_callback_error<F: FnOnce() -> Result<c_int> + UnwindSafe>(
    state: *mut ffi::lua_State,
    f: F,
) -> c_int {
    match catch_unwind(f) {
        Ok(Ok(r)) => r,
        Ok(Err(Error::StackError)) => ffi::RCALL_STACK_ERR,
        Ok(Err(e)) => {
            ffi::lua_settop(state, 0);
            if ffi::lua_checkstack(state, 2) == 0 {
                ffi::RCALL_STACK_ERR
            } else {
                push_wrapped_error(state, e);
                ffi::RCALL_ERR
            }
        }
        Err(e) => {
            ffi::lua_settop(state, 0);
            if ffi::lua_checkstack(state, 2) == 0 {
                lua_internal_abort!("not enough stack space to throw rust panic");
            } else {
                push_wrapped_panic(state, e);
                ffi::RCALL_ERR
            }
        }
    }
}

// Takes an error at the top of the stack, and if it is a WrappedError, converts it to an
// Error::CallbackError with a traceback, if it is some lua type, prints the error along with a
// traceback, and if it is a WrappedPanic, does not modify it.
pub unsafe extern "C" fn error_traceback(state: *mut ffi::lua_State) -> c_int {
    if ffi::lua_checkstack(state, 2) == 0 {
        // If we don't have enough stack space to even check the error type, do nothing
    } else if is_wrapped_error(state, 1) {
        let traceback = if ffi::lua_checkstack(state, 11) != 0 {
            gc_guard(state, || {
                ffi::luaL_traceback(state, state, ptr::null(), 0);
            });
            let traceback = CStr::from_ptr(ffi::lua_tostring(state, -1))
                .to_string_lossy()
                .into_owned();
            ffi::lua_pop(state, 1);
            traceback
        } else {
            "not enough stack space for traceback".to_owned()
        };

        let error = pop_wrapped_error(state).unwrap();
        push_wrapped_error(
            state,
            Error::CallbackError {
                traceback,
                cause: Arc::new(error),
            },
        );
    } else if !is_wrapped_panic(state, 1) {
        if ffi::lua_checkstack(state, 11) != 0 {
            gc_guard(state, || {
                let s = ffi::lua_tostring(state, 1);
                let s = if s.is_null() {
                    cstr!("<unprintable lua error>")
                } else {
                    s
                };
                ffi::luaL_traceback(state, state, s, 0);
                ffi::lua_remove(state, -2);
            });
        }
    }
    1
}

// A variant of pcall that does not allow lua to catch panic errors from callback_error
#[cfg_attr(unwind, unwind)]
pub unsafe extern "C" fn safe_pcall(state: *mut ffi::lua_State) -> c_int {
    ffi::luaL_checkstack(state, 2, ptr::null());

    let top = ffi::lua_gettop(state);
    if top == 0 {
        ffi::lua_pushstring(state, cstr!("not enough arguments to pcall"));
        ffi::lua_error(state);
    } else if ffi::lua_pcall(state, top - 1, ffi::LUA_MULTRET, 0) != ffi::LUA_OK {
        if is_wrapped_panic(state, -1) {
            ffi::lua_error(state);
        }
        ffi::lua_pushboolean(state, 0);
        ffi::lua_insert(state, -2);
        2
    } else {
        ffi::lua_pushboolean(state, 1);
        ffi::lua_insert(state, 1);
        ffi::lua_gettop(state)
    }
}

// A variant of xpcall that does not allow lua to catch panic errors from callback_error
#[cfg_attr(unwind, unwind)]
pub unsafe extern "C" fn safe_xpcall(state: *mut ffi::lua_State) -> c_int {
    #[cfg_attr(unwind, unwind)]
    unsafe extern "C" fn xpcall_msgh(state: *mut ffi::lua_State) -> c_int {
        ffi::luaL_checkstack(state, 2, ptr::null());

        if is_wrapped_panic(state, -1) {
            1
        } else {
            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
            ffi::lua_insert(state, 1);
            ffi::lua_call(state, ffi::lua_gettop(state) - 1, ffi::LUA_MULTRET);
            ffi::lua_gettop(state)
        }
    }

    ffi::luaL_checkstack(state, 2, ptr::null());

    let top = ffi::lua_gettop(state);
    if top < 2 {
        ffi::lua_pushstring(state, cstr!("not enough arguments to xpcall"));
        ffi::lua_error(state);
    }

    ffi::lua_pushvalue(state, 2);
    ffi::lua_pushcclosure(state, xpcall_msgh, 1);
    ffi::lua_copy(state, 1, 2);
    ffi::lua_replace(state, 1);

    let res = ffi::lua_pcall(state, ffi::lua_gettop(state) - 2, ffi::LUA_MULTRET, 1);
    if res != ffi::LUA_OK {
        if is_wrapped_panic(state, -1) {
            ffi::lua_error(state);
        }
        ffi::lua_pushboolean(state, 0);
        ffi::lua_insert(state, -2);
        2
    } else {
        ffi::lua_pushboolean(state, 1);
        ffi::lua_insert(state, 2);
        ffi::lua_gettop(state) - 1
    }
}

// Does not call lua_checkstack, uses 1 stack space.
pub unsafe fn main_state(state: *mut ffi::lua_State) -> *mut ffi::lua_State {
    ffi::lua_rawgeti(state, ffi::LUA_REGISTRYINDEX, ffi::LUA_RIDX_MAINTHREAD);
    let main_state = ffi::lua_tothread(state, -1);
    ffi::lua_pop(state, 1);
    main_state
}

// Pushes a WrappedError::Error to the top of the stack.  Uses two stack spaces and does not call
// lua_checkstack.
pub unsafe fn push_wrapped_error(state: *mut ffi::lua_State, err: Error) {
    gc_guard(state, || {
        let ud = ffi::lua_newuserdata(state, mem::size_of::<WrappedError>()) as *mut WrappedError;
        ptr::write(ud, WrappedError(err))
    });

    get_error_metatable(state);
    ffi::lua_setmetatable(state, -2);
}

// Pops a WrappedError off of the top of the stack, if it is a WrappedError.  If it is not a
// WrappedError, returns None and does not pop anything.  Uses 2 stack spaces and does not call
// lua_checkstack.
pub unsafe fn pop_wrapped_error(state: *mut ffi::lua_State) -> Option<Error> {
    if !is_wrapped_error(state, -1) {
        None
    } else {
        let err = &*get_userdata::<WrappedError>(state, -1);
        // We are assuming here that Error::clone() cannot panic.
        let err = err.0.clone();
        ffi::lua_pop(state, 1);
        Some(err)
    }
}

// Runs the given function with the Lua garbage collector disabled.  `rlua` assumes that all
// allocation failures are aborts, so when the garbage collector is disabled, 'm' functions that can
// cause either an allocation error or a a `__gc` metamethod error are prevented from causing errors
// at all.  The given function should never panic or longjmp, because this could inadverntently
// disable the gc.  This is useful when error handling must allocate, and `__gc` errors at that time
// would shadow more important errors, or be extremely difficult to handle safely.
pub unsafe fn gc_guard<R, F: FnOnce() -> R>(state: *mut ffi::lua_State, f: F) -> R {
    if ffi::lua_gc(state, ffi::LUA_GCISRUNNING, 0) != 0 {
        ffi::lua_gc(state, ffi::LUA_GCSTOP, 0);
        let r = f();
        ffi::lua_gc(state, ffi::LUA_GCRESTART, 0);
        r
    } else {
        f()
    }
}

// Initialize the error, panic, and destructed userdata metatables.
pub unsafe fn init_error_metatables(state: *mut ffi::lua_State) {
    check_stack(state, 8);

    // Create error metatable

    unsafe extern "C" fn error_tostring(state: *mut ffi::lua_State) -> c_int {
        rust_callback_error(state, || {
            check_stack_err(state, 2)?;
            if is_wrapped_error(state, -1) {
                let error = get_userdata::<WrappedError>(state, -1);
                let error_str = (*error).0.to_string();
                gc_guard(state, || {
                    ffi::lua_pushlstring(
                        state,
                        error_str.as_ptr() as *const c_char,
                        error_str.len(),
                    )
                });
                ffi::lua_remove(state, -2);

                Ok(1)
            } else {
                panic!("userdata mismatch in Error metamethod");
            }
        })
    }

    ffi::lua_pushlightuserdata(
        state,
        &ERROR_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_newtable(state);

    ffi::lua_pushstring(state, cstr!("__gc"));
    ffi::lua_pushcfunction(state, userdata_destructor::<WrappedError>);
    ffi::lua_rawset(state, -3);

    ffi::lua_pushstring(state, cstr!("__tostring"));
    safe::lua_pushrfunction(state, error_tostring).unwrap();
    ffi::lua_rawset(state, -3);

    ffi::lua_pushstring(state, cstr!("__metatable"));
    ffi::lua_pushboolean(state, 0);
    ffi::lua_rawset(state, -3);

    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);

    // Create panic metatable

    ffi::lua_pushlightuserdata(
        state,
        &PANIC_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_newtable(state);

    ffi::lua_pushstring(state, cstr!("__gc"));
    ffi::lua_pushcfunction(state, userdata_destructor::<WrappedPanic>);
    ffi::lua_rawset(state, -3);

    ffi::lua_pushstring(state, cstr!("__metatable"));
    ffi::lua_pushboolean(state, 0);
    ffi::lua_rawset(state, -3);

    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);

    // Create destructed userdata metatable

    unsafe extern "C" fn destructed_error(state: *mut ffi::lua_State) -> c_int {
        rust_callback_error(state, || Err(Error::CallbackDestructed))
    }

    ffi::lua_pushlightuserdata(
        state,
        &DESTRUCTED_USERDATA_METATABLE as *const u8 as *mut c_void,
    );
    ffi::lua_newtable(state);
    safe::lua_pushrfunction(state, destructed_error).unwrap();

    for &method in &[
        cstr!("__add"),
        cstr!("__sub"),
        cstr!("__mul"),
        cstr!("__div"),
        cstr!("__mod"),
        cstr!("__pow"),
        cstr!("__unm"),
        cstr!("__idiv"),
        cstr!("__band"),
        cstr!("__bor"),
        cstr!("__bxor"),
        cstr!("__bnot"),
        cstr!("__shl"),
        cstr!("__shr"),
        cstr!("__concat"),
        cstr!("__len"),
        cstr!("__eq"),
        cstr!("__lt"),
        cstr!("__le"),
        cstr!("__index"),
        cstr!("__newindex"),
        cstr!("__call"),
        cstr!("__tostring"),
        cstr!("__pairs"),
        cstr!("__ipairs"),
    ] {
        ffi::lua_pushstring(state, method);
        ffi::lua_pushvalue(state, -2);
        ffi::lua_rawset(state, -4);
    }

    ffi::lua_pop(state, 1);
    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);
}

struct WrappedError(pub Error);
struct WrappedPanic(pub Option<Box<Any + Send>>);

// Pushes a WrappedError::Panic to the top of the stack.  Uses two stack spaces and does not call
// lua_checkstack.
unsafe fn push_wrapped_panic(state: *mut ffi::lua_State, panic: Box<Any + Send>) {
    gc_guard(state, || {
        let ud = ffi::lua_newuserdata(state, mem::size_of::<WrappedPanic>()) as *mut WrappedPanic;
        ptr::write(ud, WrappedPanic(Some(panic)))
    });

    get_panic_metatable(state);
    ffi::lua_setmetatable(state, -2);
}

// Checks if the value at the given index is a WrappedError, uses 2 stack spaces and does not call
// lua_checkstack.
unsafe fn is_wrapped_error(state: *mut ffi::lua_State, index: c_int) -> bool {
    let userdata = ffi::lua_touserdata(state, index);
    if userdata.is_null() {
        return false;
    }

    if ffi::lua_getmetatable(state, index) == 0 {
        return false;
    }

    get_error_metatable(state);
    let res = ffi::lua_rawequal(state, -1, -2) != 0;
    ffi::lua_pop(state, 2);
    res
}

// Checks if the value at the given index is a WrappedPanic.  Uses 2 stack spaces and does not call
// lua_checkstack.
unsafe fn is_wrapped_panic(state: *mut ffi::lua_State, index: c_int) -> bool {
    let userdata = ffi::lua_touserdata(state, index);
    if userdata.is_null() {
        return false;
    }

    if ffi::lua_getmetatable(state, index) == 0 {
        return false;
    }

    get_panic_metatable(state);
    let res = ffi::lua_rawequal(state, -1, -2) != 0;
    ffi::lua_pop(state, 2);
    res
}

unsafe fn get_error_metatable(state: *mut ffi::lua_State) {
    ffi::lua_pushlightuserdata(
        state,
        &ERROR_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
}

unsafe fn get_panic_metatable(state: *mut ffi::lua_State) {
    ffi::lua_pushlightuserdata(
        state,
        &PANIC_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
}

unsafe fn get_destructed_userdata_metatable(state: *mut ffi::lua_State) {
    ffi::lua_pushlightuserdata(
        state,
        &DESTRUCTED_USERDATA_METATABLE as *const u8 as *mut c_void,
    );
    ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
}

static ERROR_METATABLE_REGISTRY_KEY: u8 = 0;
static PANIC_METATABLE_REGISTRY_KEY: u8 = 0;
static DESTRUCTED_USERDATA_METATABLE: u8 = 0;
