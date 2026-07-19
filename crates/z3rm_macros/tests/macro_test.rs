//! # z3rm_macros integration tests
//!
//! Tests must run WITH `--features z3rm-migration` because the macro
//! emits compile_error! without that feature. Run with:
//!   cargo test -p z3rm_macros --features z3rm-migration

#![cfg(feature = "z3rm-migration")]

use z3rm_macros::z3rm_todo;
use z3rm_macros_types::Z3rmTodo;

#[z3rm_todo("removed-crate", "fake removed-crate hole for test")]
fn _hole_removed_crate() {}

#[z3rm_todo("broken-ref", "fake broken-ref hole for test")]
fn _hole_broken_ref() {}

#[z3rm_todo("stub")]
fn _hole_stub_no_desc() {}

#[test]
fn macro_registers_entries_in_inventory() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().into_iter().collect();
    let ours: Vec<&Z3rmTodo> = all
        .iter()
        .filter(|h| h.description.contains("hole for test") || h.file.ends_with("macro_test.rs"))
        .copied()
        .collect();
    assert!(ours.len() >= 3, "expected >=3 holes, got {}", ours.len());
}

#[test]
fn macro_preserves_category_string() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().into_iter().collect();
    let cats: Vec<&str> = all.iter().map(|h| h.category).collect();
    assert!(cats.contains(&"removed-crate"));
    assert!(cats.contains(&"broken-ref"));
    assert!(cats.contains(&"stub"));
}

#[test]
fn macro_fills_file_and_line() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().into_iter().collect();
    let ours: Vec<&Z3rmTodo> = all
        .iter()
        .filter(|h| h.file.ends_with("macro_test.rs"))
        .copied()
        .collect();
    assert!(!ours.is_empty());
    for h in &ours {
        assert!(h.line > 0);
    }
}

#[test]
fn macro_description_optional() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().into_iter().collect();
    let no_desc = all.iter().find(|h| h.category == "stub" && h.description.is_empty());
    assert!(no_desc.is_some());
}

#[test]
fn macro_preserves_item() {
    _hole_removed_crate();
    _hole_broken_ref();
    _hole_stub_no_desc();
}
