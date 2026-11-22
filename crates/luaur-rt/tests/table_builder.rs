//! `Lua::build_table` and `TableBuilder`: tables filled on the stack, and the
//! direct string pushes `Table::raw_set` shares with them.

use luaur_rt::{Error, Lua, LuaString, Result, Table, Value};

#[test]
fn builds_keys_values_and_nested_tables() -> Result<()> {
    let lua = Lua::new();
    let table = lua.build_table(0, 4, |t| {
        t.set("name", "crate")?;
        t.set("count", 3)?;
        t.set(String::from("owned"), String::from("value"))?;
        t.set_table("position", 0, 3, |p| {
            p.set("x", 1.5)?;
            p.set("y", -2.0)?;
            p.set("z", 0.25)
        })
    })?;
    assert_eq!(table.get::<String>("name")?, "crate");
    assert_eq!(table.get::<i64>("count")?, 3);
    assert_eq!(table.get::<String>("owned")?, "value");
    let position: Table = table.get("position")?;
    assert_eq!(position.get::<f64>("x")?, 1.5);
    assert_eq!(position.get::<f64>("y")?, -2.0);
    assert_eq!(position.get::<f64>("z")?, 0.25);
    Ok(())
}

#[test]
fn appends_a_sequence() -> Result<()> {
    let lua = Lua::new();
    let table = lua.build_table(3, 0, |t| {
        t.push("first")?;
        t.push_table(0, 1, |item| item.set("id", 2))?;
        t.push(3.5)
    })?;
    assert_eq!(table.raw_len(), 3);
    assert_eq!(table.get::<String>(1)?, "first");
    assert_eq!(table.get::<Table>(2)?.get::<i64>("id")?, 2);
    assert_eq!(table.get::<f64>(3)?, 3.5);
    Ok(())
}

#[test]
fn a_read_only_table_takes_no_more_values() -> Result<()> {
    let lua = Lua::new();
    let table = lua.build_table(0, 1, |t| {
        t.set("kept", true)?;
        t.set_readonly(true);
        assert!(t.set("refused", 1).is_err());
        assert!(t.push(1).is_err());
        assert!(t.set_table("child", 0, 0, |_| Ok(())).is_err());
        Ok(())
    })?;
    assert!(table.is_readonly());
    assert_eq!(table.get::<Value>("refused")?, Value::Nil);
    lua.globals().set("frozen", table)?;
    assert!(lua.load("frozen.kept = false").exec().is_err());
    Ok(())
}

#[test]
fn a_failed_build_leaves_the_stack_as_it_was() -> Result<()> {
    let lua = Lua::new();
    // A failed build that left its table, key or half-built child behind
    // would run the stack out well before this many attempts.
    for _ in 0..20_000 {
        let result = lua.build_table(0, 2, |t| {
            t.set("a", 1)?;
            t.set_table("child", 0, 1, |child| {
                child.set("b", 2)?;
                Err(Error::RuntimeError("stop".into()))
            })
        });
        assert!(result.is_err());
    }
    let table = lua.build_table(0, 1, |t| t.set("after", "ok"))?;
    assert_eq!(table.get::<String>("after")?, "ok");
    Ok(())
}

#[test]
fn from_iterator_tables_keep_their_contents() -> Result<()> {
    let lua = Lua::new();
    let map = lua.create_table_from([("a", 1), ("b", 2)])?;
    assert_eq!(map.get::<i64>("a")?, 1);
    assert_eq!(map.get::<i64>("b")?, 2);
    let sequence = lua.create_sequence_from(["x", "y", "z"])?;
    assert_eq!(sequence.raw_len(), 3);
    assert_eq!(sequence.get::<String>(3)?, "z");
    Ok(())
}

#[test]
fn raw_set_pushes_rust_strings_directly() -> Result<()> {
    let lua = Lua::new();
    let table = lua.create_table();
    table.raw_set("key", "value")?;
    table.raw_set(String::from("owned"), String::from("owned value"))?;
    table.raw_set("", "empty key")?;
    table.raw_set("nul\0inside", "nul\0value")?;
    assert_eq!(table.raw_get::<String>("key")?, "value");
    assert_eq!(table.raw_get::<String>("owned")?, "owned value");
    assert_eq!(table.raw_get::<String>("")?, "empty key");
    assert_eq!(
        table.raw_get::<LuaString>("nul\0inside")?.as_bytes(),
        b"nul\0value"
    );
    table.set_readonly(true);
    assert!(table.raw_set("key", "again").is_err());
    assert_eq!(table.raw_get::<String>("key")?, "value");
    Ok(())
}

#[test]
fn capacity_is_room_not_content() -> Result<()> {
    let lua = Lua::new();
    let table = lua.create_table_with_capacity(16, 16)?;
    assert_eq!(table.raw_len(), 0);
    assert!(table.pairs::<Value, Value>().next().is_none());
    Ok(())
}
