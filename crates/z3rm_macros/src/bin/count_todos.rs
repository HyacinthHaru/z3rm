//! # count_todos
//!
//! §8.1 Plan 1 deliverable: links all z3rm crates and prints the inventory
//! count of remaining `#[z3rm_todo]` migration holes, grouped by category.
//!
//! Usage: `cargo run -p z3rm_macros --bin count_todos --features z3rm-migration`
//!
//! Must be compiled WITH `z3rm-migration` feature so that holes register
//! to inventory instead of blocking compilation.

use z3rm_macros_types::Z3rmTodo;

fn main() {
    let all: Vec<&Z3rmTodo> = inventory::iter::<Z3rmTodo>().into_iter().collect();

    if all.is_empty() {
        println!("z3rm migration: 0 holes remaining — migration complete!");
        return;
    }

    // Group by category
    let mut by_category: std::collections::BTreeMap<&str, Vec<&Z3rmTodo>> =
        std::collections::BTreeMap::new();
    for h in all.iter() {
        by_category.entry(h.category).or_default().push(h);
    }

    println!("z3rm migration: {} holes remaining", all.len());
    println!();
    for (cat, holes) in &by_category {
        println!("  {} ({}):", cat, holes.len());
        for h in holes.iter() {
            println!("    {}:{} — {}", h.file, h.line, h.description);
        }
    }
}
