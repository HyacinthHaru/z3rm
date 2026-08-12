//! SPEC 里那些"不要这么做"的硬约束。
//!
//! 这些约束的特点是：违反它们**不会让任何东西编译失败或行为出错**，只会让架构
//! 悄悄退化，等发现时已经付出了代价。SPEC 把它们写成禁令而不是需求，正说明它们
//! 靠的是纪律而不是类型系统。这里把纪律变成会红的测试。
//!
//! 数值型的约束在各自 crate 里就地断言（`D_MAX` 在 `delta_chain.rs`，
//! `keep_alive` 默认值在 `server_settings.rs`），这里只放需要跨 crate 扫描的那些。

use std::path::{Path, PathBuf};

/// mux 这一侧的全部源码目录。
fn mux_source_roots() -> Vec<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates = manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("crates"));
    ["mux", "mux_server", "mux_protocol"]
        .into_iter()
        .map(|name| crates.join(name).join("src"))
        .collect()
}

/// 收集目录下所有 `.rs` 的 `(路径, 去掉行注释的内容)`。
///
/// 这个仓库到处引用 SPEC 原文，禁用词在文档注释里出现是常态；不剥注释的话每条
/// 断言都会被自己的解释文字触发。
fn source_files_without_line_comments(root: &Path) -> Vec<(PathBuf, String)> {
    let mut collected = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return collected;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collected.extend(source_files_without_line_comments(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let stripped = contents
                .lines()
                .map(|line| line.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            collected.push((path, stripped));
        }
    }
    collected
}

fn offenders(needles: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    for root in mux_source_roots() {
        for (path, contents) in source_files_without_line_comments(&root) {
            for (number, line) in contents.lines().enumerate() {
                if needles.iter().any(|needle| line.contains(needle)) {
                    found.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    found
}

/// §3.2 `MuxDomain` 是个具体 struct。只有一个实现时不要为它抽 trait；等第二个
/// 真实实现出现了再抽。
///
/// 一个只有单实现的 `dyn Domain` 会把每次调用变成虚分发、让类型推导变差，换来
/// 的"可扩展性"是假的 —— 而这笔账要等到有人试图删掉它时才结清。
#[test]
fn there_is_no_domain_trait_for_a_single_implementation() {
    let found = offenders(&["trait Domain", "dyn Domain"]);
    assert!(
        found.is_empty(),
        "§3.2 forbids a Domain trait while MuxDomain is the only implementation:\n{}",
        found.join("\n")
    );
}

/// §3.1 本地和远程会话走同一条路径：socket 上的分帧二进制协议。没有共享内存快
/// 通道，也没有第二套解析。
///
/// 共享内存快通道的诱惑在于"本地场景能更快"，代价是两条数据路径要各自维护、各自
/// 出 bug，而远程路径永远得不到本地路径那样的实战检验。
#[test]
fn there_is_no_shared_memory_fast_path() {
    let found = offenders(&["memmap", "shm_open", "MAP_SHARED", "shared_memory"]);
    assert!(
        found.is_empty(),
        "§3.1 requires one data path (framed binary protocol over a socket); \
         a shared-memory fast path would create a second one:\n{}",
        found.join("\n")
    );
}

/// 上面两条断言只有在扫描真的能命中东西时才有意义。路径拼错、注释剥离过头、
/// 匹配逻辑写反 —— 任何一个都会让它们变成永远为真的摆设。
#[test]
fn the_scan_can_actually_find_things() {
    let files: usize = mux_source_roots()
        .iter()
        .map(|root| source_files_without_line_comments(root).len())
        .sum();
    assert!(
        files >= 3,
        "expected to scan the mux crates' sources, found {files} files under {:?}",
        mux_source_roots()
    );

    // 正向对照：一个确定出现在非注释代码里的 token 必须被扫到。
    assert!(
        !offenders(&["pub struct MuxDomain"]).is_empty(),
        "the scan found no `pub struct MuxDomain`, so it cannot be trusted to find a violation"
    );
    // 反向对照：只出现在注释里的东西必须被剥掉，否则每条禁令都会被自己的说明触发。
    assert!(
        offenders(&["§3.2 forbids"]).is_empty(),
        "line comments are not being stripped; the guards would fire on their own prose"
    );
}
