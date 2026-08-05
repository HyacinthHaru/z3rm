//! 每个在文档注释里声明了 `Default:` 的设置字段，都必须真的出现在
//! `assets/settings/default.json` 里。
//!
//! 这条约束是被一次真实事故逼出来的：一个提交在无关的改动里删掉了 default.json
//! 的 2837 行，之后有人手打了一百来行加回去。加回去的值有四个和上游不一样，还有
//! 四个字段干脆没加回来 —— 代码里 `unwrap_or(...)` 兜住了行为，所以什么都没坏，
//! 只是用户再也无法从随包设置里发现这些键的存在。缺失不会让任何测试变红，正是
//! 这类退化最难被发现的原因。
//!
//! 这里只查"在不在"，不查"值对不对"：把文档里的 `Default: "platform_default"`
//! 解析成 JSON 值需要一套小型解析器，而缺失本身已经覆盖了实际发生过的事故形态。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    // CARGO_MANIFEST_DIR 是 crates/settings_content。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 剥掉 `//` 行注释和 `/* */` 块注释，以及尾随逗号。default.json 是手写 JSONC。
fn strip_jsonc(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let mut in_string = false;
    while index < bytes.len() {
        let character = bytes[index];
        if in_string {
            out.push(character);
            if character == '\\' && index + 1 < bytes.len() {
                out.push(bytes[index + 1]);
                index += 2;
                continue;
            }
            if character == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            in_string = true;
            out.push(character);
            index += 1;
            continue;
        }
        if character == '/' && bytes.get(index + 1) == Some(&'/') {
            while index < bytes.len() && bytes[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if character == '/' && bytes.get(index + 1) == Some(&'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == '*' && bytes[index + 1] == '/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        out.push(character);
        index += 1;
    }
    // 尾随逗号：`,` 后面除空白外只跟 `}` 或 `]`。
    let mut cleaned = String::with_capacity(out.len());
    let characters: Vec<char> = out.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == ',' {
            let mut lookahead = index + 1;
            while lookahead < characters.len() && characters[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if matches!(characters.get(lookahead), Some('}') | Some(']')) {
                index += 1;
                continue;
            }
        }
        cleaned.push(characters[index]);
        index += 1;
    }
    cleaned
}

fn collect_object_keys(value: &serde_json::Value, keys: &mut HashSet<String>) {
    if let Some(object) = value.as_object() {
        for (key, nested) in object {
            keys.insert(key.clone());
            collect_object_keys(nested, keys);
        }
    }
}

/// `(字段名, 文档里写的默认值, 所在文件)`。
fn fields_with_a_documented_default(source_directory: &Path) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(source_directory) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "rs") {
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // 一个字段的文档注释是紧挨在 `pub name:` 上方的连续 `///` 行。
            let mut documented_default: Option<String> = None;
            for line in contents.lines() {
                let trimmed = line.trim();
                if let Some(doc) = trimmed.strip_prefix("///") {
                    if let Some(position) = doc.find("Default:") {
                        documented_default =
                            Some(doc[position + "Default:".len()..].trim().to_string());
                    }
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("pub ") {
                    if let Some(name) = rest.split(':').next() {
                        if let Some(default) = documented_default.take() {
                            found.push((name.trim().to_string(), default, file_name.clone()));
                        }
                    }
                    continue;
                }
                // 任何既不是文档注释也不是字段的行，都终止当前的文档块。
                if !trimmed.is_empty() && !trimmed.starts_with("#[") {
                    documented_default = None;
                }
            }
        }
    }
    found
}

#[test]
fn every_documented_default_is_shipped_in_default_json() {
    let root = repository_root();
    let default_json = root.join("assets/settings/default.json");
    let contents = std::fs::read_to_string(&default_json)
        .unwrap_or_else(|error| panic!("reading {}: {error}", default_json.display()));
    let parsed: serde_json::Value = serde_json::from_str(&strip_jsonc(&contents))
        .unwrap_or_else(|error| panic!("parsing {} as JSONC: {error}", default_json.display()));

    let mut shipped = HashSet::new();
    collect_object_keys(&parsed, &mut shipped);

    let documented = fields_with_a_documented_default(&root.join("crates/settings_content/src"));
    assert!(
        documented.len() > 50,
        "the doc-comment scan found only {} fields, so it is not reading the sources",
        documented.len()
    );

    let missing: Vec<String> = documented
        .iter()
        .filter(|(name, _, _)| !shipped.contains(name))
        .map(|(name, default, file)| format!("  {name} (Default: {default}) [{file}]"))
        .collect();
    assert!(
        missing.is_empty(),
        "these fields document a default that assets/settings/default.json never ships, \
         so users cannot discover them:\n{}",
        missing.join("\n")
    );
}
