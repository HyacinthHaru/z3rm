// z3rm_todo 属性宏 (spec §8.1)
//
// "Fixing a hole" = "deleting the #[z3rm_todo] attribute from that code"
//
// The macro emits BOTH branches as cfg-gated tokens. The USER crate's
// feature flag (z3rm-migration in their Cargo.toml) controls which
// branch survives cfg evaluation at the use site:
//   - feature OFF (default): compile_error! fires, build blocked
//   - feature ON: inventory registration, build proceeds

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse::Parse, parse::ParseStream, LitStr, Token};

struct Z3rmTodoArgs {
    category: LitStr,
    description: Option<LitStr>,
}

impl Parse for Z3rmTodoArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let category: LitStr = input.parse()?;
        let description = if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            Some(input.parse::<LitStr>()?)
        } else {
            None
        };
        Ok(Z3rmTodoArgs { category, description })
    }
}

/// 标记迁移洞。用法:
/// ```ignore
/// #[z3rm_todo("removed-crate", "workspace 不再依赖 project::worktree")]
/// fn some_function() { ... }
/// ```
#[proc_macro_attribute]
pub fn z3rm_todo(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let args: Z3rmTodoArgs = syn::parse_macro_input!(attrs as Z3rmTodoArgs);
    let item: proc_macro2::TokenStream = item.into();
    let category = args.category.value();
    let description = args.description.map(|d| d.value()).unwrap_or_default();

    // Build the compile_error message as a single string literal.
    let blocker_msg = if description.is_empty() {
        format!(
            "z3rm migration hole ({}): delete this #[z3rm_todo] attribute to resolve",
            category
        )
    } else {
        format!(
            "z3rm migration hole ({}): {} — delete this #[z3rm_todo] attribute to resolve",
            category, description
        )
    };
    let blocker_lit = syn::LitStr::new(&blocker_msg, proc_macro2::Span::call_site());

    // Emit both branches with cfg gates. The user crate's Cargo.toml
    // z3rm-migration feature controls which one survives at compile time.
    let expanded = quote! {
        // When z3rm-migration is ON: register the hole to inventory for counting.
        #[cfg(feature = "z3rm-migration")]
        inventory::submit! {
            z3rm_macros_types::Z3rmTodo {
                category: #category,
                description: #description,
                file: file!(),
                line: line!(),
            }
        }

        // When z3rm-migration is OFF (default): block compilation.
        #[cfg(not(feature = "z3rm-migration"))]
        compile_error!(#blocker_lit);

        // The item is always emitted — the cfg gates only affect the
        // inventory/compile_error wrappers above, not the item itself.
        #item
    };

    expanded.into()
}
