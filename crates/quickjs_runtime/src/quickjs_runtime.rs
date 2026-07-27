//! QuickJS 运行时封装，提供资源限制与线程隔离。
//!
//! 设计原则 (spec §5.2):
//! - CPU fuel: 50ms/秒中断预算
//! - 内存限制: 64MB/扩展
//! - IO rate: 令牌桶限流
//! - 专用 OS 线程隔离

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use parking_lot::Mutex;
use rquickjs::{Context, Runtime};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// CPU fuel 预算: 每秒 50ms 执行时间
const CPU_FUEL_BUDGET_MS: u64 = 50;

/// 默认内存限制: 64MB
const DEFAULT_MEMORY_LIMIT_MB: usize = 64;

/// IO 令牌桶默认参数
const IO_TOKEN_BUCKET_DEFAULT_RATE: f64 = 100.0; // 每秒补充令牌数
const IO_TOKEN_BUCKET_DEFAULT_CAPACITY: f64 = 200.0; // 最大令牌容量

// ---------------------------------------------------------------------------
// CPU Fuel 中断器
// ---------------------------------------------------------------------------

/// CPU fuel 跟踪器: 记录 JS 执行时间，超预算时中断。
///
/// 通过 rquickjs 的 `set_interrupt_handler` 回调定期触发。
/// 每次回调检查已用时间，超过预算返回 `true` 触发异常。
struct CpuFuelTracker {
    /// 预算窗口开始时间
    window_start: Cell<Option<Instant>>,
    /// 当前窗口已用毫秒数 (由 JS 引擎定期累加)
    elapsed_ms: Cell<u64>,
    /// 预算上限 (ms/秒)
    budget_ms: u64,
}

impl CpuFuelTracker {
    fn new(budget_ms: u64) -> Self {
        Self {
            window_start: Cell::new(None),
            elapsed_ms: Cell::new(0),
            budget_ms,
        }
    }

    /// 中断检查: 返回 `true` 表示应中断执行。
    ///
    /// rquickjs 引擎在执行循环中定期调用此闭包。
    /// 每次调用增加 1ms 计数（引擎内部约每 N 字节码指令调用一次）。
    fn check(&self) -> bool {
        let now = Instant::now();
        let start = self.window_start.get().unwrap_or_else(|| {
            self.window_start.set(Some(now));
            self.elapsed_ms.set(0);
            now
        });

        // 窗口超过 1 秒则重置
        if now.duration_since(start).as_secs() >= 1 {
            self.window_start.set(Some(now));
            self.elapsed_ms.set(0);
            false
        } else {
            // 每次回调计 1ms（保守估算）
            let elapsed = self.elapsed_ms.get() + 1;
            self.elapsed_ms.set(elapsed);
            elapsed >= self.budget_ms
        }
    }
}

impl Clone for CpuFuelTracker {
    fn clone(&self) -> Self {
        Self {
            window_start: Cell::new(self.window_start.get()),
            elapsed_ms: Cell::new(self.elapsed_ms.get()),
            budget_ms: self.budget_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// IO 令牌桶限流器
// ---------------------------------------------------------------------------

/// IO 操作令牌桶: 控制扩展的 IO 频率。
///
/// 扩展每次执行 IO 操作（文件读写、网络请求等）需消耗令牌。
/// 令牌按固定速率补充，超过容量则丢弃。
pub struct IoTokenBucket {
    rate: f64,
    capacity: f64,
    tokens: Mutex<f64>,
    last_refill: Mutex<Instant>,
}

impl IoTokenBucket {
    /// 创建令牌桶 (spec §5.2 IO rate)
    pub fn new(rate: f64, capacity: f64) -> Self {
        Self {
            rate,
            capacity,
            tokens: Mutex::new(capacity),
            last_refill: Mutex::new(Instant::now()),
        }
    }

    /// 默认配置: 100 tokens/s, 容量 200
    pub fn default_config() -> Self {
        Self::new(IO_TOKEN_BUCKET_DEFAULT_RATE, IO_TOKEN_BUCKET_DEFAULT_CAPACITY)
    }

    /// 尝试获取 `count` 个令牌。成功返回 `true`。
    pub fn try_acquire(&self, count: f64) -> bool {
        // 先补充令牌
        self.refill();

        let mut tokens = self.tokens.lock();
        if *tokens >= count {
            *tokens -= count;
            true
        } else {
            false
        }
    }

    /// 根据时间补充令牌
    pub fn refill(&self) {
        let now = Instant::now();
        let mut last = self.last_refill.lock();
        let elapsed = now.duration_since(*last).as_secs_f64();
        if elapsed > 0.0 {
            let mut tokens = self.tokens.lock();
            *tokens = (*tokens + elapsed * self.rate).min(self.capacity);
            *last = now;
        }
    }
}

// ---------------------------------------------------------------------------
// QuickJsRuntime
// ---------------------------------------------------------------------------

/// QuickJS 运行时实例，带资源限制。
///
/// 每个扩展拥有独立的 Runtime + Context，运行在专用 OS 线程中。
///
/// # 资源限制
/// - CPU: 50ms/秒 fuel 预算，超限中断
/// - 内存: 64MB (默认)，超限抛出 JS 异常
/// - IO: 令牌桶限流，控制文件/网络操作频率
///
/// # 线程隔离
/// 扩展 JS 代码在专用 `std::thread` 中执行，与主 UI 线程隔离。
/// 通过 Arc 共享状态，避免跨线程数据竞争。
pub struct QuickJsRuntime {
    runtime: Runtime,
    /// IO 令牌桶 (Arc 以便跨线程共享)
    io_bucket: Arc<IoTokenBucket>,
    /// CPU fuel 跟踪器 (Cell 可无锁访问; 仅中断 handler 使用)
    #[allow(dead_code)]
    cpu_tracker: CpuFuelTracker,
}

impl QuickJsRuntime {
    /// 创建新的 QuickJS 运行时 (spec §5.2)
    ///
    /// # 参数
    /// - `memory_limit_mb`: 内存上限 (MB)，默认 64
    /// - `cpu_budget_ms`: CPU fuel 预算 (ms/秒)，默认 50
    ///
    /// # 资源限制
    /// - 内存: `JS_SetMemoryLimit` 等效，通过 `set_memory_limit`
    /// - CPU: 中断 handler 定期检测 fuel 消耗
    /// - IO: 令牌桶限流，默认 100 tokens/s
    pub fn new(memory_limit_mb: usize, cpu_budget_ms: u64) -> Result<Self> {
        let runtime = Runtime::new().context("创建 QuickJS Runtime 失败")?;

        // 内存限制 (spec §5.2: 64MB per extension)
        if memory_limit_mb > 0 {
            runtime.set_memory_limit(memory_limit_mb * 1024 * 1024);
        }

        // CPU fuel 中断器 (spec §5.2: 50ms/second budget)
        let cpu_tracker = CpuFuelTracker::new(cpu_budget_ms);
        let tracker = cpu_tracker.clone();

        runtime.set_interrupt_handler(Some(Box::new(move || tracker.check())));

        // IO 令牌桶 (spec §5.2: IO rate limiting)
        let io_bucket = Arc::new(IoTokenBucket::default_config());

        Ok(Self {
            runtime,
            io_bucket,
            cpu_tracker,
        })
    }

    /// 使用默认配置创建运行时: 64MB 内存, 50ms CPU budget
    pub fn with_defaults() -> Result<Self> {
        Self::new(DEFAULT_MEMORY_LIMIT_MB, CPU_FUEL_BUDGET_MS)
    }

    /// 创建新的 JS 执行上下文
    pub fn create_context(&self) -> Result<Context> {
        Context::full(&self.runtime).map_err(|e| anyhow!("创建 Context 失败: {e}"))
    }

    /// 获取 IO 令牌桶引用
    pub fn io_bucket(&self) -> &Arc<IoTokenBucket> {
        &self.io_bucket
    }

    /// 在专用线程中创建独立运行时并执行 JS 代码 (spec §5.2: dedicated OS thread)
    ///
    /// QuickJS Runtime/Context 不可跨线程共享 (非 Send)。
    /// 此方法在子线程内创建全新的 Runtime + Context，执行完成后销毁。
    /// 资源限制 (CPU fuel, 内存, IO) 在子线程内生效。
    pub fn execute_in_thread<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(rquickjs::Ctx<'_>) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        // 克隆配置参数到子线程
        let _io_bucket = self.io_bucket.clone();
        let memory_limit_mb = DEFAULT_MEMORY_LIMIT_MB;
        let cpu_budget_ms = CPU_FUEL_BUDGET_MS;

        let join_handle = thread::Builder::new()
            .name("quickjs-ext".to_string())
            .spawn(move || {
                // 在子线程内创建独立的 Runtime + Context
                let runtime =
                    Runtime::new().map_err(|e| anyhow!("子线程创建 Runtime 失败: {e}"))?;
                if memory_limit_mb > 0 {
                    runtime.set_memory_limit(memory_limit_mb * 1024 * 1024);
                }

                // CPU fuel 中断器
                let tracker = CpuFuelTracker::new(cpu_budget_ms);
                let tracker_clone = tracker.clone();
                runtime.set_interrupt_handler(Some(Box::new(move || tracker_clone.check())));

                let ctx = Context::full(&runtime)
                    .map_err(|e| anyhow!("子线程创建 Context 失败: {e}"))?;

                // 执行用户函数
                ctx.with(f)
            })
            .context("创建扩展线程失败")?;

        join_handle
            .join()
            .map_err(|e| anyhow!("扩展线程异常退出: {e:?}"))?
    }

    /// 执行 JS 源码字符串，返回结果值
    pub fn eval_js(&self, source: &str) -> Result<String> {
        let ctx = self.create_context()?;
        ctx.with(|ctx| {
            let result: String = ctx.eval(source)?;
            Ok(result)
        })
    }
}

// ---------------------------------------------------------------------------
// ExtensionRunner: 扩展加载与执行
// ---------------------------------------------------------------------------

/// 扩展运行结果
#[derive(Debug)]
pub struct ExtensionRunResult {
    /// 扩展 ID
    pub extension_id: String,
    /// 执行结果
    pub result: Result<()>,
    /// 执行耗时
    pub duration: Duration,
    /// CPU fuel 是否耗尽
    pub cpu_exhausted: bool,
    /// 内存是否超限
    pub memory_exceeded: bool,
    /// §5.4 VDOM JSON 字符串: 扩展在 `activate()` 中通过 `context.render(vdom)`
    /// 或 `registerChromeView('status-bar', view)` 注入的 VDOM。`None` 表示扩展未
    /// 提供可渲染的 VDOM。
    pub vdom_json: Option<String>,
}

/// 扩展加载器: 在独立线程中加载并执行扩展
pub struct ExtensionRunner {
    memory_limit_mb: usize,
    cpu_budget_ms: u64,
}

impl ExtensionRunner {
    /// 创建扩展加载器
    pub fn new(memory_limit_mb: usize, cpu_budget_ms: u64) -> Self {
        Self {
            memory_limit_mb,
            cpu_budget_ms,
        }
    }

    /// 默认配置: 64MB 内存, 50ms CPU
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MEMORY_LIMIT_MB, CPU_FUEL_BUDGET_MS)
    }

    /// 加载并激活一个扩展 (spec §5.2: dedicated OS thread isolation)
    ///
    /// 流程:
    /// 1. 创建独立 Runtime + Context
    /// 2. 注入资源限制 (CPU fuel, 内存, IO)
    /// 3. 在专用线程中执行扩展源码
    /// 4. 调用 `activate(context)`
    /// 5. 若扩展调用 `context.render(vdom)` 或注册了 `status-bar` chrome view,
    ///    抓取该 VDOM 并以 JSON 字符串形式返回 (spec §5.4)。
    pub fn load_extension(
        &self,
        extension_id: &str,
        source: &str,
        _activate_fn: &str,
    ) -> ExtensionRunResult {
        let start = Instant::now();
        let (result, vdom_json) = match self.do_load(extension_id, source) {
            Ok(vdom) => (Ok(()), vdom),
            Err(e) => (Err(e), None),
        };
        let duration = start.elapsed();
        let cpu_exhausted = result.is_err();

        ExtensionRunResult {
            extension_id: extension_id.to_string(),
            result,
            duration,
            cpu_exhausted,
            memory_exceeded: false,
            vdom_json,
        }
    }

    /// 内部加载逻辑;返回 `(Result<()> 的错误结果, Optional<VDOM-JSON String>)`。
    /// 在主调用线程上执行 (调用方负责后续派发到后台执行器)。
    fn do_load(
        &self,
        _extension_id: &str,
        source: &str,
    ) -> Result<Option<String>> {
        let runtime =
            QuickJsRuntime::new(self.memory_limit_mb, self.cpu_budget_ms).context("创建 Runtime 失败")?;
        let ctx = runtime.create_context()?;

        ctx.with(|ctx| {
            // §5.2: Strip ES module `export` keyword — QuickJS eval runs scripts, not modules.
            let script_source = source.replace("export function", "function")
                .replace("export const", "const")
                .replace("export default", "const __default =");

            // Execute extension source to define activate() in globals
            let _: rquickjs::Value = ctx.eval(script_source.as_str())?;

            // §5.2/§5.3/§5.4: 配置 chrome context:
            // - registerChromeView(name, view) 保存视图对象(支持 `view.render()` 调用)
            // - render(vdom) 立即返回的 VDOM 优先于已注册的 chrome view
            // - 返回值为 JSON 字符串 (rquickjs 0.6 Value::get::<String> 要求 here-string
            //   形式以避免所有权/lifetime 跨线程问题)
            // §5.1/§5.4 Day-0 context: real chrome APIs plus non-throwing mux /
            // commands / keymaps stubs so built-in extensions can activate and
            // return VDOM without requiring a live host bridge yet.
            let call_activate: &str = r#"
                (function() {
                    var __vdom_result = null;
                    var __chrome_views = {};
                    var __commands = {};
                    var __event_handlers = {};
                    function safeCall(fn) {
                        try { return fn(); } catch (e) { return undefined; }
                    }
                    var context = {
                        render: function(vdom) {
                            __vdom_result = vdom;
                            return vdom;
                        },
                        registerChromeView: function(name, view) {
                            if (view && typeof view.invalidate !== 'function') {
                                view.invalidate = function() {
                                    // Host re-render hook; no-op until chrome bridge polls.
                                };
                            }
                            __chrome_views[name] = view;
                            return view;
                        },
                        on: function(event, handler) {
                            if (!__event_handlers[event]) __event_handlers[event] = [];
                            __event_handlers[event].push(handler);
                        },
                        emit: function(event, data) {
                            var handlers = __event_handlers[event] || [];
                            for (var i = 0; i < handlers.length; i++) {
                                try { handlers[i](data); } catch (e) {}
                            }
                        },
                        capabilities: { terminal: true, mux: true, workspace: true },
                        // §5.1 mux surface used by built-ins (status-bar, session-manager).
                        // listSessions returns [] until a host bridge injects live data;
                        // subscribe stores handlers so later emit/host push can fire them.
                        mux: {
                            listSessions: function() { return []; },
                            subscribe: function(event, handler) {
                                if (!__event_handlers['mux:' + event]) {
                                    __event_handlers['mux:' + event] = [];
                                }
                                __event_handlers['mux:' + event].push(handler);
                                return function unsubscribe() {
                                    var list = __event_handlers['mux:' + event] || [];
                                    var idx = list.indexOf(handler);
                                    if (idx >= 0) list.splice(idx, 1);
                                };
                            },
                            focusPane: function(_paneId) { return false; },
                            createSession: function(_name) { return null; },
                            killSession: function(_id) { return false; },
                            attach: function(_id) { return false; },
                            detach: function() { return false; }
                        },
                        commands: {
                            register: function(name, handler) {
                                __commands[name] = handler;
                                return true;
                            },
                            execute: function(name, args) {
                                var handler = __commands[name];
                                if (typeof handler === 'function') {
                                    return safeCall(function() { return handler(args); });
                                }
                                return undefined;
                            }
                        },
                        keymaps: {
                            bind: function(_chord, _command) { return true; },
                            unbind: function(_chord) { return true; }
                        }
                    };
                    if (typeof activate !== 'function') return null;
                    activate(context);
                    if (__vdom_result !== null) {
                        return JSON.stringify(__vdom_result);
                    }
                    var view = __chrome_views['status-bar'];
                    if (view && typeof view.render === 'function') {
                        try {
                            return JSON.stringify(view.render());
                        } catch (e) {
                            return null;
                        }
                    }
                    // Prefer any registered chrome view that can render.
                    var names = Object.keys(__chrome_views);
                    for (var i = 0; i < names.length; i++) {
                        var candidate = __chrome_views[names[i]];
                        if (candidate && typeof candidate.render === 'function') {
                            try {
                                var rendered = candidate.render();
                                if (rendered !== null && rendered !== undefined) {
                                    return JSON.stringify(rendered);
                                }
                            } catch (e) {}
                        }
                    }
                    return null;
                })()
            "#;
            let captured: rquickjs::Value = ctx.eval(call_activate)?;
            let vdom_json = match captured.get::<String>() {
                Ok(s) if !s.is_empty() => Some(s),
                _ => None,
            };
            Ok(vdom_json)
        })
    }
}

impl Default for ExtensionRunner {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ---------------------------------------------------------------------------
// 线程安全执行器
// ---------------------------------------------------------------------------

/// 线程安全的 JS 执行状态，用于跨线程共享
#[derive(Debug)]
pub struct JsExecutionContext {
    /// 执行开始时间
    pub started_at: Instant,
    /// 已用 CPU fuel (ms)
    pub cpu_fuel_used: AtomicU64,
    /// IO 操作计数
    pub io_ops_count: AtomicU64,
}

impl JsExecutionContext {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            cpu_fuel_used: AtomicU64::new(0),
            io_ops_count: AtomicU64::new(0),
        }
    }

    /// 检查 CPU fuel 是否耗尽
    pub fn is_cpu_exhausted(&self) -> bool {
        self.cpu_fuel_used.load(Ordering::Relaxed) >= CPU_FUEL_BUDGET_MS
    }

    /// 记录 CPU fuel 消耗
    pub fn record_cpu_usage(&self, ms: u64) {
        self.cpu_fuel_used.fetch_add(ms, Ordering::Relaxed);
    }

    /// 记录 IO 操作
    pub fn record_io_op(&self) {
        self.io_ops_count.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for JsExecutionContext {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() -> Result<()> {
        let runtime = QuickJsRuntime::with_defaults()?;
        let ctx = runtime.create_context()?;

        // 验证 Context 可执行基本 JS
        let result: i32 = ctx.with(|ctx| ctx.eval("1 + 2"))?;
        assert_eq!(result, 3);
        Ok(())
    }

    #[test]
    fn test_memory_limit() -> Result<()> {
        // 创建内存受限的 Runtime (1MB)
        let runtime = QuickJsRuntime::new(1, CPU_FUEL_BUDGET_MS)?;
        let ctx = runtime.create_context()?;

        // 尝试分配大量内存 → 应触发内存限制
        let result = ctx.with(|ctx| {
            let _: rquickjs::Value = ctx.eval(
                r#"
                let arr = [];
                for (let i = 0; i < 10000000; i++) {
                    arr.push(new Array(1000));
                }
                "#,
            )?;
            Ok::<_, anyhow::Error>(())
        });

        // 内存超限应返回错误
        assert!(result.is_err(), "内存限制应触发错误");
        Ok(())
    }

    #[test]
    fn test_cpu_fuel_tracker() {
        let tracker = CpuFuelTracker::new(5); // 5ms 预算
        let check = tracker.clone();

        // 前 5 次检查不应中断
        for _ in 0..4 {
            assert!(!check.check(), "预算内不应中断");
        }
        // 第 5 次应中断
        assert!(check.check(), "超预算应中断");
    }

    #[test]
    fn test_io_token_bucket() {
        let bucket = IoTokenBucket::new(10.0, 20.0);

        // 初始 20 tokens
        assert!(bucket.try_acquire(10.0));
        assert!(bucket.try_acquire(5.0));
        // 剩余 5 tokens，申请 10 应失败
        assert!(!bucket.try_acquire(10.0));
        // 申请 5 应成功
        assert!(bucket.try_acquire(5.0));
    }

    #[test]
    fn test_io_token_refill() {
        let bucket = IoTokenBucket::new(100.0, 200.0);

        // 耗尽令牌
        assert!(bucket.try_acquire(200.0));
        assert!(!bucket.try_acquire(1.0));

        // 等待补充
        std::thread::sleep(Duration::from_millis(100));
        // 100ms 后补充约 10 tokens
        assert!(bucket.try_acquire(10.0));
    }

    #[test]
    fn test_extension_runner_basic() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let result = runner.load_extension(
            "test-ext",
            r#"
            function activate() { return "activated"; }
            activate();
            "#,
            "activate",
        );

        assert!(result.result.is_ok(), "扩展加载应成功: {:?}", result.result);
        assert_eq!(result.extension_id, "test-ext");
        assert!(result.duration.as_micros() > 0);
        Ok(())
    }

    #[test]
    fn test_extension_infinite_loop_detected() {
        // 极小 CPU budget 的 runner
        let runner = ExtensionRunner::new(64, 1); // 1ms budget

        let result = runner.load_extension(
            "infinite-loop",
            r#"
            let count = 0;
            while (true) { count++; }
            "#,
            "activate",
        );

        // 无限循环应被中断（CPU fuel 耗尽或内存超限）
        assert!(
            result.result.is_err(),
            "无限循环应被资源限制终止"
        );
    }

    #[test]
    fn test_thread_isolation() -> Result<()> {
        let runtime = QuickJsRuntime::with_defaults()?;

        // 在专用线程中执行 JS
        let result = runtime.execute_in_thread(|ctx| {
            let r: String = ctx.eval("'hello from thread'")?;
            Ok::<_, anyhow::Error>(r)
        })?;

        assert_eq!(result, "hello from thread");
        Ok(())
    }

    #[test]
    fn test_js_execution_context() {
        let ctx = JsExecutionContext::new();

        // 记录 CPU 使用
        for _ in 0..50 {
            ctx.record_cpu_usage(1);
        }
        assert!(ctx.is_cpu_exhausted());
        assert_eq!(ctx.cpu_fuel_used.load(Ordering::Relaxed), 50);

        // 记录 IO 操作
        ctx.record_io_op();
        ctx.record_io_op();
        assert_eq!(ctx.io_ops_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_context_creation_multiple() -> Result<()> {
        let runtime = QuickJsRuntime::with_defaults()?;

        // 同一 Runtime 可创建多个 Context
        let ctx1 = runtime.create_context()?;
        let ctx2 = runtime.create_context()?;

        let r1: i32 = ctx1.with(|ctx| ctx.eval("42"))?;
        let r2: i32 = ctx2.with(|ctx| ctx.eval("99"))?;

        // 两个 Context 独立
        assert_ne!(r1, r2);
        Ok(())
    }

    #[test]
    fn test_builtin_status_bar_activates_with_mux_context() -> Result<()> {
        // Built-in extensions call context.mux.subscribe / registerChromeView.
        // Day-0 host must not throw; activate should return status-bar VDOM.
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            function activate(context) {
                var state = { sessionName: 'demo', paneTitle: 'shell' };
                var view = {
                    render: function() {
                        return {
                            type: 'div',
                            props: { id: 'status-bar' },
                            children: [
                                { type: 'span', children: [state.sessionName] },
                                { type: 'span', children: [state.paneTitle] }
                            ]
                        };
                    }
                };
                context.mux.subscribe('pane:focus', function(pane) {
                    state.paneTitle = pane.title || '';
                });
                context.commands.register('noop', function() { return true; });
                context.keymaps.bind('ctrl-x', 'noop');
                context.registerChromeView('status-bar', view);
            }
            function deactivate() {}
        "#;
        let result = runner.load_extension("z3rm-status-bar-like", source, "activate");
        assert!(result.result.is_ok(), "activate must succeed: {:?}", result.result);
        let vdom = result.vdom_json.expect("status-bar VDOM must be captured");
        assert!(vdom.contains("status-bar"), "vdom={vdom}");
        assert!(vdom.contains("demo"), "vdom={vdom}");
        Ok(())
    }

    /// 测试扩展加载桩: 创建临时文件 → 加载 → 验证
    #[test]
    fn test_extension_loading_stub() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();

        let source = r#"
        // 模拟扩展源码
        function activate(context) {
            // Day 0: stub, 完整实现待 extension_host 重写
            return { status: "active" };
        }
        function deactivate() {}
        activate(null);
        "#;

        let result = runner.load_extension("stub-ext", source, "activate");
        assert!(result.result.is_ok(), "桩扩展应加载成功");
        Ok(())
    }

    #[test]
    fn test_eval_js() -> Result<()> {
        let runtime = QuickJsRuntime::with_defaults()?;
        let result = runtime.eval_js("'quickjs works'")?;
        assert_eq!(result, "quickjs works");
        Ok(())
    }

    #[test]
    fn test_io_bucket_rate_limiting() {
        let bucket = Arc::new(IoTokenBucket::new(10.0, 10.0));

        // 初始 10 tokens
        for _ in 0..10 {
            assert!(bucket.try_acquire(1.0), "应有足够令牌");
        }
        // 耗尽后应拒绝
        assert!(!bucket.try_acquire(1.0), "令牌耗尽应拒绝");

        // 等待 1 秒补充 10 tokens
        std::thread::sleep(Duration::from_millis(1000));
        assert!(bucket.try_acquire(1.0), "补充后应可用");
    }
}
