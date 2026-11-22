//! The [`Table`] handle. Mirrors `mlua::Table`.

use crate::error::Result;
use crate::state::{Lua, LuaRef};
use crate::sync::{NotSync, XRc, NOT_SYNC};
use crate::sys::*;
use crate::traits::{FromLua, IntoLua};
use crate::value::Value;

/// A handle to a Lua table.
///
/// Mirrors `mlua::Table`. Holds a registry reference keeping the table alive.
///
/// Under the `send` feature this handle is `Send` (the VM can be moved across
/// threads) but never `Sync` — see [`crate::sync::NotSync`].
#[derive(Clone)]
pub struct Table {
    pub(crate) reference: XRc<LuaRef>,
    pub(crate) _not_sync: NotSync,
}

/// Reserve `slots` stack slots for a raw table operation.
///
/// `lua_checkstack` raises the stack top inside the current call-info frame;
/// without it, `lua_pushvalue` (and the extra copy `value_from_stack` pushes
/// for reference types) can trip the `top < ci.top` LUAU_ASSERT and abort the
/// process when the caller's stack margin is already thin (for example after
/// a long sequence of VM operations in the same frame).
pub(crate) fn ensure_stack(state: *mut lua_State, slots: c_int) -> Result<()> {
    if unsafe { lua_checkstack(state, slots) } == 0 {
        // Same shape as the guards in `Function::call` / `Thread::resume`:
        // a catchable `RuntimeError` rather than a VM abort.
        return Err(crate::error::Error::RuntimeError(
            "stack overflow: not enough stack space for a raw table operation".to_string(),
        ));
    }
    Ok(())
}

impl Table {
    pub(crate) fn from_ref(reference: LuaRef) -> Table {
        Table {
            reference: XRc::new(reference),
            _not_sync: NOT_SYNC,
        }
    }

    pub(crate) unsafe fn push_to_stack(&self) {
        self.reference.push();
    }

    /// The owning [`Lua`].
    pub fn lua(&self) -> Lua {
        self.reference.lua()
    }

    /// Set `table[key] = value`, honoring metamethods (`__newindex`).
    ///
    /// Mirrors `mlua::Table::set`. Errors (`RuntimeError`) propagate from a
    /// `__newindex` metamethod that raises.
    pub fn set<K: IntoLua, V: IntoLua>(&self, key: K, value: V) -> Result<()> {
        let lua = self.lua();
        let state = lua.state();
        // The table, key and value, then the settable closure below them.
        ensure_stack(state, 4)?;
        // Drive the (possibly metamethod-invoking) settable under pcall so a
        // raising `__newindex` (or readonly table) surfaces as `Err`.
        unsafe {
            let top = lua_gettop(state);
            self.reference.push(); // table
            if let Err(error) = push_pair(&lua, key, value) {
                lua_settop(state, top);
                return Err(error);
            }
            let status = protected_settable(state);
            if status != 0 {
                return Err(lua.pop_error(status));
            }
        }
        Ok(())
    }

    /// Get `table[key]`, honoring metamethods (`__index`), converting the
    /// result to `V`.
    ///
    /// Mirrors `mlua::Table::get`. The value type is the sole explicit type
    /// parameter (key type is inferred), matching mlua's `get::<V>(key)`.
    pub fn get<V: FromLua>(&self, key: impl IntoLua) -> Result<V> {
        let lua = self.lua();
        let state = lua.state();
        // The table and key, then the gettable closure below them.
        ensure_stack(state, 3)?;
        let value = unsafe {
            let top = lua_gettop(state);
            self.reference.push(); // table
            if let Err(error) = key.push_into_stack(&lua) {
                lua_settop(state, top);
                return Err(error);
            }
            let status = protected_gettable(state);
            if status != 0 {
                return Err(lua.pop_error(status));
            }
            let v = lua.value_from_stack(-1)?;
            lua_pop(state, 1); // pop the result value
            v
        };
        V::from_lua(value, &lua)
    }

    /// Whether `table[key]` is non-nil.
    ///
    /// Mirrors `mlua::Table::contains_key`.
    pub fn contains_key<K: IntoLua>(&self, key: K) -> Result<bool> {
        let v: Value = self.get(key)?;
        Ok(!v.is_nil())
    }

    /// The border length (`#table`).
    ///
    /// Mirrors `mlua::Table::raw_len` (returns `usize`). luaur's `lua_objlen`
    /// gives the same border-length semantics as `lua_rawlen`.
    pub fn raw_len(&self) -> usize {
        let state = self.reference.state();
        unsafe {
            self.reference.push();
            let n = lua_objlen(state, -1);
            lua_pop(state, 1);
            n.max(0) as usize
        }
    }

    /// The length (`#table`), honoring a `__len` metamethod.
    ///
    /// Mirrors `mlua::Table::len`. Returns `Err` if a `__len` metamethod
    /// raises. Without a `__len` metamethod this is the raw border length.
    pub fn len(&self) -> Result<usize> {
        let lua = self.lua();
        // Fast path: no metatable -> raw border length (no metamethod possible).
        if self.metatable().is_none() {
            return Ok(self.raw_len());
        }
        // Evaluate `#self` protected so a raising/returning `__len` is honored.
        let f = lua.load("local t = ...; return #t").into_function()?;
        let n: i64 = f.call(self.clone())?;
        Ok(n.max(0) as usize)
    }

    /// Whether the table is empty, without invoking metamethods.
    ///
    /// Mirrors `mlua::Table::is_empty`: checks **both** the array part and the
    /// hash part (a table with only string keys is *not* empty). Uses a single
    /// `lua_next` probe — present iff the table has at least one key.
    pub fn is_empty(&self) -> bool {
        let state = self.reference.state();
        unsafe {
            self.reference.push(); // table
            lua_pushnil(state); // first key
            if lua_next(state, -2) == 0 {
                // No first key: table is empty. `lua_next` already popped the key.
                lua_pop(state, 1); // pop table
                return true;
            }
            // stack: table, key, value — pop value+key+table.
            lua_pop(state, 3);
            false
        }
    }

    /// Iterate over `(key, value)` pairs.
    ///
    /// Mirrors `mlua::Table::pairs`. Returns an iterator yielding `Result<(K,
    /// V)>` items. Uses `lua_next` under the hood.
    pub fn pairs<K: FromLua, V: FromLua>(&self) -> TablePairs<K, V> {
        TablePairs {
            table: self.clone(),
            next_key: Some(Value::Nil),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Collect all `(key, value)` pairs into a `Vec`. Convenience over
    /// [`Table::pairs`].
    pub fn pairs_vec<K: FromLua, V: FromLua>(&self) -> Result<Vec<(K, V)>> {
        self.pairs().collect()
    }

    /// Iterate over the sequence part `[1..]`, stopping at the first `nil`
    /// (raw access — ignores `__index`). Mirrors `mlua::Table::sequence_values`.
    pub fn sequence_values<V: FromLua>(&self) -> TableSequence<V> {
        TableSequence {
            table: self.clone(),
            index: 1,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Call `f` for each `(key, value)` pair (raw `lua_next` traversal).
    /// Stops early on the first `Err`. Mirrors `mlua::Table::for_each`.
    ///
    /// The key/value types are the only explicit type parameters (the closure
    /// type is inferred), matching mlua's `for_each::<K, V>(f)`.
    pub fn for_each<K: FromLua, V: FromLua>(
        &self,
        mut f: impl FnMut(K, V) -> Result<()>,
    ) -> Result<()> {
        for pair in self.pairs::<K, V>() {
            let (k, v) = pair?;
            f(k, v)?;
        }
        Ok(())
    }

    /// Call `f` for each value in the sequence part. Mirrors
    /// `mlua::Table::for_each_value`.
    pub fn for_each_value<V: FromLua>(&self, mut f: impl FnMut(V) -> Result<()>) -> Result<()> {
        for v in self.sequence_values::<V>() {
            f(v?)?;
        }
        Ok(())
    }

    // --- raw (metamethod-bypassing) access ---------------------------------

    /// Set `table[key] = value` without invoking `__newindex`.
    ///
    /// Mirrors `mlua::Table::raw_set`. Errors if the table is readonly.
    pub fn raw_set<K: IntoLua, V: IntoLua>(&self, key: K, value: V) -> Result<()> {
        let lua = self.lua();
        let state = lua.state();
        ensure_stack(state, 3)?;
        unsafe {
            let top = lua_gettop(state);
            self.reference.push(); // table
            if let Err(error) = push_pair(&lua, key, value) {
                lua_settop(state, top);
                return Err(error);
            }
            // Read off the table already pushed, after the conversions, so a
            // conversion error still comes first.
            if lua_getreadonly(state, -3) != 0 {
                lua_settop(state, top);
                return Err(readonly_error());
            }
            lua_rawset(state, -3);
            lua_pop(state, 1); // pop table
        }
        Ok(())
    }

    /// Get `table[key]` without invoking `__index`.
    ///
    /// Mirrors `mlua::Table::raw_get`.
    pub fn raw_get<V: FromLua>(&self, key: impl IntoLua) -> Result<V> {
        let lua = self.lua();
        let state = lua.state();
        ensure_stack(state, 3)?;
        let value = unsafe {
            let top = lua_gettop(state);
            self.reference.push(); // table
            if let Err(error) = key.push_into_stack(&lua) {
                lua_settop(state, top);
                return Err(error);
            }
            lua_rawget(state, -2); // replaces key with value
            let v = lua.value_from_stack(-1)?;
            lua_pop(state, 2); // pop value + table
            v
        };
        V::from_lua(value, &lua)
    }

    /// Append `value` at position `#table + 1` using raw access.
    ///
    /// Mirrors `mlua::Table::raw_push`. Errors if readonly.
    pub fn raw_push<V: IntoLua>(&self, value: V) -> Result<()> {
        let n = self.raw_len();
        self.raw_set((n + 1) as i64, value)
    }

    /// Remove and return the last sequence element via raw access.
    ///
    /// Mirrors `mlua::Table::raw_pop`. Errors if readonly.
    pub fn raw_pop<V: FromLua>(&self) -> Result<V> {
        let lua = self.lua();
        let n = self.raw_len();
        if n == 0 {
            return V::from_lua(Value::Nil, &lua);
        }
        if self.is_readonly() {
            return Err(crate::error::Error::RuntimeError(
                "attempt to modify a readonly table".to_string(),
            ));
        }
        let v: V = self.raw_get(n as i64)?;
        self.raw_set(n as i64, Value::Nil)?;
        Ok(v)
    }

    /// Insert `value` at 1-based `idx`, shifting later elements up (raw).
    ///
    /// Mirrors `mlua::Table::raw_insert`. Errors on bad index or readonly.
    pub fn raw_insert<V: IntoLua>(&self, idx: i64, value: V) -> Result<()> {
        let n = self.raw_len() as i64;
        if idx < 1 || idx > n + 1 {
            return Err(crate::error::Error::RuntimeError(format!(
                "bad argument #2 to 'insert' (position out of bounds): {idx}"
            )));
        }
        if self.is_readonly() {
            return Err(crate::error::Error::RuntimeError(
                "attempt to modify a readonly table".to_string(),
            ));
        }
        // Shift [idx..=n] up by one, then place the new value.
        let mut i = n;
        while i >= idx {
            let moved: Value = self.raw_get(i)?;
            self.raw_set(i + 1, moved)?;
            i -= 1;
        }
        self.raw_set(idx, value.into_lua(&self.lua())?)
    }

    /// Remove and return the element at 1-based `idx`, shifting later
    /// elements down (raw). Mirrors `mlua::Table::raw_remove`.
    pub fn raw_remove(&self, idx: i64) -> Result<Value> {
        let n = self.raw_len() as i64;
        if n == 0 {
            return Ok(Value::Nil);
        }
        if idx < 1 || idx > n {
            return Err(crate::error::Error::RuntimeError(format!(
                "bad argument #1 to 'remove' (position out of bounds): {idx}"
            )));
        }
        if self.is_readonly() {
            return Err(crate::error::Error::RuntimeError(
                "attempt to modify a readonly table".to_string(),
            ));
        }
        let removed: Value = self.raw_get(idx)?;
        let mut i = idx;
        while i < n {
            let moved: Value = self.raw_get(i + 1)?;
            self.raw_set(i, moved)?;
            i += 1;
        }
        self.raw_set(n, Value::Nil)?;
        Ok(removed)
    }

    /// Append `value` honoring `__len`/`__newindex` (uses `#self + 1`).
    /// Mirrors `mlua::Table::push`.
    pub fn push<V: IntoLua>(&self, value: V) -> Result<()> {
        let n = self.len()?;
        self.set((n + 1) as i64, value)
    }

    /// Pop the last element honoring `__len`/`__index`/`__newindex`.
    /// Mirrors `mlua::Table::pop`.
    pub fn pop<V: FromLua>(&self) -> Result<V> {
        let lua = self.lua();
        let n = self.len()?;
        if n == 0 {
            return V::from_lua(Value::Nil, &lua);
        }
        let v: V = self.get(n as i64)?;
        self.set(n as i64, Value::Nil)?;
        Ok(v)
    }

    /// Remove all keys from the table (raw). Errors if readonly.
    /// Mirrors `mlua::Table::clear`.
    pub fn clear(&self) -> Result<()> {
        if self.is_readonly() {
            return Err(crate::error::Error::RuntimeError(
                "attempt to modify a readonly table".to_string(),
            ));
        }
        // Collect every key (raw traversal), then nil them out.
        let lua = self.lua();
        let state = lua.state();
        let mut keys: Vec<Value> = Vec::new();
        unsafe {
            self.reference.push(); // table
            lua_pushnil(state); // first key
            while lua_next(state, -2) != 0 {
                // stack: table, key, value
                let k = lua.value_from_stack(-2)?;
                keys.push(k);
                lua_pop(state, 1); // pop value, keep key for next iteration
            }
            lua_pop(state, 1); // pop table
        }
        for k in keys {
            self.raw_set(k, Value::Nil)?;
        }
        Ok(())
    }

    // --- identity / equality / metatables ----------------------------------

    /// A raw pointer identifying this table (for identity comparison).
    /// Mirrors `mlua::Table::to_pointer`.
    pub fn to_pointer(&self) -> *const std::ffi::c_void {
        let state = self.reference.state();
        unsafe {
            self.reference.push();
            let p = lua_topointer(state, -1);
            lua_pop(state, 1);
            p
        }
    }

    /// Compare for equality honoring an `__eq` metamethod.
    /// Mirrors `mlua::Table::equals`.
    pub fn equals(&self, other: &Table) -> Result<bool> {
        let lua = self.lua();
        let state = lua.state();
        unsafe {
            self.reference.push();
            other.reference.push();
            let eq = lua_equal(state, -2, -1);
            lua_pop(state, 2);
            Ok(eq != 0)
        }
    }

    /// The table's metatable, if any. Mirrors `mlua::Table::metatable`.
    pub fn metatable(&self) -> Option<Table> {
        let lua = self.lua();
        let state = lua.state();
        unsafe {
            self.reference.push();
            let has = lua_getmetatable(state, -1);
            if has == 0 {
                lua_pop(state, 1); // pop table
                return None;
            }
            // stack: table, metatable
            let mt = Table::from_ref(lua.pop_ref());
            lua_pop(state, 1); // pop table
            Some(mt)
        }
    }

    /// Set (or clear, with `None`) the table's metatable.
    /// Mirrors `mlua::Table::set_metatable`. Errors if the table is readonly.
    pub fn set_metatable(&self, metatable: Option<Table>) -> Result<()> {
        if self.is_readonly() {
            return Err(crate::error::Error::RuntimeError(
                "attempt to modify a readonly table".to_string(),
            ));
        }
        let lua = self.lua();
        let state = lua.state();
        unsafe {
            self.reference.push(); // table
            match metatable {
                Some(mt) => mt.push_to_stack(),
                None => lua_pushnil(state),
            }
            lua_setmetatable(state, -2);
            lua_pop(state, 1); // pop table
        }
        Ok(())
    }

    // --- readonly (Luau extension) -----------------------------------------

    /// Whether the table is marked readonly (Luau). Mirrors
    /// `mlua::Table::is_readonly`.
    pub fn is_readonly(&self) -> bool {
        let state = self.reference.state();
        unsafe {
            self.reference.push();
            let ro = lua_getreadonly(state, -1);
            lua_pop(state, 1);
            ro != 0
        }
    }

    /// Mark the table readonly or writable (Luau). Mirrors
    /// `mlua::Table::set_readonly`.
    pub fn set_readonly(&self, enabled: bool) {
        let state = self.reference.state();
        unsafe {
            self.reference.push();
            lua_setreadonly(state, -1, enabled as c_int);
            lua_pop(state, 1);
        }
    }
}

/// Iterator over a table's sequence part (see [`Table::sequence_values`]).
pub struct TableSequence<V> {
    table: Table,
    index: i64,
    _phantom: std::marker::PhantomData<V>,
}

impl<V: FromLua> Iterator for TableSequence<V> {
    type Item = Result<V>;

    fn next(&mut self) -> Option<Self::Item> {
        let lua = self.table.lua();
        // Raw get of the next sequence slot; stop at the first nil.
        let value: Value = match self.table.raw_get(self.index) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        if value.is_nil() {
            return None;
        }
        self.index += 1;
        Some(V::from_lua(value, &lua))
    }
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Table(len={})", self.raw_len())
    }
}

impl PartialEq for Table {
    fn eq(&self, other: &Self) -> bool {
        // Reference (pointer) identity, matching mlua's `==` on handles: two
        // handles are equal iff they point at the *same* Lua table object.
        self.to_pointer() == other.to_pointer()
    }
}

/// Compare a [`Table`]'s sequence part to a Rust slice of values.
///
/// Mirrors mlua's `impl PartialEq<[T]> for Table`: equal when the table's
/// `1..=len` sequence (read raw) matches `other` element-wise and has the
/// same length.
impl<T> PartialEq<[T]> for Table
where
    T: FromLua + PartialEq + Clone,
{
    fn eq(&self, other: &[T]) -> bool {
        // Compare the *sequence* part (stopping at the first nil border),
        // matching mlua's `sequence_values`-based slice comparison. This is the
        // robust border semantics for tables with nil holes (e.g.
        // `{1, 2, nil, 4, 5}` compares equal to `[1, 2]`).
        let mut iter = self.sequence_values::<T>();
        for expected in other.iter() {
            match iter.next() {
                Some(Ok(got)) if &got == expected => {}
                _ => return false,
            }
        }
        // The sequence must be exactly the slice length (no extra elements).
        iter.next().is_none()
    }
}

impl<T, const N: usize> PartialEq<[T; N]> for Table
where
    T: FromLua + PartialEq + Clone,
{
    fn eq(&self, other: &[T; N]) -> bool {
        self == other.as_slice()
    }
}

impl<T> PartialEq<&[T]> for Table
where
    T: FromLua + PartialEq + Clone,
{
    fn eq(&self, other: &&[T]) -> bool {
        self == *other
    }
}

/// Iterator over a table's key/value pairs (see [`Table::pairs`]).
pub struct TablePairs<K, V> {
    table: Table,
    next_key: Option<Value>,
    _phantom: std::marker::PhantomData<(K, V)>,
}

impl<K: FromLua, V: FromLua> Iterator for TablePairs<K, V> {
    type Item = Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.next_key.take()?;
        let lua = self.table.lua();
        let state = lua.state();
        unsafe {
            self.table.reference.push(); // [.. table]
            if lua.push_value(&key).is_err() {
                lua_pop(state, 1);
                return None;
            }
            // stack: [table, key]
            let has = lua_next(state, -2);
            if has == 0 {
                // lua_next popped the key; pop the table.
                lua_pop(state, 1);
                self.next_key = None;
                return None;
            }
            // stack: [table, next_key, value]
            let k_val = match lua.value_from_stack(-2) {
                Ok(v) => v,
                Err(e) => {
                    lua_pop(state, 3);
                    return Some(Err(e));
                }
            };
            let v_val = match lua.value_from_stack(-1) {
                Ok(v) => v,
                Err(e) => {
                    lua_pop(state, 3);
                    return Some(Err(e));
                }
            };
            // Remember the key for the next iteration, then clean the stack.
            self.next_key = Some(k_val.clone());
            lua_pop(state, 3); // value, next_key, table

            let k = match K::from_lua(k_val, &lua) {
                Ok(k) => k,
                Err(e) => return Some(Err(e)),
            };
            let v = match V::from_lua(v_val, &lua) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            Some(Ok((k, v)))
        }
    }
}

/// Create a fresh empty table on `lua` and return a handle.
pub(crate) fn create_table(lua: &Lua) -> Table {
    let state = lua.state();
    unsafe {
        lua_createtable(state, 0, 0);
        Table::from_ref(lua.pop_ref())
    }
}

/// Push `key` then `value`. On `Err` what was pushed stays; the caller
/// restores the stack top.
unsafe fn push_pair(lua: &Lua, key: impl IntoLua, value: impl IntoLua) -> Result<()> {
    unsafe {
        key.push_into_stack(lua)?;
        value.push_into_stack(lua)
    }
}

fn readonly_error() -> crate::error::Error {
    crate::error::Error::RuntimeError("attempt to modify a readonly table".to_string())
}

// ---------------------------------------------------------------------------
// Building a table on the stack
// ---------------------------------------------------------------------------

/// A table being filled on the Lua stack, from [`Lua::build_table`].
///
/// The table takes its registry reference once, when it is done, rather than
/// once for each value set into it, and so do the tables built inside it with
/// [`TableBuilder::set_table`] and [`TableBuilder::push_table`]; Rust strings
/// set as keys or values push their bytes without a reference of their own.
/// Values are set raw, bypassing metamethods.
///
/// A luaur extension; mlua has no counterpart.
pub struct TableBuilder<'lua> {
    lua: &'lua Lua,
    /// Absolute stack index of the table, which stays put while it is filled:
    /// every builder method leaves the stack as it found it.
    index: c_int,
    next: i64,
    readonly: bool,
}

impl TableBuilder<'_> {
    /// Set `table[key] = value`, bypassing metamethods.
    ///
    /// Errors if the table was made read-only with
    /// [`TableBuilder::set_readonly`].
    pub fn set(&mut self, key: impl IntoLua, value: impl IntoLua) -> Result<()> {
        self.writable()?;
        let state = self.lua.state();
        ensure_stack(state, 2)?;
        unsafe {
            let top = lua_gettop(state);
            if let Err(error) = push_pair(self.lua, key, value) {
                lua_settop(state, top);
                return Err(error);
            }
            lua_rawset(state, self.index);
        }
        Ok(())
    }

    /// Set `table[key]` to a new table with room for `narr` sequence and
    /// `nrec` other entries, which `build` fills.
    pub fn set_table<F>(
        &mut self,
        key: impl IntoLua,
        narr: usize,
        nrec: usize,
        build: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut TableBuilder<'_>) -> Result<()>,
    {
        self.writable()?;
        let state = self.lua.state();
        ensure_stack(state, 1)?;
        unsafe {
            let top = lua_gettop(state);
            let pushed = key
                .push_into_stack(self.lua)
                .and_then(|()| fill_new_table(self.lua, narr, nrec, build));
            if let Err(error) = pushed {
                lua_settop(state, top);
                return Err(error);
            }
            lua_rawset(state, self.index);
        }
        Ok(())
    }

    /// Append `value` after the values appended before: the first call sets
    /// `table[1]`, the next `table[2]`, and so on.
    pub fn push(&mut self, value: impl IntoLua) -> Result<()> {
        self.writable()?;
        let n = self.next_index()?;
        let state = self.lua.state();
        ensure_stack(state, 1)?;
        unsafe {
            let top = lua_gettop(state);
            if let Err(error) = value.push_into_stack(self.lua) {
                lua_settop(state, top);
                return Err(error);
            }
            lua_rawseti(state, self.index, n);
        }
        self.next += 1;
        Ok(())
    }

    /// Append a new table with room for `narr` sequence and `nrec` other
    /// entries, which `build` fills, the way [`TableBuilder::push`] appends.
    pub fn push_table<F>(&mut self, narr: usize, nrec: usize, build: F) -> Result<()>
    where
        F: FnOnce(&mut TableBuilder<'_>) -> Result<()>,
    {
        self.writable()?;
        let n = self.next_index()?;
        let state = self.lua.state();
        unsafe {
            let top = lua_gettop(state);
            if let Err(error) = fill_new_table(self.lua, narr, nrec, build) {
                lua_settop(state, top);
                return Err(error);
            }
            lua_rawseti(state, self.index, n);
        }
        self.next += 1;
        Ok(())
    }

    /// Make the table read-only, or writable again, as
    /// [`Table::set_readonly`] does. A read-only table takes no more values
    /// through the builder.
    pub fn set_readonly(&mut self, enabled: bool) {
        unsafe { lua_setreadonly(self.lua.state(), self.index, enabled as c_int) };
        self.readonly = enabled;
    }

    // `lua_rawset` raises on a read-only table, and nothing here is
    // protected, so the refusal has to come first.
    fn writable(&self) -> Result<()> {
        if self.readonly {
            return Err(readonly_error());
        }
        Ok(())
    }

    fn next_index(&self) -> Result<c_int> {
        c_int::try_from(self.next).map_err(|_| {
            crate::error::Error::RuntimeError("the table's sequence is full".to_string())
        })
    }
}

/// Push a new table with room for `narr` sequence and `nrec` other entries
/// and fill it with `build`, leaving it on top of the stack. On `Err` what was
/// pushed stays; the caller restores the stack top.
pub(crate) unsafe fn fill_new_table<F>(lua: &Lua, narr: usize, nrec: usize, build: F) -> Result<()>
where
    F: FnOnce(&mut TableBuilder<'_>) -> Result<()>,
{
    let state = lua.state();
    ensure_stack(state, 1)?;
    unsafe {
        lua_createtable(state, capacity(narr), capacity(nrec));
        let index = lua_gettop(state);
        let mut builder = TableBuilder {
            lua,
            index,
            next: 1,
            readonly: false,
        };
        build(&mut builder)?;
        lua_settop(state, index);
    }
    Ok(())
}

fn capacity(entries: usize) -> c_int {
    c_int::try_from(entries).unwrap_or(c_int::MAX)
}

// ---------------------------------------------------------------------------
// Protected indexing
//
// `lua_gettable`/`lua_settable` may invoke `__index`/`__newindex` metamethods
// that *raise* (longjmp). Calling them unprotected across the Rust/VM boundary
// would unwind past Rust frames. We therefore run them inside `lua_pcall` via a
// tiny C trampoline, so a raising metamethod (or a readonly-table write) is
// reported as an ordinary non-zero status with the error object on the stack.
// ---------------------------------------------------------------------------

/// C trampoline: stack is `[table, key]`; performs `lua_gettable` and leaves
/// the result on top.
unsafe fn c_gettable(state: *mut lua_State) -> c_int {
    unsafe {
        lua_gettable(state, 1);
        1
    }
}

/// C trampoline: stack is `[table, key, value]`; performs `lua_settable`.
unsafe fn c_settable(state: *mut lua_State) -> c_int {
    unsafe {
        lua_settable(state, 1);
        0
    }
}

/// Run `lua_gettable` protected. Expects `[table, key]` on top; on success
/// leaves `[result]` where the two inputs were; on failure leaves the error
/// object on top and returns the non-zero status.
unsafe fn protected_gettable(state: *mut lua_State) -> c_int {
    unsafe {
        // Insert the C function below the two arguments: [t, k] -> [f, t, k].
        lua_pushcclosurek(
            state,
            Some(c_gettable),
            c"luaur-rt-gettable".as_ptr(),
            0,
            None,
        );
        lua_insert(state, -3);
        lua_pcall(state, 2, 1, 0)
    }
}

/// Run `lua_settable` protected. Expects `[table, key, value]` on top; pops
/// them on success; on failure leaves the error object and returns the status.
unsafe fn protected_settable(state: *mut lua_State) -> c_int {
    unsafe {
        // [t, k, v] -> [f, t, k, v].
        lua_pushcclosurek(
            state,
            Some(c_settable),
            c"luaur-rt-settable".as_ptr(),
            0,
            None,
        );
        lua_insert(state, -4);
        lua_pcall(state, 3, 0, 0)
    }
}
