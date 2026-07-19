//! # z3rm_macros 集成测试
//!
//! 验证 #[z3rm_todo] 宏正确展开,inventory 能收集到条目,
//! 且 category/description/file/line 字段被正确填充。
//!
//! 这是 spec §8.1 迁移追踪系统的最小可工作单元 (MWE)。

use z3rm_macros::z3rm_todo;
use z3rm_macros_types::Z3rmTodo;

// 放几个用宏标记的函数,验证 inventory 收集
#[z3rm_todo("removed-crate", "fake removed-crate hole for test")]
fn _hole_removed_crate() {}

#[z3rm_todo("broken-ref", "fake broken-ref hole for test")]
fn _hole_broken_ref() {}

#[z3rm_todo("stub")]
fn _hole_stub_no_desc() {}

#[test]
fn macro_registers_entries_in_inventory() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().collect();
    let our_holes: Vec<&Z3rmTodo> = all
        .iter()
        .filter(|h| h.description.contains("hole for test") || h.file.ends_with("macro_test.rs"))
        .copied()
        .collect();

    assert!(
        our_holes.len() >= 3,
        "expected at least 3 holes from this test file, got {}",
        our_holes.len()
    );
}

#[test]
fn macro_preserves_category_string() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().collect();
    let cats: Vec<&str> = all.iter().map(|h| h.category).collect();
    assert!(cats.contains(&"removed-crate"), "categories: {:?}", cats);
    assert!(cats.contains(&"broken-ref"), "categories: {:?}", cats);
    assert!(cats.contains(&"stub"), "categories: {:?}", cats);
}

#[test]
fn macro_fills_file_and_line() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().collect();
    let ours: Vec<&Z3rmTodo> = all
        .iter()
        .filter(|h| h.file.ends_with("macro_test.rs"))
        .copied()
        .collect();
    assert!(!ours.is_empty(), "file field should point to this test");
    for h in &ours {
        assert!(h.line > 0, "line should be > 0, got {}", h.line);
    }
}

#[test]
fn macro_description_optional() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().collect();
    let no_desc = all
        .iter()
        .find(|h| h.category == "stub" && h.description.is_empty());
    assert!(
        no_desc.is_some(),
        "z3rm_todo(\"stub\") without description should produce empty description string"
    );
}

// 受标记的函数本身应仍可被调用 (宏不破坏原 item)
#[test]
fn macro_preserves_item() {
    // 如果宏破坏了 item,这一行会编译失败
    _hole_removed_crate();
    _hole_broken_ref();
    _hole_stub_no_desc();
}

#[test]
fn debug_show_all() {
    for h in inventory::iter::<Z3rmTodo> {
        eprintln!("DEBUG cat={} desc='{}' file={} line={}", h.category, h.description, h.file, h.line);
    }
}
