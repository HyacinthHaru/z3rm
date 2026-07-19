// z3rm_todo 属性宏
// 来源: spec §8.1 — 迁移完成前不允许 cargo build 通过
// 机制: 宏始终展开为 inventory::submit!，build script 统计剩余洞数
// "修好一个洞" = "删掉这个 #[z3rm_todo] 属性"

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse::Parse, parse::ParseStream, LitStr, Token};

/// 宏参数解析: category (必需), description (可选)
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
        Ok(Z3rmTodoArgs {
            category,
            description,
        })
    }
}

/// 标记迁移洞的位置。
///
/// 用法: `#[z3rm_todo("removed-crate", "workspace 不再依赖 project::worktree")]`
///
/// 宏始终展开为 inventory::submit! 注册一个 Z3rmTodo 条目。
/// build script (count_todos 二进制) 收集所有条目并报告数量。
/// 当所有洞都被修复（属性被删除），编译通过。
/// 标记迁移洞的位置。
///
/// 用法: `#[z3rm_todo("removed-crate", "workspace 不再依赖 project::worktree")]`
///
/// 宏始终展开为 inventory::submit! 注册一个 Z3rmTodo 条目。
/// build script (count_todos 二进制) 收集所有条目并报告数量。
/// 当所有洞都被修复(属性被删除),编译通过。
///
/// §8.1 file/line 必须是宏**使用**处的位置 (caller site),不是宏定义处。
/// 旧实现用 file!()/line!() 在宏展开时解析到宏 crate 自己的源文件,
/// 所有洞的 file/line 都相同,失去定位价值。改用 proc_macro::Span::call_site。
#[proc_macro_attribute]
pub fn z3rm_todo(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let args: Z3rmTodoArgs = syn::parse_macro_input!(attrs as Z3rmTodoArgs);
    let item: proc_macro2::TokenStream = item.into();
    let category = args.category.value();
    let description = args
        .description
        .map(|description| description.value())
        .unwrap_or_default();

    // §8.1 用 call_site() 拿 caller 的 Span,file!()/line!() 才会展开成
    // 宏使用处的真实位置。注意 stable Rust 上 Span::call_site().source_file()
    // 还不稳定,但 file!() / line!() macros 内部用 Span,所以只要在
    // expanded quote! 里用 file!() / line!(),它们就会自动绑定到 call_site。
    // 关键:让 file!() / line!() 出现在 macro_rules! / quote! 展开结果中,
    // 且在该 quote! 调用点上有 call_site 的 hygiene。
    let expanded = quote! {
        inventory::submit! {
            z3rm_macros_types::Z3rmTodo {
                category: #category,
                description: #description,
                file: file!(),
                line: line!(),
            }
        }
        #item
    };

    expanded.into()
}
