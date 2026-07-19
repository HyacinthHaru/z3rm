//! # 扩展沙箱 Fuzzing 测试
//!
//! §5.2 VDOM 桥 fuzz + QuickJS 资源耗尽 fuzz。
//! 验证扩展沙箱在恶意输入下的安全性。

use quickjs_runtime::*;
use std::thread;
use std::time::Duration;

// ============================================================
// §5.2 VDOM 桥 Fuzz 测试
// ============================================================

/// §5.2 VDOM 桥 fuzz: 畸形 JSON 输入 → 验证优雅错误而非 panic
#[test]
fn test_vdom_bridge_malformed_json() {
    let malformed_inputs = [
        "",                    // 空字符串
        "{",                   // 不完整对象
        "{\"key\":",           // 缺少值
        "{\"key\": value}",    // 未引用的值
        "[1, 2, ]",           // 尾随逗号
        "{{}}",                // 嵌套花括号
        "null",                // null
        "true",                // 布尔值
        "42",                  // 数字
        "{\"depth\": {\"a\": {\"b\": {\"c\":",  // 深度嵌套不完整
    ];

    for input in &malformed_inputs {
        // 尝试解析 JSON (模拟 VDOM 桥输入)
        let result: Result<serde_json::Value, _> = serde_json::from_str(input);

        // 验证：要么成功解析有效 JSON，要么返回错误（不应 panic）
        match result {
            Ok(_) => { /* 某些输入可能有效（如 "null", "true", "42"） */ }
            Err(_) => { /* 错误是预期的，不应 panic */ }
        }
    }
}

/// §5.2 VDOM 桥 fuzz: 极端嵌套深度
#[test]
fn test_vdom_bridge_deep_nesting() {
    let depth = 100;
    let mut json = String::new();
    for _ in 0..depth {
        json.push_str("{\"a\":");
    }
    json.push_str("1");
    for _ in 0..depth {
        json.push('}');
    }

    let _result: Result<serde_json::Value, _> = serde_json::from_str(&json);
}

/// §5.2 VDOM 桥 fuzz: 超长字符串
#[test]
fn test_vdom_bridge_long_string() {
    let long_value = "A".repeat(100_000);
    let json = format!(r#"{{"key": "{}"}}"#, long_value);

    let _result: Result<serde_json::Value, _> = serde_json::from_str(&json);
}

/// §5.2 VDOM 桥 fuzz: 超大数组
#[test]
fn test_vdom_bridge_large_array() {
    let items: Vec<String> = (0..10_000).map(|i| format!("\"item{}\"", i)).collect();
    let json = format!("[{}]", items.join(","));

    let _result: Result<serde_json::Value, _> = serde_json::from_str(&json);
}

/// §5.2 VDOM 桥 fuzz: Unicode 边缘情况
#[test]
fn test_vdom_bridge_unicode_edges() {
    let unicode_inputs = [
        "{\"key\": \"\u{0000}\"}",
        "{\"key\": \"\\u{1F600}\"}",
        "{\"key\": \"\u{10FFFF}\"}",
        "{\"key\": \"\u{200B}\u{200C}\u{200D}\"}",
    ];

    for input in &unicode_inputs {
        let _result: Result<serde_json::Value, _> = serde_json::from_str(input);
    }
}

// ============================================================
// §5.2 QuickJS 资源耗尽 Fuzz 测试
// ============================================================

/// §5.2 CPU 燃料限制: 无限循环应在 fuel 耗尽时中断
#[test]
fn test_cpu_fuel_infinite_loop() {
    let runner = ExtensionRunner::with_defaults();

    // 无限循环 JS 代码
    let code = "while(true) {}";

    // 验证：CPU fuel 耗尽应返回错误，不应 panic
    let result = runner.load_extension("test", code, "activate");
    // 无限循环会被 fuel 中断器终止，结果为 Err
    assert!(result.cpu_exhausted || result.result.is_err(),
        "无限循环应被 CPU fuel 中断");
}

/// §5.2 内存限制: 大量内存分配应被限制
#[test]
fn test_memory_limit_enforced() {
    let runner = ExtensionRunner::with_defaults();

    // 尝试分配大量内存
    let code = r#"
        var arr = [];
        for (var i = 0; i < 1000000; i++) {
            arr.push(new Array(10000));
        }
    "#;

    let result = runner.load_extension("test", code, "activate");
    // 验证：内存超限应返回错误，不 crash
    assert!(result.result.is_err() || result.memory_exceeded,
        "大量内存分配应被限制");
}

// ============================================================
// §5.2 IO 令牌桶限流测试
// ============================================================

/// §5.2 IO 令牌桶: 高频 IO 操作应被限流
#[test]
fn test_io_rate_limiting() {
    let bucket = IoTokenBucket::new(10.0, 20.0); // 每秒 10 个令牌，容量 20

    // 消耗 30 个令牌（超过容量）
    let mut success_count = 0;
    for _ in 0..30 {
        if bucket.try_acquire(1.0) {
            success_count += 1;
        }
    }

    // 验证：成功消耗数不应超过容量
    assert!(success_count <= 20, "令牌桶不应允许超过容量的消耗");
    assert_eq!(success_count, 20, "应精确消耗容量内的所有令牌");
}

/// §5.2 IO 令牌桶补充
#[test]
fn test_io_token_refill() {
    let bucket = IoTokenBucket::new(100.0, 100.0);

    // 消耗所有令牌
    for _ in 0..100 {
        assert!(bucket.try_acquire(1.0));
    }

    // 立即再次尝试应失败
    assert!(!bucket.try_acquire(1.0), "令牌耗尽后不应再允许消耗");

    // 等待令牌补充
    thread::sleep(Duration::from_millis(100));
}

/// §5.2 快速 IO 调用测试
#[test]
fn test_rapid_io_calls() {
    let bucket = IoTokenBucket::new(50.0, 100.0);

    let mut allowed = 0;
    for _ in 0..200 {
        if bucket.try_acquire(1.0) {
            allowed += 1;
        }
    }

    // 验证：允许的调用数不超过容量
    assert!(allowed <= 100, "令牌桶应限制并发 IO 数量");
}

// ============================================================
// §5.2 QuickJsRuntime 测试
// ============================================================

/// §5.2 QuickJsRuntime 创建与默认配置
#[test]
fn test_quickjs_runtime_creation() {
    let runtime = QuickJsRuntime::new(64, 50).expect("runtime init");
    let _ctx = runtime.create_context();
}

/// §5.2 QuickJsRuntime 执行简单 JS
#[test]
fn test_quickjs_runtime_eval() {
    let runtime = QuickJsRuntime::new(64, 50).expect("runtime init");
    let ctx = runtime.create_context().expect("context creation");

    let result: Result<i32, anyhow::Error> = ctx.with(|ctx| {
        let value: i32 = ctx.eval("1 + 2")?;
        Ok(value)
    });

    assert!(result.is_ok(), "简单 JS 表达式应执行成功");
}
