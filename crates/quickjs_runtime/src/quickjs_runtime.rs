//! QuickJS 运行时封装，提供资源限制与线程隔离。
//!
//! 设计原则 (spec §5.2):
//! - CPU fuel: 每 wall-clock 秒 50ms 执行预算，连续超支 3 次 (~150ms) 才中断
//! - 内存限制: 64MB/扩展
//! - IO rate: 令牌桶限流
//! - 专用 OS 线程隔离
//!
//! 本 crate 不依赖 `mux`/`gpui`：宿主能力通过 [`HostBridge`] trait 注入
//! (spec §5.4)，由嵌入方 (z3rm) 实现真实的 mux/settings/terminal 调用。

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::Mutex;
use rquickjs::{Context, Function, Runtime, prelude::CatchResultExt};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// CPU fuel 预算: 每秒 50ms 执行时间
const CPU_FUEL_BUDGET_MS: u64 = 50;

/// CPU fuel 预算窗口 (spec §5.2: "50ms per second")
const CPU_FUEL_WINDOW: Duration = Duration::from_secs(1);

/// spec §5.2: 扩展连续 3 次超预算后才被杀掉 (默认预算下约 150ms)。
/// 单次超支不中断，避免偶发的长任务被误杀。
const CPU_OVER_BUDGET_KILL_MULTIPLE: u32 = 3;

/// 默认内存限制: 64MB
const DEFAULT_MEMORY_LIMIT_MB: usize = 64;

/// IO 令牌桶默认参数
const IO_TOKEN_BUCKET_DEFAULT_RATE: f64 = 100.0; // 每秒补充令牌数
/// 桶容量 = 速率 × 该系数，允许短时突发。
const IO_TOKEN_BUCKET_BURST_FACTOR: f64 = 2.0;

/// `filesystem.readTextFile` 单文件读取上限: 有界分配, 拒绝巨型文件。
pub const MAX_EXTENSION_FILE_READ: u64 = 1024 * 1024;
/// `filesystem.readDir` 单目录条目上限: 防止恶意目录撑爆宿主。
pub const MAX_EXTENSION_DIR_ENTRIES: usize = 1000;

/// §5.6 `settings.*` 键上限: 点分路径总字节数。
pub const MAX_EXTENSION_SETTINGS_KEY_LEN: usize = 256;
/// §5.6 `settings.*` 键段数上限。
pub const MAX_EXTENSION_SETTINGS_SEGMENTS: usize = 32;
/// §5.6 `settings.set` 单个值的序列化大小上限: 拒绝把巨型值写进用户设置。
pub const MAX_EXTENSION_SETTINGS_VALUE_BYTES: usize = 64 * 1024;
/// §5.6 settings 文档读写大小上限: 超限文件在读取前就被拒绝, 不做无界分配。
pub const MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES: u64 = 1024 * 1024;

/// §5.6 `network.fetch` URL 总长上限。
pub const MAX_EXTENSION_URL_LEN: usize = 8192;
/// §5.6 `network.fetch` 默认超时 (覆盖整个请求 + 响应体读取)。
pub const EXTENSION_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// §5.6 `network.fetch` 超时上限 (毫秒): 扩展传入的 `options.timeout` 封顶于此。
pub const EXTENSION_FETCH_TIMEOUT_MAX_MS: u64 = 30_000;

/// §5.6 `process.spawn` 命令长度上限 (命令必须是裸名称, 见
/// [`run_extension_process`])。
pub const MAX_EXTENSION_COMMAND_LEN: usize = 256;
/// §5.6 `process.spawn` 参数个数上限。
pub const MAX_EXTENSION_ARGUMENTS: usize = 128;
/// §5.6 `process.spawn` 单个参数长度上限。
pub const MAX_EXTENSION_ARG_LEN: usize = 4096;
/// §5.6 `process.spawn` 默认超时; 超时后子进程被杀死 (kill) 并报错。
pub const EXTENSION_PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
/// §5.6 `process.spawn` 超时上限 (毫秒): 扩展传入的 `options.timeout` 封顶于此。
pub const EXTENSION_PROCESS_TIMEOUT_MAX_MS: u64 = 30_000;

/// §5.6 单扩展可注册的 chrome 视图数上限。JS bootstrap 的
/// `registerChromeView` 用它 fail closed——超限视图注册直接抛异常,
/// 一个失控扩展无法让宿主无界累积视图。
pub const MAX_EXTENSION_VIEWS: usize = 32;

/// 环境变量：覆盖内置扩展搜索路径 (平台 PATH 分隔符分隔多个目录)。
pub const BUILTIN_EXTENSIONS_ENV: &str = "Z3RM_EXTENSIONS_DIR";

// ---------------------------------------------------------------------------
// 资源限制配置
// ---------------------------------------------------------------------------

/// spec §5.3 `[resources]` 中声明的每扩展资源上限。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtensionLimits {
    /// 内存上限 (MB)，0 表示不限制。
    pub memory_limit_mb: usize,
    /// CPU 预算 (ms / wall-clock 秒)，0 表示不限制。
    pub cpu_budget_ms: u64,
    /// 宿主调用速率上限 (ops/秒)。
    pub io_rate_limit: f64,
}

impl Default for ExtensionLimits {
    fn default() -> Self {
        Self {
            memory_limit_mb: DEFAULT_MEMORY_LIMIT_MB,
            cpu_budget_ms: CPU_FUEL_BUDGET_MS,
            io_rate_limit: IO_TOKEN_BUCKET_DEFAULT_RATE,
        }
    }
}

impl ExtensionLimits {
    pub fn new(memory_limit_mb: usize, cpu_budget_ms: u64, io_rate_limit: f64) -> Self {
        Self {
            memory_limit_mb,
            cpu_budget_ms,
            io_rate_limit,
        }
    }
}

// ---------------------------------------------------------------------------
// CPU Fuel 中断器
// ---------------------------------------------------------------------------

struct CpuFuelState {
    /// 当前预算窗口的起点。
    window_start: Instant,
    /// 本窗口内已消耗的 JS 执行时间。
    used: Duration,
    /// 上一次中断回调的时刻；`None` 表示刚进入一次新的宿主调用。
    last_checkpoint: Option<Instant>,
    /// 是否因超预算被中断过 (由宿主读取后清零)。
    interrupted: bool,
}

/// CPU fuel 跟踪器: 记录 JS 真实执行时间，超预算时中断 (spec §5.2)。
///
/// QuickJS 只在执行字节码时调用中断回调，因此**相邻两次回调之间的 wall-clock
/// 间隔就是这段时间里真实的 JS 执行时间**。宿主空闲期不会产生回调，
/// 而进入下一次宿主调用前 [`begin_execution`](CpuFuelTracker::begin_execution)
/// 会清掉上一个 checkpoint，避免把空闲时间计入预算。
#[derive(Clone)]
struct CpuFuelTracker {
    state: Arc<Mutex<CpuFuelState>>,
    /// 单窗口预算；`Duration::ZERO` 表示不限制。
    budget: Duration,
}

impl CpuFuelTracker {
    fn new(budget_ms: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(CpuFuelState {
                window_start: Instant::now(),
                used: Duration::ZERO,
                last_checkpoint: None,
                interrupted: false,
            })),
            budget: Duration::from_millis(budget_ms),
        }
    }

    /// 标记一次新的宿主 → JS 调用开始，丢弃上一次的 checkpoint。
    fn begin_execution(&self) {
        self.state.lock().last_checkpoint = None;
    }

    /// 中断检查: 返回 `true` 表示应中断执行。
    fn check(&self) -> bool {
        if self.budget.is_zero() {
            return false;
        }
        let kill_threshold = self
            .budget
            .checked_mul(CPU_OVER_BUDGET_KILL_MULTIPLE)
            .unwrap_or(Duration::MAX);

        let now = Instant::now();
        let mut state = self.state.lock();
        if let Some(previous) = state.last_checkpoint {
            state.used += now.saturating_duration_since(previous);
        }
        state.last_checkpoint = Some(now);

        if state.used >= kill_threshold {
            state.interrupted = true;
            return true;
        }

        if now.saturating_duration_since(state.window_start) >= CPU_FUEL_WINDOW {
            state.window_start = now;
            state.used = Duration::ZERO;
            return false;
        }

        false
    }

    /// 读取并清除「被 CPU 预算中断」标志。
    fn take_interrupted(&self) -> bool {
        let mut state = self.state.lock();
        std::mem::replace(&mut state.interrupted, false)
    }

    /// 当前窗口已消耗的执行时间 (测试与诊断用)。
    #[cfg(test)]
    fn used(&self) -> Duration {
        self.state.lock().used
    }
}

// ---------------------------------------------------------------------------
// IO 令牌桶限流器
// ---------------------------------------------------------------------------

/// IO 操作令牌桶: 控制扩展的宿主调用频率 (spec §5.2 / §5.6)。
///
/// 扩展每次通过 [`HostBridge`] 调用宿主都需消耗令牌。
/// 令牌按固定速率补充，超过容量则丢弃。
pub struct IoTokenBucket {
    rate: f64,
    capacity: f64,
    tokens: Mutex<f64>,
    last_refill: Mutex<Instant>,
    /// `true` 表示不限流 (`io_rate_limit = 0`，与 memory/cpu 的 0 = 不限制约定一致)：
    /// 所有获取请求直接放行，不做任何拒绝。
    unlimited: bool,
}

impl IoTokenBucket {
    /// 创建令牌桶 (spec §5.2 IO rate)
    pub fn new(rate: f64, capacity: f64) -> Self {
        Self {
            rate,
            capacity,
            tokens: Mutex::new(capacity),
            last_refill: Mutex::new(Instant::now()),
            unlimited: false,
        }
    }

    /// 不限流桶：`io_rate_limit = 0` 时使用，任何调用都直接放行。
    pub fn unlimited() -> Self {
        Self {
            rate: 0.0,
            capacity: 0.0,
            tokens: Mutex::new(0.0),
            last_refill: Mutex::new(Instant::now()),
            unlimited: true,
        }
    }

    /// 由 manifest 的 `io_rate_limit` 构造，容量允许 2× 突发。
    ///
    /// `0` 表示不限制 (与 `memory_limit_mb = 0` / `cpu_budget_ms = 0` 同约定)；
    /// 声明 `0` 的扩展不能被静默套上默认速率。非法值 (NaN/负数) 仍回退到默认速率。
    pub fn from_rate(rate: f64) -> Self {
        if rate == 0.0 {
            return Self::unlimited();
        }
        let rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            IO_TOKEN_BUCKET_DEFAULT_RATE
        };
        Self::new(rate, rate * IO_TOKEN_BUCKET_BURST_FACTOR)
    }

    /// 默认配置: 100 tokens/s, 容量 200
    pub fn default_config() -> Self {
        Self::from_rate(IO_TOKEN_BUCKET_DEFAULT_RATE)
    }

    /// 尝试获取 `count` 个令牌。成功返回 `true`。
    pub fn try_acquire(&self, count: f64) -> bool {
        if self.unlimited {
            return true;
        }
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
// §5.6 Capabilities
// ---------------------------------------------------------------------------

/// spec §5.6 文件系统访问范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilesystemAccess {
    #[default]
    None,
    Cwd,
    Home,
}

impl FilesystemAccess {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "none" | "" => Ok(Self::None),
            "cwd" => Ok(Self::Cwd),
            "home" => Ok(Self::Home),
            other => bail!("invalid filesystem capability: {other}"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cwd => "cwd",
            Self::Home => "home",
        }
    }
}

/// spec §5.6 扩展在 `extension.toml` 的 `[capabilities]` 中声明的权限。
/// 未声明的能力一律拒绝 (fail closed)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtensionCapabilities {
    pub terminal: bool,
    pub mux: bool,
    pub workspace: bool,
    pub settings: bool,
    pub network: bool,
    pub process_spawn: bool,
    pub filesystem: FilesystemAccess,
}

impl ExtensionCapabilities {
    /// 全开：仅用于嵌入方自己的测试装载路径，manifest 装载走 fail-closed 解析。
    pub fn all() -> Self {
        Self {
            terminal: true,
            mux: true,
            workspace: true,
            settings: true,
            network: true,
            process_spawn: true,
            filesystem: FilesystemAccess::Home,
        }
    }

    /// 宿主调用是否被允许。方法名形如 `mux.splitPane`，命名空间即能力名。
    pub fn allows(&self, method: &str) -> bool {
        match method.split('.').next().unwrap_or_default() {
            "mux" => self.mux,
            "terminal" => self.terminal,
            "workspace" => self.workspace,
            "settings" => self.settings,
            "network" => self.network,
            "process" => self.process_spawn,
            "filesystem" => self.filesystem != FilesystemAccess::None,
            _ => false,
        }
    }

    /// Host-originated events are gated independently from the JavaScript
    /// subscription helper so direct access to its handler registry cannot
    /// bypass the manifest.
    pub fn allows_host_event(self, event: &str) -> bool {
        match event {
            "pane:output" | "pane:dirty" | "pane:bell" | "shell:integration" => self.terminal,
            "clipboard" => self.mux,
            _ if event.starts_with("pane:")
                || event.starts_with("tab:")
                || event.starts_with("session:")
                || event.starts_with("window:") =>
            {
                self.mux
            }
            _ => true,
        }
    }

    fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "terminal": self.terminal,
            "mux": self.mux,
            "workspace": self.workspace,
            "settings": self.settings,
            "network": self.network,
            "process_spawn": self.process_spawn,
            "filesystem": self.filesystem.as_str(),
        })
    }
}

/// §5.6 把扩展请求的路径约束在声明的文件系统范围内，返回规范化的绝对路径。
///
/// `root` 是声明范围对应的约束根: [`FilesystemAccess::Home`] 传主目录,
/// [`FilesystemAccess::Cwd`] 传宿主权威工作区/当前工作根 (即 `workspace.getPath`
/// 报告的根)。同一个入口服务两个范围, 防止路径约束逻辑漂移。
///
/// - 相对路径锚定到 `root`；
/// - 整条路径 `canonicalize` 解析符号链接——链内任何一环指向范围外即拒绝
///   (规范路径会暴露真实位置)；
/// - 尾段不存在时 (如读取缺失文件) 解析最近存在的父目录再挂回尾段；
/// - 约束判定用 `starts_with` 做组件级比较 (不是前缀字符串比较), 根目录的
///   兄弟目录无法绕过。
///
/// 约束根由调用方 (各宿主桥) 按声明范围解析, 以便服务器/测试注入不同的根。
/// 桥实现共享这个唯一入口, 防止两套路径约束逻辑漂移。
pub fn confine_to_root(root: &Path, path: &str) -> Result<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let canonical = match candidate.canonicalize() {
        Ok(canonical) => canonical,
        // 尾段缺失: 解析最近存在的父目录再挂回尾段, 让读取报 NotFound 而非
        // 误导性的 "path not accessible"。
        Err(_) => match (candidate.parent(), candidate.file_name()) {
            (Some(parent), Some(name)) => parent
                .canonicalize()
                .map(|parent| parent.join(name))
                .unwrap_or(candidate),
            _ => candidate,
        },
    };
    if !canonical.starts_with(&root) {
        bail!("path escapes the declared filesystem scope: {path}");
    }
    Ok(canonical)
}

/// §5.6 `settings.*` 键安全校验 (客户端与服务器桥共用): 非空点分路径、无空段、
/// 总长与段数受限。两套桥共用同一入口, 防止校验逻辑漂移。
pub fn validate_settings_key(key: &str) -> Result<()> {
    if key.trim().is_empty() || key.split('.').any(str::is_empty) {
        bail!("settings key must be a non-empty dotted path");
    }
    if key.len() > MAX_EXTENSION_SETTINGS_KEY_LEN {
        bail!("settings key exceeds {MAX_EXTENSION_SETTINGS_KEY_LEN} bytes");
    }
    if key.split('.').count() > MAX_EXTENSION_SETTINGS_SEGMENTS {
        bail!("settings key exceeds {MAX_EXTENSION_SETTINGS_SEGMENTS} segments");
    }
    Ok(())
}

/// §5.6 解析宿主调用选项里的超时 (毫秒): 必须为正整数, 封顶 `max_ms`;
/// 缺省返回 `default`。`network.fetch` 与 `process.spawn` 共用, 保证两端
/// 行为一致; 0 或负数按 fail closed 拒绝 (不提供「无超时」逃生口)。
pub fn parse_extension_timeout(
    options: &serde_json::Value,
    default: Duration,
    max_ms: u64,
) -> Result<Duration> {
    match options.get("timeout") {
        None => Ok(default),
        Some(serde_json::Value::Number(number)) => match number.as_u64() {
            Some(ms) if ms > 0 => Ok(Duration::from_millis(ms.min(max_ms))),
            _ => bail!("timeout must be a positive number of milliseconds"),
        },
        Some(_) => bail!("timeout must be a number of milliseconds"),
    }
}

/// §5.6 `process.spawn` 有界执行 (客户端与服务器桥共用): 命令/参数受限,
/// 超时杀死子进程并报错, 输出总量有界 (读入时截断到上限 + 1 字节再判定)。
///
/// 命令必须是裸名称 (不含路径分隔符), 经 PATH 解析——fail closed 拒绝
/// 任意绝对路径/相对路径执行, 与 `validate_extension_id` 的目录名规则同源。
/// stdout/stderr 在子进程运行期间由独立线程持续排空；否则子进程写满
/// OS pipe 后会在退出前阻塞，而宿主的轮询永远看不到退出状态。
pub fn run_extension_process(
    command: &str,
    arguments: &[String],
    timeout: Duration,
) -> Result<process::Output> {
    if command.trim().is_empty() {
        bail!("process command must be a non-empty name");
    }
    if command.len() > MAX_EXTENSION_COMMAND_LEN {
        bail!("process command exceeds {MAX_EXTENSION_COMMAND_LEN} bytes");
    }
    if command.contains('/') || command.contains('\\') {
        bail!("process command must be a bare name without path separators");
    }
    if arguments.len() > MAX_EXTENSION_ARGUMENTS {
        bail!("process arguments exceed {MAX_EXTENSION_ARGUMENTS} entries");
    }
    let mut argument_bytes = 0usize;
    for argument in arguments {
        if argument.len() > MAX_EXTENSION_ARG_LEN {
            bail!("process argument exceeds {MAX_EXTENSION_ARG_LEN} bytes");
        }
        argument_bytes = argument_bytes.saturating_add(argument.len());
    }
    if argument_bytes > MAX_EXTENSION_ARG_LEN * MAX_EXTENSION_ARGUMENTS {
        bail!("process arguments exceed the aggregate byte limit");
    }

    let mut child = process::Command::new(command)
        .args(arguments)
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning process {command}"))?;

    let output_limit = MAX_EXTENSION_FILE_READ as usize;
    let stdout_reader = child.stdout.take().map(|mut output| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            output
                .take(output_limit as u64 + 1)
                .read_to_end(&mut bytes)
                .context("reading process stdout")?;
            Ok(bytes)
        })
    });
    let stderr_reader = child.stderr.take().map(|mut output| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            output
                .take(output_limit as u64 + 1)
                .read_to_end(&mut bytes)
                .context("reading process stderr")?;
            Ok(bytes)
        })
    });

    let join_pipe = |reader: Option<std::thread::JoinHandle<Result<Vec<u8>>>>,
                     stream: &'static str|
     -> Result<Vec<u8>> {
        Ok(reader
            .map(|reader| {
                reader
                    .join()
                    .map_err(|_| anyhow!("process {stream} reader thread panicked"))?
            })
            .transpose()
            .map(|bytes| bytes.unwrap_or_default())?)
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_pipe(stdout_reader, "stdout")?;
                let stderr = join_pipe(stderr_reader, "stderr")?;
                if stdout.len().saturating_add(stderr.len()) > output_limit {
                    bail!("process output exceeds {output_limit} bytes");
                }
                return Ok(process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = join_pipe(stdout_reader, "stdout");
                    let _ = join_pipe(stderr_reader, "stderr");
                    bail!("process `{command}` exceeded the {timeout:?} timeout and was killed");
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipe(stdout_reader, "stdout");
                let _ = join_pipe(stderr_reader, "stderr");
                return Err(error).with_context(|| format!("waiting for process {command}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §16.8 / §5.3 extension.toml manifest
// ---------------------------------------------------------------------------

/// §16.8 扩展运行侧声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionSide {
    Client,
    Server,
    Both,
}

impl ExtensionSide {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "client" => Ok(Self::Client),
            "server" => Ok(Self::Server),
            "both" => Ok(Self::Both),
            other => bail!("invalid extension runtime side: {other}"),
        }
    }

    /// 该侧是否需要在 GUI 客户端加载。
    pub fn runs_on_client(self) -> bool {
        matches!(self, Self::Client | Self::Both)
    }

    /// 该侧是否需要在服务器端 (mux_server / 守护进程) 加载。
    ///
    /// `Both` 同时满足两侧谓词，构成嵌入方选择运行侧的唯一依据：
    /// 客户端用 `runs_on_client()` 过滤，服务端用 `runs_on_server()` 过滤。
    pub fn runs_on_server(self) -> bool {
        matches!(self, Self::Server | Self::Both)
    }
}

/// spec §5.3 解析后的 `extension.toml`。
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub side: ExtensionSide,
    pub sync: bool,
    pub capabilities: ExtensionCapabilities,
    pub limits: ExtensionLimits,
}

fn toml_bool(table: &toml::value::Table, key: &str) -> Result<bool> {
    match table.get(key) {
        None => Ok(false),
        Some(toml::Value::Boolean(value)) => Ok(*value),
        Some(other) => bail!(
            "capability `{key}` must be a boolean, found {}",
            other.type_str()
        ),
    }
}

fn toml_positive_number(table: &toml::value::Table, key: &str) -> Result<Option<f64>> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Integer(value)) if *value >= 0 => Ok(Some(*value as f64)),
        Some(toml::Value::Float(value)) if *value >= 0.0 => Ok(Some(*value)),
        Some(_) => bail!("`{key}` must be a non-negative number"),
    }
}

/// spec §5.3 解析 manifest 文本。`fallback_id` 通常是扩展目录名。
pub fn parse_manifest_str(fallback_id: &str, text: &str) -> Result<ExtensionManifest> {
    let document: toml::Value = text.parse().context("invalid TOML")?;
    let root = document
        .as_table()
        .context("extension manifest must be a table")?;

    // Zed 格式把 name/version 放在顶层，z3rm 格式放在 `[extension]`；两者都接受。
    let extension = root.get("extension").and_then(toml::Value::as_table);
    let lookup = |key: &str| -> Option<&toml::Value> {
        extension
            .and_then(|table| table.get(key))
            .or_else(|| root.get(key))
    };

    let id = lookup("id")
        .and_then(toml::Value::as_str)
        .unwrap_or(fallback_id)
        .to_string();
    let name = lookup("name")
        .and_then(toml::Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let version = lookup("version")
        .and_then(toml::Value::as_str)
        .unwrap_or("0.0.0")
        .to_string();

    let runtime = root
        .get("runtime")
        .and_then(toml::Value::as_table)
        .context("extension manifest missing [runtime] section")?;
    let side = runtime
        .get("side")
        .and_then(toml::Value::as_str)
        .context("extension manifest missing [runtime] side")?;
    let side = ExtensionSide::parse(side)?;
    let sync = runtime
        .get("sync")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);

    // §5.6 fail closed: 缺失或非表格形式的 [capabilities] 一律不授予任何权限。
    // (Zed 的旧 manifest 用 `[[capabilities]]` 数组表达 process:exec，语义不同。)
    let capabilities = match root.get("capabilities") {
        Some(toml::Value::Table(table)) => ExtensionCapabilities {
            terminal: toml_bool(table, "terminal")?,
            mux: toml_bool(table, "mux")?,
            workspace: toml_bool(table, "workspace")?,
            settings: toml_bool(table, "settings")?,
            network: toml_bool(table, "network")?,
            process_spawn: toml_bool(table, "process_spawn")?,
            filesystem: match table.get("filesystem") {
                None => FilesystemAccess::None,
                Some(toml::Value::String(value)) => FilesystemAccess::parse(value)?,
                Some(toml::Value::Boolean(false)) => FilesystemAccess::None,
                Some(_) => bail!("`filesystem` capability must be a string"),
            },
        },
        _ => ExtensionCapabilities::default(),
    };

    let defaults = ExtensionLimits::default();
    let resources = root.get("resources").and_then(toml::Value::as_table);
    let resource_value = |key: &str| -> Result<Option<f64>> {
        if let Some(table) = resources
            && let Some(value) = toml_positive_number(table, key)?
        {
            return Ok(Some(value));
        }
        toml_positive_number(root, key)
    };
    let limits = ExtensionLimits {
        memory_limit_mb: resource_value("memory_limit_mb")?
            .map(|value| value as usize)
            .unwrap_or(defaults.memory_limit_mb),
        cpu_budget_ms: resource_value("cpu_budget_ms")?
            .map(|value| value as u64)
            .unwrap_or(defaults.cpu_budget_ms),
        io_rate_limit: resource_value("io_rate_limit")?.unwrap_or(defaults.io_rate_limit),
    };

    Ok(ExtensionManifest {
        id,
        name,
        version,
        side,
        sync,
        capabilities,
        limits,
    })
}

impl ExtensionManifest {
    /// §5.6 Canonical policy fingerprint: the exact serialized policy tuple
    /// this manifest's approval covers — id, version, runtime side,
    /// capabilities and resource limits — as canonical JSON (objects built
    /// from `BTreeMap`, so key order is deterministic). Fingerprints are
    /// never hashed, so they cannot collide: two manifests share a
    /// fingerprint iff their entire policy tuple is byte-identical, and any
    /// change invalidates the prior approval.
    ///
    /// This is the single source of truth for both embedding sides: the GUI
    /// client's consent store and the daemon's server-extension approval
    /// ledger must compute byte-identical fingerprints so one approval
    /// format covers both. `serde_json::Value` numbers format like the
    /// client store expects (integers without a fractional part, f64 with
    /// `serde_json`'s default shortest representation).
    pub fn policy_fingerprint(&self) -> String {
        let payload = serde_json::json!({
            "id": self.id,
            "version": self.version,
            "side": manifest_side_name(self.side),
            "capabilities": manifest_capabilities_json(&self.capabilities),
            "limits": manifest_limits_json(&self.limits),
        });
        payload.to_string()
    }
}

/// Canonical side name used by [`ExtensionManifest::policy_fingerprint`].
fn manifest_side_name(side: ExtensionSide) -> &'static str {
    match side {
        ExtensionSide::Client => "client",
        ExtensionSide::Server => "server",
        ExtensionSide::Both => "both",
    }
}

/// Canonical JSON for the capability tuple of a manifest. Field order is
/// fixed by the literal and the shape must not drift from the client consent
/// store format (approval records outlive code changes).
fn manifest_capabilities_json(capabilities: &ExtensionCapabilities) -> serde_json::Value {
    serde_json::json!({
        "terminal": capabilities.terminal,
        "mux": capabilities.mux,
        "workspace": capabilities.workspace,
        "settings": capabilities.settings,
        "network": capabilities.network,
        "process_spawn": capabilities.process_spawn,
        "filesystem": capabilities.filesystem.as_str(),
    })
}

/// Canonical JSON for the resource-limit tuple of a manifest.
fn manifest_limits_json(limits: &ExtensionLimits) -> serde_json::Value {
    serde_json::json!({
        "memory_limit_mb": limits.memory_limit_mb,
        "cpu_budget_ms": limits.cpu_budget_ms,
        "io_rate_limit": limits.io_rate_limit,
    })
}

/// spec §5.3 从磁盘读取并解析 `extension.toml`。
pub fn parse_manifest(path: &Path) -> Result<ExtensionManifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let fallback_id = path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    parse_manifest_str(&fallback_id, &text).with_context(|| format!("parsing {}", path.display()))
}

// ---------------------------------------------------------------------------
// §5.5 内置扩展发现
// ---------------------------------------------------------------------------

/// 一个已发现、可加载的扩展。
#[derive(Debug, Clone)]
pub struct DiscoveredExtension {
    pub manifest: ExtensionManifest,
    pub directory: PathBuf,
    pub source: String,
}

/// spec §5.5 内置扩展的搜索根目录，按优先级排列。
///
/// 内置扩展**不会**被拷贝进用户的 extensions 目录：拷贝会在升级时留下过期副本，
/// 而且启动时写盘失败会波及主程序启动 (§15.7 要求核心命令不受扩展宿主影响)。
/// 改为直接把仓库/安装包里的 `extensions/` 作为额外扫描根；同名 id 由用户目录
/// 覆盖，从而保留 §5.5 的「用户可 fork 内置扩展」语义。
pub fn builtin_extension_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let push = |candidate: PathBuf, roots: &mut Vec<PathBuf>| {
        if candidate.is_dir() && !roots.contains(&candidate) {
            roots.push(candidate);
        }
    };

    if let Some(value) = std::env::var_os(BUILTIN_EXTENSIONS_ENV) {
        for candidate in std::env::split_paths(&value) {
            push(candidate, &mut roots);
        }
    }

    match std::env::current_exe() {
        Ok(executable) => {
            if let Some(directory) = executable.parent() {
                push(directory.join("extensions"), &mut roots);
                // macOS .app bundle: Contents/MacOS/z3rm → Contents/Resources/extensions
                push(directory.join("../Resources/extensions"), &mut roots);
                push(directory.join("../lib/z3rm/extensions"), &mut roots);
            }
        }
        Err(error) => {
            tracing::debug!(%error, "could not resolve executable path for extension discovery");
        }
    }

    // 源码检出 (开发构建)。发布产物里这个绝对路径不存在，`is_dir` 会自然跳过。
    if let Some(repository_root) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
    {
        push(repository_root.join("extensions"), &mut roots);
    }

    roots
}

/// 用户安装目录 + 内置目录，用户目录优先 (同 id 覆盖内置)。
pub fn extension_roots(user_extensions_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![user_extensions_dir.to_path_buf()];
    for root in builtin_extension_roots() {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// 扫描给定根目录，按运行侧谓词过滤扩展 (spec §5.2 / §5.3 / §16.8)。
///
/// 缺少 `main.js` 或 `extension.toml` 的目录、不属于该侧的扩展、解析失败的
/// manifest 都会被跳过并记录日志——一个坏扩展不能阻断其它扩展的加载。
fn discover_extensions_for(
    roots: &[PathBuf],
    include: impl Fn(ExtensionSide) -> bool,
) -> Vec<DiscoveredExtension> {
    let mut discovered: BTreeMap<String, DiscoveredExtension> = BTreeMap::new();

    for root in roots {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::debug!(path = %root.display(), %error, "extension root not readable");
                continue;
            }
        };

        let mut directories: Vec<PathBuf> = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => directories.push(entry.path()),
                Err(error) => {
                    tracing::warn!(path = %root.display(), %error, "failed to read extension directory entry")
                }
            }
        }
        // read_dir 顺序依赖文件系统；排序让加载顺序（以及渲染顺序）稳定。
        directories.sort();

        for directory in directories {
            let manifest_path = directory.join("extension.toml");
            let source_path = directory.join("main.js");
            if !manifest_path.is_file() || !source_path.is_file() {
                continue;
            }

            let manifest = match parse_manifest(&manifest_path) {
                Ok(manifest) => manifest,
                Err(error) => {
                    tracing::warn!(path = %manifest_path.display(), error = %error, "extension manifest rejected");
                    continue;
                }
            };
            if !include(manifest.side) {
                continue;
            }
            if discovered.contains_key(&manifest.id) {
                // 前面的根优先：用户安装的 fork 覆盖同名内置扩展。
                continue;
            }
            let source = match std::fs::read_to_string(&source_path) {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(path = %source_path.display(), %error, "extension source unreadable");
                    continue;
                }
            };
            discovered.insert(
                manifest.id.clone(),
                DiscoveredExtension {
                    manifest,
                    directory,
                    source,
                },
            );
        }
    }

    discovered.into_values().collect()
}

/// 扫描给定根目录，返回所有可在客户端加载的扩展 (spec §5.2 / §16.8)。
///
/// `side = "client"` 与 `side = "both"` 的扩展在此侧加载；`side = "server"`
/// 的扩展由 [`discover_server_extensions`] 接管，不会被客户端执行。
pub fn discover_client_extensions(roots: &[PathBuf]) -> Vec<DiscoveredExtension> {
    discover_extensions_for(roots, |side| side.runs_on_client())
}

/// 扫描给定根目录，返回所有可在服务器端加载的扩展 (spec §16.8)。
///
/// `side = "server"` 与 `side = "both"` 的扩展在此侧加载；`side = "client"`
/// 的扩展只属于 GUI 进程，不会被服务器端执行。嵌入方 (mux_server) 用这条路径
/// 做服务端发现；目前 daemon 尚无 extension host，安装请求会显式报错，绝不静默
/// 接受一个声称要跑在服务端的扩展。
pub fn discover_server_extensions(roots: &[PathBuf]) -> Vec<DiscoveredExtension> {
    discover_extensions_for(roots, |side| side.runs_on_server())
}

// ---------------------------------------------------------------------------
// §5.4 宿主桥
// ---------------------------------------------------------------------------

/// spec §5.4 扩展 JS 可以调用的宿主能力。
///
/// QuickJS 跑在专用 OS 线程上，不能持有 GPUI 的 `Entity`，因此 mux/settings/
/// terminal 调用统一走这条 JSON 通道，由嵌入方在宿主线程上同步执行。
/// 实现应当自带超时——扩展线程阻塞只影响它自己 (spec §5.2)。
pub trait HostBridge: Send + Sync + 'static {
    /// `method` 形如 `mux.splitPane`；`args` 是位置参数数组。
    fn call(&self, method: &str, args: &serde_json::Value) -> Result<serde_json::Value>;
}

fn host_call_response(
    bridge: &Arc<dyn HostBridge>,
    capabilities: ExtensionCapabilities,
    io_bucket: &IoTokenBucket,
    io_violated: &AtomicBool,
    method: &str,
    arguments_json: &str,
) -> serde_json::Value {
    let outcome = (|| -> Result<serde_json::Value> {
        // §5.6 运行时能力强制：JS 侧的检查只是提前失败，这里才是权威判定。
        if !capabilities.allows(method) {
            bail!("capability denied: `{method}` requires an undeclared capability");
        }
        if !io_bucket.try_acquire(1.0) {
            // §5.6 拒绝发生在 Rust 侧，JS 可能 catch 掉异常；持久标志是宿主
            // 判定违规并挂起扩展的唯一可靠信号。
            io_violated.store(true, Ordering::Relaxed);
            bail!("io rate limit exceeded while calling `{method}`");
        }
        let arguments: serde_json::Value = serde_json::from_str(arguments_json)
            .with_context(|| format!("invalid arguments for `{method}`"))?;
        bridge.call(method, &arguments)
    })();

    match outcome {
        Ok(value) => serde_json::json!({ "ok": true, "value": value }),
        Err(error) => serde_json::json!({ "ok": false, "error": error.to_string() }),
    }
}

/// 把 `__z3rm_host_call` 注入到 JS 全局，供 bootstrap 的 `hostCall` 使用。
fn install_host_call(
    ctx: &rquickjs::Ctx<'_>,
    bridge: Arc<dyn HostBridge>,
    capabilities: ExtensionCapabilities,
    io_bucket: Arc<IoTokenBucket>,
    io_violated: Arc<AtomicBool>,
) -> Result<()> {
    let function = Function::new(ctx.clone(), move |method: String, arguments: String| {
        host_call_response(
            &bridge,
            capabilities,
            &io_bucket,
            &io_violated,
            &method,
            &arguments,
        )
        .to_string()
    })
    .catch(ctx)
    .map_err(|error| anyhow!("creating __z3rm_host_call failed: {error}"))?;

    ctx.globals()
        .set("__z3rm_host_call", function)
        .catch(ctx)
        .map_err(|error| anyhow!("installing __z3rm_host_call failed: {error}"))
}

// ---------------------------------------------------------------------------
// §5.4 JS bootstrap
// ---------------------------------------------------------------------------

/// spec §5.4 扩展 context。占位符 `__Z3RM_CAPABILITIES__` 由 manifest 声明填充。
const CONTEXT_BOOTSTRAP_JS: &str = r#"
(function() {
    var capabilities = __Z3RM_CAPABILITIES__;
    var maxChromeViews = __Z3RM_MAX_EXTENSION_VIEWS__;

    globalThis.__chrome_views = {};
    globalThis.__z3rm_view_order = [];
    globalThis.__z3rm_rerender = true;
    globalThis.__z3rm_event_handlers = {};
    globalThis.__z3rm_mux_sessions = [];
    globalThis.__z3rm_commands = {};
    globalThis.__z3rm_command_order = [];
    globalThis.__z3rm_keymaps = {};
    globalThis.__z3rm_errors = [];
    globalThis.__z3rm_render_result = null;

    function recordError(where, error) {
        var message;
        if (error === null || error === undefined) {
            // QuickJS 在内存耗尽时可能连异常对象本身都构造不出来，抛出的
            // 值会是 null/undefined 而不是带消息的 Error。把这种情况归一为
            // "out of memory"，宿主据此挂起扩展 (spec §5.6)，而不是当成普通
            // JS 错误放过。
            message = 'out of memory';
        } else if (error.message) {
            message = error.message;
        } else {
            message = String(error);
        }
        // Bounded so a misbehaving handler cannot grow the heap without limit.
        if (globalThis.__z3rm_errors.length < 64) {
            globalThis.__z3rm_errors.push(where + ': ' + message);
        }
    }
    globalThis.__z3rm_take_errors = function() {
        var errors = globalThis.__z3rm_errors;
        globalThis.__z3rm_errors = [];
        return JSON.stringify(errors);
    };

    function capabilityGranted(name) {
        if (name === 'filesystem') {
            return !!capabilities.filesystem && capabilities.filesystem !== 'none';
        }
        return !!capabilities[name];
    }
    function requireCapability(name) {
        if (!capabilityGranted(name)) {
            throw new Error('capability "' + name + '" is not declared in extension.toml');
        }
    }

    function hostBridgeAvailable() {
        return typeof globalThis.__z3rm_host_call === 'function';
    }
    function hostCall(method, args) {
        if (!hostBridgeAvailable()) {
            throw new Error('host bridge unavailable for ' + method);
        }
        var response = JSON.parse(globalThis.__z3rm_host_call(method, JSON.stringify(args || [])));
        if (!response.ok) {
            throw new Error(response.error || ('host call failed: ' + method));
        }
        return response.value;
    }

    function subscribe(prefix, event, handler) {
        if (typeof handler !== 'function') {
            throw new Error('subscribe requires a handler function for ' + event);
        }
        var key = prefix + event;
        var registry = globalThis.__z3rm_event_handlers;
        if (!registry[key]) { registry[key] = []; }
        registry[key].push(handler);
        return function unsubscribe() {
            var handlers = registry[key] || [];
            var index = handlers.indexOf(handler);
            if (index >= 0) { handlers.splice(index, 1); }
        };
    }

    function invalidate() { globalThis.__z3rm_rerender = true; }

    // Host-callable entry points -------------------------------------------
    globalThis.__z3rm_dispatch_event = function(name, payload) {
        // A subscription made through mux/keymaps/terminal is stored under a
        // namespaced key; the host emits the bare event name, so try each.
        var keys = [name];
        var prefixes = ['mux:', 'keymap:', 'terminal:'];
        for (var p = 0; p < prefixes.length; p++) {
            if (name.indexOf(prefixes[p]) !== 0) { keys.push(prefixes[p] + name); }
        }
        var registry = globalThis.__z3rm_event_handlers || {};
        var delivered = 0;
        for (var k = 0; k < keys.length; k++) {
            var handlers = registry[keys[k]] || [];
            for (var i = 0; i < handlers.length; i++) {
                try { handlers[i](payload); delivered++; }
                catch (error) { recordError('event ' + keys[k], error); }
            }
        }
        return delivered;
    };

    globalThis.__z3rm_execute_command = function(id, args) {
        var entry = globalThis.__z3rm_commands[id];
        if (!entry) { return false; }
        try { entry.handler(args); return true; }
        catch (error) { recordError('command ' + id, error); return false; }
    };

    function attachDisplayLists(value, view) {
        if (value === null || value === undefined) { return; }
        if (Array.isArray(value)) {
            for (var i = 0; i < value.length; i++) {
                attachDisplayLists(value[i], view);
            }
            return;
        }
        if (typeof value !== 'object') { return; }
        if (value.type === 'display-list' && value.props
            && typeof value.props.renderer === 'string') {
            var renderer = view[value.props.renderer];
            if (typeof renderer !== 'function') {
                recordError('display-list', 'missing renderer ' + value.props.renderer);
            } else {
                value.props.drawOps = renderer.call(view);
            }
        }
        if (Array.isArray(value.children)) {
            attachDisplayLists(value.children, view);
        }
    }

    globalThis.__z3rm_render_views = function() {
        globalThis.__z3rm_rerender = false;
        var views = globalThis.__chrome_views || {};
        var results = [];
        if (globalThis.__z3rm_render_result !== null && globalThis.__z3rm_render_result !== undefined) {
            results.push(globalThis.__z3rm_render_result);
        }
        // status-bar renders first so the chrome ordering stays stable.
        var names = [];
        if (views['status-bar']) { names.push('status-bar'); }
        var order = globalThis.__z3rm_view_order || [];
        for (var i = 0; i < order.length; i++) {
            if (order[i] !== 'status-bar') { names.push(order[i]); }
        }
        for (var n = 0; n < names.length; n++) {
            var view = views[names[n]];
            if (!view || typeof view.render !== 'function') { continue; }
            try {
                var rendered = view.render();
                if (rendered !== null && rendered !== undefined) {
                    if (typeof rendered === 'object' && !Array.isArray(rendered)
                        && (rendered.id === undefined || rendered.id === null)) {
                        rendered.id = names[n];
                    }
                    attachDisplayLists(rendered, view);
                    // §5.4 keep the last rendered VDOM so the display-list
                    // refresh entry can re-invoke only renderer methods
                    // without re-running render().
                    view.__z3rm_last_render = rendered;
                    results.push(rendered);
                }
            } catch (error) { recordError('render ' + names[n], error); }
        }
        return JSON.stringify(results);
    };

    // §5.4 Re-invoke only the display-list renderer methods on the last
    // rendered chrome. Unlike __z3rm_render_views this never re-runs view
    // render() bodies and never touches the invalidation flag, so a ticking
    // clock refreshes its draw ops without invalidating the surrounding VDOM.
    // One renderer throwing drops just its region; other regions still tick.
    function refreshDisplayLists(value, view, out) {
        if (value === null || value === undefined) { return; }
        if (Array.isArray(value)) {
            for (var i = 0; i < value.length; i++) {
                refreshDisplayLists(value[i], view, out);
            }
            return;
        }
        if (typeof value !== 'object') { return; }
        if (value.type === 'display-list' && value.props
            && typeof value.props.renderer === 'string') {
            var renderer = view[value.props.renderer];
            if (typeof renderer !== 'function') {
                recordError('display-list', 'missing renderer ' + value.props.renderer);
            } else {
                try {
                    value.props.drawOps = renderer.call(view);
                    out.push({ region: value.props.id || '', ops: value.props.drawOps });
                } catch (error) {
                    recordError('display-list ' + value.props.id, error);
                }
            }
        }
        if (Array.isArray(value.children)) {
            refreshDisplayLists(value.children, view, out);
        }
    }

    globalThis.__z3rm_refresh_display_lists = function() {
        var views = globalThis.__chrome_views || {};
        var results = [];
        var names = Object.keys(views);
        for (var i = 0; i < names.length; i++) {
            var view = views[names[i]];
            if (!view || !view.__z3rm_last_render) { continue; }
            refreshDisplayLists(view.__z3rm_last_render, view, results);
        }
        return JSON.stringify(results);
    };

    globalThis.__z3rm_list_commands = function() {
        return JSON.stringify(globalThis.__z3rm_command_order.map(function(id) {
            return { id: id, command: globalThis.__z3rm_commands[id].label };
        }));
    };

    globalThis.__z3rm_list_keymaps = function() {
        return JSON.stringify(Object.keys(globalThis.__z3rm_keymaps).map(function(chord) {
            return { chord: chord, command: globalThis.__z3rm_keymaps[chord] };
        }));
    };

    // Extension-facing context ---------------------------------------------
    var context = {
        capabilities: capabilities,

        render: function(vdom) {
            globalThis.__z3rm_render_result = vdom;
            invalidate();
            return vdom;
        },

        registerChromeView: function(name, view) {
            if (typeof name !== 'string' || name.length === 0) {
                throw new Error('registerChromeView requires a non-empty name');
            }
            if (!view) { throw new Error('registerChromeView requires a view: ' + name); }
            if (!globalThis.__chrome_views[name]
                && globalThis.__z3rm_view_order.length >= maxChromeViews) {
                throw new Error('registerChromeView limit exceeded: ' + maxChromeViews);
            }
            if (typeof view.invalidate !== 'function') { view.invalidate = invalidate; }
            if (!globalThis.__chrome_views[name]) { globalThis.__z3rm_view_order.push(name); }
            globalThis.__chrome_views[name] = view;
            invalidate();
            return view;
        },

        on: function(event, handler) { return subscribe('', event, handler); },
        emit: function(event, data) { return globalThis.__z3rm_dispatch_event(event, data); },

        mux: {
            subscribe: function(event, handler) {
                requireCapability('mux');
                return subscribe('mux:', event, handler);
            },
            listSessions: function() {
                requireCapability('mux');
                if (!hostBridgeAvailable()) { return globalThis.__z3rm_mux_sessions; }
                globalThis.__z3rm_mux_sessions = hostCall('mux.listSessions', []) || [];
                return globalThis.__z3rm_mux_sessions;
            },
            currentSession: function() {
                requireCapability('mux');
                return hostCall('mux.currentSession', []);
            },
            focusedPane: function() {
                requireCapability('mux');
                return hostCall('mux.focusedPane', []);
            },
            createSession: function(name, cwd) {
                requireCapability('mux');
                return hostCall('mux.createSession', [name, cwd]);
            },
            killSession: function(sessionId) {
                requireCapability('mux');
                return hostCall('mux.killSession', [sessionId]);
            },
            attach: function(sessionId) {
                requireCapability('mux');
                return hostCall('mux.attach', [sessionId]);
            },
            detach: function() {
                requireCapability('mux');
                return hostCall('mux.detach', []);
            },
            focusPane: function(paneId) {
                requireCapability('mux');
                return hostCall('mux.focusPane', [paneId]);
            },
            splitPane: function(direction, paneId) {
                requireCapability('mux');
                return hostCall('mux.splitPane', [direction, paneId]);
            },
            closePane: function(paneId) {
                requireCapability('mux');
                return hostCall('mux.closePane', [paneId]);
            },
            sendInput: function(paneId, data) {
                requireCapability('mux');
                return hostCall('mux.sendInput', [paneId, data]);
            },
            capturePane: function(paneId, lines) {
                requireCapability('mux');
                return hostCall('mux.capturePane', [paneId, lines]);
            },
            resizePane: function(paneId, cols, rows) {
                requireCapability('mux');
                return hostCall('mux.resizePane', [paneId, cols, rows]);
            },
            setPaneTitle: function(paneId, title) {
                requireCapability('mux');
                return hostCall('mux.setPaneTitle', [paneId, title]);
            },
            applyLayout: function(layout) {
                requireCapability('mux');
                return hostCall('mux.applyLayout', [layout]);
            }
        },

        terminal: {
            subscribe: function(event, handler) {
                requireCapability('terminal');
                return subscribe('terminal:', event, handler);
            },
            write: function(paneId, data) {
                requireCapability('terminal');
                return hostCall('terminal.write', [paneId, data]);
            },
            capture: function(paneId, lines) {
                requireCapability('terminal');
                return hostCall('terminal.capture', [paneId, lines]);
            }
        },

        settings: {
            get: function(key) {
                requireCapability('settings');
                return hostCall('settings.get', [key]);
            },
            set: function(key, value) {
                requireCapability('settings');
                return hostCall('settings.set', [key, value]);
            }
        },

        workspace: {
            getPath: function() {
                requireCapability('workspace');
                return hostCall('workspace.getPath', []);
            }
        },

        filesystem: {
            readTextFile: function(path) {
                requireCapability('filesystem');
                return hostCall('filesystem.readTextFile', [path]);
            },
            readDir: function(path) {
                requireCapability('filesystem');
                return hostCall('filesystem.readDir', [path]);
            }
        },

        network: {
            fetch: function(url, options) {
                requireCapability('network');
                return hostCall('network.fetch', [url, options]);
            }
        },

        process: {
            spawn: function(command, args, options) {
                requireCapability('process_spawn');
                return hostCall('process.spawn', options === undefined
                    ? [command, args]
                    : [command, args, options]);
            }
        },

        commands: {
            register: function(id, handler, options) {
                if (typeof handler !== 'function') {
                    throw new Error('command handler must be a function: ' + id);
                }
                var label = (options && options.label) ? options.label : id;
                globalThis.__z3rm_commands[id] = { handler: handler, label: label };
                if (globalThis.__z3rm_command_order.indexOf(id) < 0) {
                    globalThis.__z3rm_command_order.push(id);
                }
                return true;
            },
            unregister: function(id) {
                if (!globalThis.__z3rm_commands[id]) { return false; }
                delete globalThis.__z3rm_commands[id];
                var index = globalThis.__z3rm_command_order.indexOf(id);
                if (index >= 0) { globalThis.__z3rm_command_order.splice(index, 1); }
                return true;
            },
            list: function() {
                return globalThis.__z3rm_command_order.map(function(id) {
                    return { id: id, label: globalThis.__z3rm_commands[id].label };
                });
            },
            execute: function(id, args) {
                return globalThis.__z3rm_execute_command(id, args);
            }
        },

        keymaps: {
            bind: function(chord, command) {
                if (!chord || !command) {
                    throw new Error('keymaps.bind requires a chord and a command id');
                }
                globalThis.__z3rm_keymaps[chord] = command;
                return true;
            },
            unbind: function(chord) {
                if (!(chord in globalThis.__z3rm_keymaps)) { return false; }
                delete globalThis.__z3rm_keymaps[chord];
                return true;
            },
            list: function() {
                return Object.keys(globalThis.__z3rm_keymaps).map(function(chord) {
                    return { chord: chord, command: globalThis.__z3rm_keymaps[chord] };
                });
            },
            subscribe: function(event, handler) {
                return subscribe('keymap:', event, handler);
            }
        }
    };

    // Host-pushed session snapshots keep listSessions() useful even when the
    // bridge is not installed yet (startup races the mux connection).
    context.on('mux:sessions', function(sessions) {
        globalThis.__z3rm_mux_sessions = sessions || [];
    });

    globalThis.__z3rm_context = context;
    return true;
})()
"#;

/// 调用扩展的 `activate(context)`。异常向 Rust 传播，装载即失败。
const ACTIVATE_JS: &str = r#"
(function() {
    if (typeof activate !== 'function') { return false; }
    activate(globalThis.__z3rm_context);
    return true;
})()
"#;

fn bootstrap_source(capabilities: ExtensionCapabilities) -> Result<String> {
    let capabilities_json = serde_json::to_string(&capabilities.to_json())
        .context("serializing extension capabilities")?;
    Ok(CONTEXT_BOOTSTRAP_JS
        .replace("__Z3RM_CAPABILITIES__", &capabilities_json)
        .replace("__Z3RM_MAX_EXTENSION_VIEWS__", &MAX_EXTENSION_VIEWS.to_string()))
}

/// §5.2 QuickJS `eval` 执行的是脚本而非 ES 模块，先剥掉 `export` 关键字。
fn to_script_source(source: &str) -> String {
    source
        .replace("export function", "function")
        .replace("export const", "const")
        .replace("export default", "const __default =")
}

/// 求值并把 QuickJS 异常展开成带真实消息的 `anyhow::Error`。
/// 直接用 `?` 只会得到 "Exception generated by quickjs"，排查扩展问题时毫无信息。
fn eval_checked<'js, V>(ctx: &rquickjs::Ctx<'js>, source: &str) -> Result<V>
where
    V: rquickjs::FromJs<'js>,
{
    ctx.eval::<V, _>(source)
        .catch(ctx)
        .map_err(|error| anyhow!("{error}"))
}

// ---------------------------------------------------------------------------
// QuickJsRuntime
// ---------------------------------------------------------------------------

/// QuickJS 运行时实例，带资源限制。
///
/// 每个扩展拥有独立的 Runtime + Context，运行在专用 OS 线程中。
///
/// # 资源限制
/// - CPU: 50ms/秒 fuel 预算，连续超支 3 次后中断
/// - 内存: 64MB (默认)，超限抛出 JS 异常
/// - IO: 令牌桶限流，控制宿主调用频率
///
/// # 线程隔离
/// 扩展 JS 代码在专用 `std::thread` 中执行，与主 UI 线程隔离。
pub struct QuickJsRuntime {
    runtime: Runtime,
    limits: ExtensionLimits,
    /// IO 令牌桶 (Arc 以便注入到 host bridge 闭包)
    io_bucket: Arc<IoTokenBucket>,
    /// §5.6 置位表示某次宿主调用被 IO 速率上限拒绝 (令牌耗尽)。与
    /// `memory_violated` 同语义: JS 侧 try/catch 吞掉异常也无法隐藏违规,
    /// 宿主通过 [`take_io_violated`](Self::take_io_violated) 读取并清零。
    io_violated: Arc<AtomicBool>,
    cpu_tracker: CpuFuelTracker,
}

impl QuickJsRuntime {
    /// 创建新的 QuickJS 运行时 (spec §5.2)，IO 速率取默认值。
    pub fn new(memory_limit_mb: usize, cpu_budget_ms: u64) -> Result<Self> {
        Self::with_limits(ExtensionLimits {
            memory_limit_mb,
            cpu_budget_ms,
            ..ExtensionLimits::default()
        })
    }

    /// 按完整的 `[resources]` 配置创建运行时。
    pub fn with_limits(limits: ExtensionLimits) -> Result<Self> {
        let runtime = Runtime::new().context("创建 QuickJS Runtime 失败")?;

        // 内存限制 (spec §5.2: 64MB per extension)
        if limits.memory_limit_mb > 0 {
            let memory_limit_bytes = limits
                .memory_limit_mb
                .checked_mul(1024 * 1024)
                .context("extension memory limit is too large")?;
            runtime.set_memory_limit(memory_limit_bytes);
        }

        // CPU fuel 中断器 (spec §5.2: 50ms/second budget)
        let cpu_tracker = CpuFuelTracker::new(limits.cpu_budget_ms);
        runtime.set_interrupt_handler(Some(Box::new({
            let cpu_tracker = cpu_tracker.clone();
            move || cpu_tracker.check()
        })));

        Ok(Self {
            runtime,
            limits,
            io_bucket: Arc::new(IoTokenBucket::from_rate(limits.io_rate_limit)),
            io_violated: Arc::new(AtomicBool::new(false)),
            cpu_tracker,
        })
    }

    /// 使用默认配置创建运行时: 64MB 内存, 50ms CPU budget
    pub fn with_defaults() -> Result<Self> {
        Self::with_limits(ExtensionLimits::default())
    }

    /// 创建新的 JS 执行上下文
    pub fn create_context(&self) -> Result<Context> {
        Context::full(&self.runtime).map_err(|e| anyhow!("创建 Context 失败: {e}"))
    }
    /// 获取 IO 令牌桶引用
    pub fn io_bucket(&self) -> &Arc<IoTokenBucket> {
        &self.io_bucket
    }

    /// IO 违规标志引用 (Arc 以便注入到 host bridge 闭包)。
    pub fn io_violated(&self) -> &Arc<AtomicBool> {
        &self.io_violated
    }

    /// 读取并清除「宿主调用被 IO 速率上限拒绝」标志 (spec §5.6)。
    pub fn take_io_violated(&self) -> bool {
        self.io_violated.swap(false, Ordering::Relaxed)
    }

    pub fn limits(&self) -> ExtensionLimits {
        self.limits
    }

    /// 进入一次新的宿主 → JS 调用；宿主空闲时间不计入 CPU 预算。
    pub fn begin_execution(&self) {
        self.cpu_tracker.begin_execution();
    }

    /// 读取并清除「上一次执行被 CPU 预算中断」标志。
    pub fn take_cpu_interrupted(&self) -> bool {
        self.cpu_tracker.take_interrupted()
    }

    /// 当前堆占用是否已达到声明的内存上限。
    pub fn memory_exceeded(&self) -> bool {
        if self.limits.memory_limit_mb == 0 {
            return false;
        }
        let limit_bytes = (self.limits.memory_limit_mb as u64).saturating_mul(1024 * 1024);
        let malloc_size = self.runtime.memory_usage().malloc_size as u64;
        malloc_size >= limit_bytes
    }

    /// 在专用线程中创建独立运行时并执行 JS 代码 (spec §5.2: dedicated OS thread)
    ///
    /// QuickJS Runtime/Context 不可跨线程共享 (非 Send)。
    /// 此方法在子线程内创建全新的 Runtime + Context，执行完成后销毁。
    /// 资源限制沿用本实例的配置。
    pub fn execute_in_thread<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(rquickjs::Ctx<'_>) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let limits = self.limits;

        let join_handle = thread::Builder::new()
            .name("quickjs-ext".to_string())
            .spawn(move || {
                let runtime = QuickJsRuntime::with_limits(limits)?;
                let ctx = runtime.create_context()?;
                runtime.begin_execution();
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
        self.begin_execution();
        ctx.with(|ctx| eval_checked::<String>(&ctx, source))
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

struct LoadOutcome {
    vdom_json: Result<Option<String>>,
    cpu_exhausted: bool,
    memory_exceeded: bool,
}

/// 扩展加载器: 在独立线程中加载并执行扩展
pub struct ExtensionRunner {
    limits: ExtensionLimits,
    capabilities: ExtensionCapabilities,
    bridge: Option<Arc<dyn HostBridge>>,
}

impl ExtensionRunner {
    /// 创建扩展加载器。
    ///
    /// 这个构造函数面向嵌入方自己的测试装载路径，默认授予全部能力；
    /// 真实扩展请走 [`for_manifest`](Self::for_manifest)，那条路径按
    /// `[capabilities]` 声明 fail-closed。
    pub fn new(memory_limit_mb: usize, cpu_budget_ms: u64) -> Self {
        Self::with_limits(ExtensionLimits {
            memory_limit_mb,
            cpu_budget_ms,
            ..ExtensionLimits::default()
        })
    }

    pub fn with_limits(limits: ExtensionLimits) -> Self {
        Self {
            limits,
            capabilities: ExtensionCapabilities::all(),
            bridge: None,
        }
    }

    /// 默认配置: 64MB 内存, 50ms CPU
    pub fn with_defaults() -> Self {
        Self::with_limits(ExtensionLimits::default())
    }

    /// §5.3/§5.6 按 manifest 的资源上限和能力声明构造加载器。
    pub fn for_manifest(manifest: &ExtensionManifest) -> Self {
        Self {
            limits: manifest.limits,
            capabilities: manifest.capabilities,
            bridge: None,
        }
    }

    pub fn with_capabilities(mut self, capabilities: ExtensionCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// §5.4 注入宿主桥；缺省时 `context.mux.*` 等调用会抛出可读异常而非返回假值。
    pub fn with_bridge(mut self, bridge: Arc<dyn HostBridge>) -> Self {
        self.bridge = Some(bridge);
        self
    }

    pub fn capabilities(&self) -> ExtensionCapabilities {
        self.capabilities
    }

    /// 加载并激活一个扩展 (spec §5.2: dedicated OS thread isolation)
    ///
    /// 运行时在 activate 之后立即销毁；需要保持 chrome 存活请用
    /// [`load_live`](Self::load_live)。
    pub fn load_extension(
        &self,
        extension_id: &str,
        source: &str,
        _activate_fn: &str,
    ) -> ExtensionRunResult {
        let start = Instant::now();
        let limits = self.limits;
        let capabilities = self.capabilities;
        let bridge = self.bridge.clone();
        let source = source.to_string();
        let join = thread::Builder::new()
            .name("quickjs-ext-load".to_string())
            .spawn(move || {
                ExtensionRunner {
                    limits,
                    capabilities,
                    bridge,
                }
                .do_load(&source)
            });
        let outcome = match join {
            Ok(handle) => match handle.join() {
                Ok(outcome) => outcome,
                Err(error) => LoadOutcome {
                    vdom_json: Err(anyhow!("extension thread panicked: {error:?}")),
                    cpu_exhausted: false,
                    memory_exceeded: false,
                },
            },
            Err(error) => LoadOutcome {
                vdom_json: Err(anyhow!("creating extension thread failed: {error}")),
                cpu_exhausted: false,
                memory_exceeded: false,
            },
        };
        let duration = start.elapsed();
        let (result, vdom_json) = match outcome.vdom_json {
            Ok(vdom_json) => (Ok(()), vdom_json),
            Err(error) => (Err(error), None),
        };

        ExtensionRunResult {
            extension_id: extension_id.to_string(),
            result,
            duration,
            cpu_exhausted: outcome.cpu_exhausted,
            memory_exceeded: outcome.memory_exceeded,
            vdom_json,
        }
    }

    /// §5.4 load and activate an extension, keeping the runtime/context alive
    /// so the host can re-render chrome views after activation (clock ticks,
    /// pane-focus title changes). Returns a [`LiveExtension`] whose
    /// `render_now()` re-evaluates registered views.
    ///
    /// Unlike [`load_extension`](Self::load_extension), the QuickJS runtime
    /// is NOT dropped after activate; the caller must keep the `LiveExtension`
    /// alive for the chrome to stay live, and must call it from one thread
    /// (QuickJS Ctx is not Send across `ctx.with`).
    ///
    /// # Thread isolation (spec §5.2)
    /// This runs QuickJS on the CALLER's thread, and that is deliberate: the
    /// returned [`LiveExtension`] re-enters the same runtime on every
    /// `render_now` / `emit_event` / `execute_command`, so every one of those
    /// calls must happen on the very thread that created it. The dedicated-OS-
    /// thread guarantee of §5.2 ("the extension host must not run on the GPUI
    /// render thread") is therefore the caller's responsibility: the embedder
    /// must invoke `load_live` — and every subsequent `LiveExtension` method —
    /// from a dedicated extension thread, never the UI thread. z3rm satisfies
    /// this by driving the whole lifecycle from its `quickjs-ext-host` thread
    /// (see `ExtensionHostController`). Spawning a thread here would break the
    /// API, because the handle could then never be safely used from the caller.
    pub fn load_live(
        &self,
        extension_id: &str,
        source: &str,
        _activate_fn: &str,
    ) -> Result<LiveExtension> {
        let runtime = QuickJsRuntime::with_limits(self.limits).context("创建 Runtime 失败")?;
        let ctx = runtime.create_context()?;
        let bootstrap = bootstrap_source(self.capabilities)?;
        let script_source = to_script_source(source);
        let bridge = self.bridge.clone();
        let io_bucket = runtime.io_bucket().clone();
        let io_violated = runtime.io_violated().clone();
        let capabilities = self.capabilities;

        runtime.begin_execution();
        ctx.with(|ctx| {
            if let Some(bridge) = bridge {
                install_host_call(&ctx, bridge, capabilities, io_bucket, io_violated)?;
            }
            eval_checked::<rquickjs::Value>(&ctx, &script_source)
                .context("evaluating extension source")?;
            eval_checked::<rquickjs::Value>(&ctx, &bootstrap)
                .context("installing extension context")?;
            let activated: bool =
                eval_checked(&ctx, ACTIVATE_JS).context("calling activate(context)")?;
            if !activated {
                bail!("extension does not export an `activate` function");
            }
            Ok::<_, anyhow::Error>(())
        })
        .with_context(|| format!("activating extension `{extension_id}`"))?;

        // spec §5.6: 激活期堆占用就已顶到声明上限的扩展不允许活着出去——
        // QuickJS 可能恰好没有抛分配失败，但扩展已经处于持续超限状态。
        if runtime.memory_exceeded() {
            bail!("extension `{extension_id}` exceeded its memory budget during activation");
        }

        Ok(LiveExtension {
            extension_id: extension_id.to_string(),
            capabilities,
            runtime,
            ctx,
            memory_violated: AtomicBool::new(false),
        })
    }

    /// 内部加载逻辑：一次性激活并抓取 VDOM，同时汇报资源状态。
    fn do_load(&self, source: &str) -> LoadOutcome {
        let runtime = match QuickJsRuntime::with_limits(self.limits) {
            Ok(runtime) => runtime,
            Err(error) => {
                return LoadOutcome {
                    vdom_json: Err(error),
                    cpu_exhausted: false,
                    memory_exceeded: false,
                };
            }
        };
        let context = match runtime.create_context() {
            Ok(context) => context,
            Err(error) => {
                return LoadOutcome {
                    vdom_json: Err(error),
                    cpu_exhausted: false,
                    memory_exceeded: false,
                };
            }
        };

        let bootstrap = match bootstrap_source(self.capabilities) {
            Ok(bootstrap) => bootstrap,
            Err(error) => {
                return LoadOutcome {
                    vdom_json: Err(error),
                    cpu_exhausted: false,
                    memory_exceeded: false,
                };
            }
        };
        let script_source = to_script_source(source);
        let bridge = self.bridge.clone();
        let io_bucket = runtime.io_bucket().clone();
        let io_violated = runtime.io_violated().clone();
        let capabilities = self.capabilities;

        runtime.begin_execution();
        let vdom_json = context.with(|ctx| {
            if let Some(bridge) = bridge {
                install_host_call(&ctx, bridge, capabilities, io_bucket, io_violated)?;
            }
            eval_checked::<rquickjs::Value>(&ctx, &script_source)
                .context("evaluating extension source")?;
            eval_checked::<rquickjs::Value>(&ctx, &bootstrap)
                .context("installing extension context")?;
            // A source file without `activate` is still a successful load; the
            // fuzz corpus relies on bare scripts being evaluated.
            let activated: bool =
                eval_checked(&ctx, ACTIVATE_JS).context("calling activate(context)")?;
            if !activated {
                return Ok(None);
            }
            let rendered: String = eval_checked(&ctx, "globalThis.__z3rm_render_views()")
                .context("rendering views")?;
            Ok::<_, anyhow::Error>(first_vdom(&rendered))
        });

        // 内存判定要在 runtime 还活着的时候做；错误文本里的 "out of memory" 是
        // QuickJS 分配失败的直接证据，堆占用则覆盖「刚好卡在上限」的情况。
        let memory_exceeded = runtime.memory_exceeded()
            || vdom_json
                .as_ref()
                .err()
                .is_some_and(|error| format!("{error:#}").contains("out of memory"));

        LoadOutcome {
            vdom_json,
            cpu_exhausted: runtime.take_cpu_interrupted(),
            memory_exceeded,
        }
    }
}

/// `__z3rm_render_views()` 返回 VDOM 数组的 JSON；取第一个作为单值渲染结果。
fn first_vdom(rendered: &str) -> Option<String> {
    parse_rendered_views(rendered)
        .ok()
        .and_then(|views| views.into_iter().next())
}

fn parse_rendered_views(rendered: &str) -> Result<Vec<String>> {
    let values: Vec<serde_json::Value> =
        serde_json::from_str(rendered).context("extension render output is not a JSON array")?;
    values
        .into_iter()
        .map(|value| serde_json::to_string(&value).context("re-serializing extension VDOM"))
        .collect()
}

/// §5.4 One display-list refresh result: the region id and the fresh draw ops
/// JSON produced by re-invoking only the registered renderer method.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayListRegion {
    pub region_id: String,
    pub ops_json: String,
}

fn parse_display_list_regions(rendered: &str) -> Result<Vec<DisplayListRegion>> {
    let values: Vec<serde_json::Value> = serde_json::from_str(rendered)
        .context("extension display list refresh output is not a JSON array")?;
    values
        .into_iter()
        .map(|value| {
            let region_id = value
                .get("region")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let ops = value
                .get("ops")
                .ok_or_else(|| anyhow::anyhow!("display list region {region_id:?} missing ops"))?;
            let ops_json =
                serde_json::to_string(ops).context("re-serializing display list draw ops")?;
            Ok(DisplayListRegion {
                region_id,
                ops_json,
            })
        })
        .collect()
}

/// §5.4 a live chrome extension: the QuickJS runtime and context survive
/// activation so the host can re-render registered views when they
/// `invalidate()`. Built-in extensions (status-bar clock, pane-focus title)
/// need this to update after the initial paint.
///
/// `Ctx` is `!Send` only through `ctx.with`; the `Context` handle itself is
/// shareable, so all `ctx.with` calls run on whatever thread calls
/// `render_now`. The mux recorder already pins QuickJS work to one thread;
/// callers must do the same.
pub struct LiveExtension {
    extension_id: String,
    capabilities: ExtensionCapabilities,
    /// Kept alive so the JS runtime is not dropped after activation.
    runtime: QuickJsRuntime,
    /// Live context handle; `ctx.with` re-enters the QuickJS runtime to
    /// re-render views. Context is a handle (Send); the Ctx inside `with` is
    /// not, so all `with` calls must run on one thread.
    ctx: Context,
    /// 置位表示某次 JS 执行触发了内存上限 (分配失败或堆占用卡在上限)。
    /// 由宿主在 [`take_memory_violated`](Self::take_memory_violated) 读取并
    /// 清零；违规扩展按 spec §5.6 应被挂起而非继续执行。
    memory_violated: AtomicBool,
}

impl LiveExtension {
    pub fn id(&self) -> &str {
        &self.extension_id
    }

    pub fn capabilities(&self) -> ExtensionCapabilities {
        self.capabilities
    }

    /// 上一次 JS 执行是否被 CPU 预算中断 (spec §5.6: 违规应挂起扩展)。
    pub fn take_cpu_interrupted(&self) -> bool {
        self.runtime.take_cpu_interrupted()
    }

    /// Whether a host call exceeded the declared IO rate limit since the last
    /// check. Hosts use this to suspend an extension even when its JavaScript
    /// catches the rejected call.
    pub fn take_io_violated(&self) -> bool {
        self.runtime.take_io_violated()
    }

    /// 当前堆占用是否已达到声明的内存上限 (与 [`QuickJsRuntime::memory_exceeded`]
    /// 语义一致，供宿主在挂起判定时参考)。
    pub fn memory_exceeded(&self) -> bool {
        self.runtime.memory_exceeded()
    }

    /// 读取并清除「发生过内存违规」标志：某次宿主 → JS 调用因内存上限失败
    /// (QuickJS 抛 `out of memory`)，或堆占用卡在声明上限之上。
    /// spec §5.6: 违规应导致扩展挂起，宿主用该标志做挂起判定。
    pub fn take_memory_violated(&self) -> bool {
        self.memory_violated.swap(false, Ordering::Relaxed)
    }

    /// 在专用线程上求值一段 JS，并记录资源违规标志。
    ///
    /// 所有宿主 → JS 入口都走这里：`begin_execution` 保证宿主空闲期不计入
    /// CPU 预算，而任何失败若命中 QuickJS 的内存上限 (错误文本含 `out of
    /// memory`) 或堆占用达到上限，都会被记录为内存违规——宿主随后可以
    /// 显式挂起扩展，而不是让超限继续静默执行。
    fn run_js<T>(&self, f: impl for<'js> FnOnce(&rquickjs::Ctx<'js>) -> Result<T>) -> Result<T> {
        self.runtime.begin_execution();
        let outcome = self.ctx.with(|ctx| f(&ctx));
        if let Err(error) = &outcome
            && format!("{error:#}").contains("out of memory")
        {
            self.memory_violated.store(true, Ordering::Relaxed);
        }
        if self.runtime.memory_exceeded() {
            self.memory_violated.store(true, Ordering::Relaxed);
        }
        outcome
    }

    /// §5.4 安装/替换宿主桥。宿主线程在 mux 连接建立后调用，此前扩展已经
    /// activate 完毕——`hostCall` 每次调用都重新查全局，因此后装也生效。
    pub fn install_bridge(&self, bridge: Arc<dyn HostBridge>) -> Result<()> {
        let capabilities = self.capabilities;
        let io_bucket = self.runtime.io_bucket().clone();
        let io_violated = self.runtime.io_violated().clone();
        self.run_js(|ctx| install_host_call(ctx, bridge, capabilities, io_bucket, io_violated))
    }

    /// §5.4 是否有视图请求过重绘 (`view.invalidate()` / `context.render()`)。
    /// 宿主据此按需渲染，取代固定频率轮询。
    pub fn needs_render(&self) -> Result<bool> {
        self.run_js(|ctx| eval_checked::<bool>(ctx, "!!globalThis.__z3rm_rerender"))
    }

    /// §5.4 invoke `invalidate()` on every registered chrome view. Extensions
    /// call this themselves from event handlers; the host exposes it so a
    /// host-driven event (e.g. pane focus) can also request a re-render. The
    /// next [`render_now`](Self::render_now) pulls a fresh VDOM.
    pub fn invalidate_registered_views(&self) -> Result<()> {
        self.run_js(|ctx| {
            eval_checked::<()>(
                ctx,
                r#"
                (function() {
                    globalThis.__z3rm_rerender = true;
                    var views = globalThis.__chrome_views || {};
                    var names = Object.keys(views);
                    for (var i = 0; i < names.length; i++) {
                        var view = views[names[i]];
                        if (view && typeof view.invalidate === 'function') {
                            try { view.invalidate(); } catch (error) {}
                        }
                    }
                })()
            "#,
            )
        })
    }

    /// §5.4 re-evaluate every registered chrome view's `render()` and return
    /// one JSON VDOM per view that produced output. Clears the invalidation
    /// flag, so a subsequent [`needs_render`](Self::needs_render) reports
    /// whether anything changed since this render.
    pub fn render_all_views(&self) -> Result<Vec<String>> {
        let rendered: String =
            self.run_js(|ctx| eval_checked(ctx, "globalThis.__z3rm_render_views()"))?;
        parse_rendered_views(&rendered)
    }

    /// §5.4 便捷入口：返回首个非空 VDOM (status-bar 优先)。
    pub fn render_now(&self) -> Result<Option<String>> {
        Ok(self.render_all_views()?.into_iter().next())
    }

    /// §5.4 Re-invoke only the registered display-list renderer methods on the
    /// last rendered chrome, returning one entry per refreshed region.
    ///
    /// Unlike [`render_all_views`](Self::render_all_views) this neither
    /// re-runs view `render()` bodies nor clears the invalidation flag, so a
    /// ticking clock refreshes its draw ops without invalidating the
    /// surrounding chrome tree (§5.4 display-list pattern).
    pub fn refresh_display_lists(&self) -> Result<Vec<DisplayListRegion>> {
        let rendered: String =
            self.run_js(|ctx| eval_checked(ctx, "globalThis.__z3rm_refresh_display_lists()"))?;
        parse_display_list_regions(&rendered)
    }

    /// §3.4 把宿主事件投递给扩展的订阅者，返回被调用的 handler 数量。
    pub fn emit_event(&self, event_name: &str, payload_json: &str) -> Result<usize> {
        let name = serde_json::to_string(event_name).context("serializing event name")?;
        let payload = if payload_json.trim().is_empty() {
            "null".to_string()
        } else {
            payload_json.to_string()
        };
        let snippet = format!("globalThis.__z3rm_dispatch_event({name}, {payload})");
        let delivered: i32 = self.run_js(|ctx| eval_checked(ctx, &snippet))?;
        Ok(delivered.max(0) as usize)
    }

    pub fn execute_command(&self, command_id: &str, arguments_json: &str) -> Result<bool> {
        let id = serde_json::to_string(command_id).context("serializing command id")?;
        let arguments = if arguments_json.trim().is_empty() {
            "undefined".to_string()
        } else {
            let value: serde_json::Value = serde_json::from_str(arguments_json)
                .context("parsing chrome command arguments")?;
            serde_json::to_string(&value).context("serializing chrome command arguments")?
        };
        // Parse/re-serialize before embedding: `arguments_json` can originate
        // in an ExtensionChromeAction request and must never be treated as JS.
        let snippet = format!("globalThis.__z3rm_execute_command({id}, {arguments})");
        self.run_js(|ctx| eval_checked(ctx, &snippet))
    }

    /// 扩展注册的命令列表 (JSON: `[{id, label}]`)。
    pub fn list_commands(&self) -> Result<String> {
        self.run_js(|ctx| eval_checked(ctx, "globalThis.__z3rm_list_commands()"))
    }

    /// 扩展注册的键位列表 (JSON: `[{chord, command}]`)。
    pub fn list_keymaps(&self) -> Result<String> {
        self.run_js(|ctx| eval_checked(ctx, "globalThis.__z3rm_list_keymaps()"))
    }

    /// 取走扩展内部被捕获的错误（事件 handler / render 抛出的异常），
    /// 宿主负责记录日志——扩展异常不能静默丢弃。
    ///
    /// bootstrap 的 render/event try/catch 会把 QuickJS 的内存超限异常收进这个
    /// 列表而不是让它冒泡到 Rust，因此这里顺带扫描 "out of memory" 并记录内存
    /// 违规标志——否则被 JS 层吞掉的超限会静默继续。
    pub fn take_errors(&self) -> Result<Vec<String>> {
        let json: String =
            self.run_js(|ctx| eval_checked(ctx, "globalThis.__z3rm_take_errors()"))?;
        let errors: Vec<String> =
            serde_json::from_str(&json).context("parsing extension error list")?;
        if errors.iter().any(|error| error.contains("out of memory")) {
            self.memory_violated.store(true, Ordering::Relaxed);
        }
        Ok(errors)
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
    fn host_events_require_their_manifest_capabilities() {
        let none = ExtensionCapabilities::default();
        assert!(!none.allows_host_event("pane:output"));
        assert!(!none.allows_host_event("pane:focus"));

        let terminal = ExtensionCapabilities {
            terminal: true,
            ..ExtensionCapabilities::default()
        };
        assert!(terminal.allows_host_event("pane:output"));
        assert!(!terminal.allows_host_event("pane:focus"));

        let mux = ExtensionCapabilities {
            mux: true,
            ..ExtensionCapabilities::default()
        };
        assert!(mux.allows_host_event("pane:focus"));
        assert!(!mux.allows_host_event("pane:output"));
        assert!(none.allows_host_event("extension:custom"));
    }

    #[test]
    fn oversized_memory_limit_is_rejected() {
        let result = QuickJsRuntime::with_limits(ExtensionLimits::new(usize::MAX, 50, 100.0));
        assert!(result.is_err());
    }

    /// 记录调用的假宿主桥，用来断言 JS → Rust 参数传递与能力拦截。
    struct RecordingBridge {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
        responses: BTreeMap<String, serde_json::Value>,
    }

    impl RecordingBridge {
        fn new(responses: BTreeMap<String, serde_json::Value>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responses,
            })
        }

        fn calls(&self) -> Vec<(String, serde_json::Value)> {
            self.calls.lock().clone()
        }
    }

    impl HostBridge for RecordingBridge {
        fn call(&self, method: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
            self.calls.lock().push((method.to_string(), args.clone()));
            self.responses
                .get(method)
                .cloned()
                .ok_or_else(|| anyhow!("unimplemented host method: {method}"))
        }
    }

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
    fn extension_settings_keys_and_timeouts_fail_closed() {
        assert!(validate_settings_key("terminal.theme").is_ok());
        assert!(validate_settings_key("").is_err());
        assert!(validate_settings_key("terminal..theme").is_err());
        let too_many_segments = std::iter::repeat_n("x", MAX_EXTENSION_SETTINGS_SEGMENTS + 1)
            .collect::<Vec<_>>()
            .join(".");
        assert!(validate_settings_key(&too_many_segments).is_err());

        let default = Duration::from_secs(3);
        assert_eq!(
            parse_extension_timeout(&serde_json::json!({}), default, 1_000).unwrap(),
            default
        );
        assert_eq!(
            parse_extension_timeout(&serde_json::json!({"timeout": 2_000}), default, 1_000)
                .unwrap(),
            Duration::from_secs(1)
        );
        assert!(parse_extension_timeout(&serde_json::json!({"timeout": 0}), default, 1_000)
            .is_err());
        assert!(parse_extension_timeout(&serde_json::json!({"timeout": -1}), default, 1_000)
            .is_err());
    }

    #[test]
    fn extension_process_rejects_paths_and_bounds_execution() {
        assert!(run_extension_process("./tool", &[], Duration::from_secs(1)).is_err());
        assert!(run_extension_process("", &[], Duration::from_secs(1)).is_err());

        let output = run_extension_process(
            "printf",
            &["z3rm".to_string()],
            Duration::from_secs(1),
        )
        .expect("bounded process should complete");
        assert_eq!(output.stdout, b"z3rm");
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

    /// P1-1: fuel 必须是真实的 wall-clock 预算，而不是回调计数。
    #[test]
    fn cpu_fuel_tracks_wall_clock_time_not_callback_count() {
        let tracker = CpuFuelTracker::new(50);

        // 大量紧挨着的回调只花掉微不足道的时间，不应触发中断。
        for _ in 0..10_000 {
            assert!(
                !tracker.check(),
                "回调计数不应消耗预算，已用 {:?}",
                tracker.used()
            );
        }
        assert!(
            tracker.used() < Duration::from_millis(50),
            "1 万次空回调不该烧掉 50ms 预算，实际 {:?}",
            tracker.used()
        );
    }

    /// spec §5.2: 超预算不是立刻杀，要连续 3 个预算周期 (~150ms) 才中断。
    #[test]
    fn cpu_fuel_interrupts_only_after_three_budget_overruns() {
        // 预算取得比睡眠抖动大一个量级：贴着阈值睡会让 thread::sleep 的
        // 过冲直接决定断言结果。
        let tracker = CpuFuelTracker::new(20);

        // 第一次 check 建立 checkpoint，不计时。
        assert!(!tracker.check());
        // 25ms 远低于 3 × 20ms 的中断阈值。
        std::thread::sleep(Duration::from_millis(25));
        assert!(!tracker.check(), "低于 3× 预算不应中断");
        // 再过 50ms 累计 75ms > 3 × 20ms，应中断。
        std::thread::sleep(Duration::from_millis(50));
        assert!(tracker.check(), "累计超过 3× 预算应中断");
        assert!(tracker.take_interrupted());
        assert!(!tracker.take_interrupted(), "标志读取后应清零");
    }

    /// 宿主空闲期不能计入扩展的 CPU 预算。
    #[test]
    fn cpu_fuel_excludes_host_idle_time() {
        let tracker = CpuFuelTracker::new(10);
        assert!(!tracker.check());
        std::thread::sleep(Duration::from_millis(25));
        // 宿主重新进入 JS：上一次 checkpoint 作废。
        tracker.begin_execution();
        assert!(!tracker.check(), "空闲期不应计费");
        assert_eq!(tracker.used(), Duration::ZERO);
    }

    #[test]
    fn test_io_token_bucket() {
        let bucket = Arc::new(IoTokenBucket::new(100.0, 200.0));

        // 初始 200 tokens
        assert!(bucket.try_acquire(200.0));
        assert!(!bucket.try_acquire(1.0));

        // 等待补充
        std::thread::sleep(Duration::from_millis(100));
        // 100ms 后补充约 10 tokens
        assert!(bucket.try_acquire(10.0));
    }

    /// §5.4 chrome must stay live: after activate, a registered view's
    /// `invalidate()` must request a host re-render, and `render_now()` must
    /// return a fresh VDOM reflecting post-activate state.
    #[test]
    fn test_live_extension_re_renders_after_invalidate() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            var ticks = 0;
            function activate(context) {
                var view = {
                    render: function() {
                        ticks++;
                        return { type: 'div', props: { id: 'tick' }, children: [String(ticks)] };
                    }
                };
                context.registerChromeView('status-bar', view);
                globalThis.__test_view = view;
            }
        "#;
        let live = runner.load_live("tick-ext", source, "activate")?;

        let first = live.render_now()?.context("initial vdom present")?;
        assert!(first.contains("\"id\":\"tick\""), "first vdom: {first}");
        assert!(first.contains("\"1\""), "first render tick=1: {first}");

        // §5.4 drive invalidate through the extension's own view handle to prove
        // the host hook is wired end-to-end, then re-render and observe ticks=2.
        live.invalidate_registered_views()?;
        let second = live.render_now()?.context("second vdom present")?;
        assert!(second.contains("\"2\""), "second render tick=2: {second}");
        assert_ne!(first, second, "render_now must re-evaluate view.render()");
        Ok(())
    }

    /// P1-4: `invalidate()` 必须让 Rust 侧看到失效标志，宿主才能按需渲染。
    #[test]
    fn invalidate_flag_is_observable_from_rust() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            function activate(context) {
                globalThis.__view = context.registerChromeView('status-bar', {
                    render: function() { return { type: 'span', children: ['x'] }; }
                });
                context.mux.subscribe('pane:focus', function() {
                    globalThis.__view.invalidate();
                });
            }
        "#;
        let live = runner.load_live("dirty-ext", source, "activate")?;

        assert!(live.needs_render()?, "registerChromeView 应请求首帧渲染");
        live.render_all_views()?;
        assert!(!live.needs_render()?, "渲染后失效标志应清零");

        assert_eq!(live.emit_event("pane:focus", r#"{"title":"a"}"#)?, 1);
        assert!(
            live.needs_render()?,
            "事件触发的 invalidate 必须被 Rust 观察到"
        );
        Ok(())
    }

    /// §5.4: a display-list refresh must re-invoke only the registered
    /// renderer method — never the view's render() — and must not touch the
    /// invalidation flag, so a ticking clock repaints without invalidating
    /// the surrounding chrome.
    #[test]
    fn display_list_refresh_reinvokes_only_renderer_methods() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            var renders = 0;
            var ticks = 0;
            function activate(context) {
                context.registerChromeView('status-bar', {
                    render: function() {
                        renders++;
                        // A second render() would replace the display-list
                        // region with a plain div: a refresh that re-ran
                        // render() would return no regions at all.
                        if (renders > 1) {
                            return { type: 'div', children: ['re-rendered'] };
                        }
                        return {
                            type: 'display-list',
                            props: { id: 'clock', renderer: 'renderClock' }
                        };
                    },
                    renderClock: function() {
                        ticks++;
                        return [{ op: 'drawText', text: String(ticks), x: 0, y: 0 }];
                    }
                });
            }
        "#;
        let live = runner.load_live("clock-ext", source, "activate")?;
        live.render_all_views()?;

        // Initial VDOM rendering attaches the first draw list; the refresh
        // path must invoke only renderClock and return its next tick.
        let regions = live.refresh_display_lists()?;
        assert_eq!(regions.len(), 1, "clock region must refresh: {regions:?}");
        assert_eq!(regions[0].region_id, "clock");
        assert!(
            regions[0].ops_json.contains("\"2\""),
            "first refresh tick must be visible: {}",
            regions[0].ops_json
        );

        // The second refresh increments the renderer again while render()
        // must not have re-run (the cached VDOM still holds the region).
        let regions = live.refresh_display_lists()?;
        assert_eq!(regions.len(), 1, "render() must not run on refresh: {regions:?}");
        assert!(
            regions[0].ops_json.contains("\"3\""),
            "second refresh tick must be visible: {}",
            regions[0].ops_json
        );

        // §5.4 refresh must not set the invalidation flag.
        assert!(
            !live.needs_render()?,
            "display-list refresh must not invalidate the chrome tree"
        );
        Ok(())
    }

    /// 扩展加载与激活: 一次性 load_extension 应成功并回报时长。
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
        assert!(result.result.is_err(), "无限循环应被资源限制终止");
        assert!(result.cpu_exhausted, "中断原因应报告为 CPU 预算耗尽");
    }

    /// P1-3: 任何 JS 错误都不该被误报成 CPU 耗尽。
    #[test]
    fn plain_js_error_is_not_reported_as_cpu_exhaustion() {
        let runner = ExtensionRunner::with_defaults();
        let result = runner.load_extension(
            "throwing-ext",
            "function activate() { throw new Error('boom'); }",
            "activate",
        );

        assert!(result.result.is_err(), "抛异常的扩展应加载失败");
        assert!(!result.cpu_exhausted, "普通异常不是 CPU 耗尽");
        assert!(!result.memory_exceeded, "普通异常不是内存超限");
        let message = format!("{:#}", result.result.expect_err("error expected"));
        assert!(message.contains("boom"), "错误应保留 JS 消息: {message}");
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

    /// §5.2: one-shot execution must happen on a freshly spawned OS thread,
    /// distinct from (and isolated from) the caller — the sandbox guarantee
    /// that a runaway extension cannot execute on the embedding/UI thread.
    #[test]
    fn execute_in_thread_runs_on_a_dedicated_os_thread() -> Result<()> {
        let runtime = QuickJsRuntime::with_defaults()?;
        let caller = std::thread::current().id();
        let (result, inner) = runtime.execute_in_thread(|ctx| {
            let r: String = ctx.eval("'hello from thread'")?;
            Ok::<_, anyhow::Error>((r, std::thread::current().id()))
        })?;
        assert_eq!(result, "hello from thread");
        assert_ne!(caller, inner, "JS must not execute on the caller's thread");
        Ok(())
    }

    /// §5.2/§5.4: a [`LiveExtension`]'s runtime is pinned to the thread that
    /// created it; the embedder must drive its whole lifecycle from one
    /// dedicated thread (the host does this via `quickjs-ext-host`). This test
    /// reproduces that pattern: the runtime is created and re-entered for a
    /// render on the same spawned thread, never the test's main thread.
    #[test]
    fn live_extension_runs_on_a_single_dedicated_thread() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            function activate(context) {
                context.registerChromeView('pinned', {
                    render: function() { return { type: 'span', children: ['ok'] }; }
                });
            }
        "#;
        let host_thread = std::thread::Builder::new()
            .name("quickjs-ext-test".to_string())
            .spawn(move || -> Result<String> {
                // Create and re-enter the LiveExtension entirely on this thread,
                // exactly as the production host does. Touching it from any
                // other thread would be undefined behavior.
                let live = runner.load_live("pinned-ext", source, "activate")?;
                live.render_now()?.context("pinned view must render")
            })?;
        let vdom = host_thread
            .join()
            .map_err(|e| anyhow!("extension thread panicked: {e:?}"))??;
        assert!(vdom.contains("ok"), "vdom={vdom}");
        assert!(
            vdom.contains("\"id\":\"pinned\""),
            "named views need stable VDOM identity: {vdom}"
        );
        Ok(())
    }

    #[test]
    fn live_extension_rejects_excess_chrome_views() -> Result<()> {
        let registrations = (0..=MAX_EXTENSION_VIEWS)
            .map(|index| {
                format!(
                    "context.registerChromeView('view-{index}', {{ render: function() {{ return null; }} }});"
                )
            })
            .collect::<String>();
        let source = format!("function activate(context) {{ {registrations} }}");
        let runner = ExtensionRunner::with_defaults();
        let error = match runner.load_live("view-limit", &source, "activate") {
            Ok(_) => bail!("the view registration limit must fail closed"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("registerChromeView limit exceeded"), "{message}");
        Ok(())
    }
    /// P1-3: `execute_in_thread` 必须沿用实例配置，而不是硬编码默认值。
    #[test]
    fn execute_in_thread_honours_instance_limits() -> Result<()> {
        let runtime = QuickJsRuntime::with_limits(ExtensionLimits::new(1, 50, 100.0))?;
        let result = runtime.execute_in_thread(|ctx| {
            let value: rquickjs::Value = ctx.eval(
                r#"
                var blocks = [];
                for (var i = 0; i < 10000000; i++) { blocks.push(new Array(1000)); }
                "#,
            )?;
            Ok::<_, anyhow::Error>(value.is_undefined())
        });
        assert!(result.is_err(), "1MB 限制应在子线程内生效");
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
        assert!(
            result.result.is_ok(),
            "activate must succeed: {:?}",
            result.result
        );
        let vdom = result
            .vdom_json
            .context("status-bar VDOM must be captured")?;
        assert!(vdom.contains("status-bar"), "vdom={vdom}");
        assert!(vdom.contains("demo"), "vdom={vdom}");
        Ok(())
    }

    #[test]
    fn live_mux_event_updates_registered_view() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            function activate(context) {
                var state = { paneTitle: 'shell' };
                context.mux.subscribe('pane:focus', function(pane) {
                    state.paneTitle = pane.title || '';
                });
                context.registerChromeView('status-bar', {
                    render: function() {
                        return { type: 'span', children: [state.paneTitle] };
                    }
                });
            }
        "#;

        let extension = runner.load_live("event-test", source, "activate")?;
        let initial = extension
            .render_now()?
            .context("registered view did not render")?;
        assert!(initial.contains("shell"), "initial vdom={initial}");

        extension.emit_event("pane:focus", r#"{"title":"editor"}"#)?;
        let updated = extension
            .render_now()?
            .context("registered view did not render after event")?;
        assert!(updated.contains("editor"), "updated vdom={updated}");
        Ok(())
    }

    /// P0-2: `keymaps.subscribe` 必须存在并能收到宿主的 `prefix` 事件。
    #[test]
    fn keymap_subscribe_receives_host_events() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            function activate(context) {
                var state = { visible: false, prefix: '' };
                context.keymaps.subscribe('prefix', function(event) {
                    state.visible = event.active;
                    state.prefix = event.prefix || '';
                });
                context.registerChromeView('which-key', {
                    render: function() {
                        if (!state.visible) { return null; }
                        return { type: 'span', children: [state.prefix] };
                    }
                });
            }
        "#;
        let extension = runner.load_live("which-key-like", source, "activate")?;
        assert!(extension.render_now()?.is_none(), "隐藏时不应产生 VDOM");

        let delivered = extension.emit_event("prefix", r#"{"active":true,"prefix":"ctrl-b"}"#)?;
        assert_eq!(delivered, 1, "prefix 事件必须投递给 keymaps 订阅者");
        let vdom = extension.render_now()?.context("which-key vdom")?;
        assert!(vdom.contains("ctrl-b"), "vdom={vdom}");
        Ok(())
    }

    /// P0-4: mux 调用必须真正落到宿主桥上，参数原样传递。
    #[test]
    fn mux_calls_reach_the_host_bridge() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([
            (
                "mux.listSessions".to_string(),
                serde_json::json!([{ "id": "s1", "name": "work", "clients": 2 }]),
            ),
            ("mux.focusPane".to_string(), serde_json::json!(true)),
            ("mux.splitPane".to_string(), serde_json::json!("pane-2")),
        ]));
        let runner = ExtensionRunner::with_defaults().with_bridge(bridge.clone());
        let source = r#"
            function activate(context) {
                globalThis.__sessions = context.mux.listSessions();
                context.commands.register('split', function() {
                    globalThis.__new_pane = context.mux.splitPane('right', 'pane-1');
                });
                context.mux.focusPane('pane-1');
            }
        "#;
        let extension = runner.load_live("bridge-test", source, "activate")?;
        assert!(extension.execute_command("split", "[]")?);

        let calls = bridge.calls();
        assert_eq!(
            calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            vec!["mux.listSessions", "mux.focusPane", "mux.splitPane"],
        );
        assert_eq!(calls[1].1, serde_json::json!(["pane-1"]));
        assert_eq!(calls[2].1, serde_json::json!(["right", "pane-1"]));
        Ok(())
    }
    #[test]
    fn execute_command_rejects_javascript_argument_injection() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();
        let source = r#"
            function activate(context) {
                context.commands.register('safe', function(args) {
                    globalThis.__safe_arg = args;
                });
            }
        "#;
        let extension = runner.load_live("command-arguments", source, "activate")?;
        assert!(extension.execute_command("safe", r#"{"ok":true}"#)?);
        assert!(extension
            .execute_command("safe", r#"{}); globalThis.__injected = true; ({}"#)
            .is_err());
        Ok(())
    }

    /// P0-4 + P0-5: `listSessions` 返回的字段名必须是扩展读取的 `clients`。
    #[test]
    fn list_sessions_exposes_client_count_field() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([(
            "mux.listSessions".to_string(),
            serde_json::json!([{ "id": "s1", "name": "work", "cwd": "/tmp", "clients": 3 }]),
        )]));
        let runner = ExtensionRunner::with_defaults().with_bridge(bridge);
        let source = r#"
            function activate(context) {
                context.registerChromeView('session-manager', {
                    render: function() {
                        var sessions = context.mux.listSessions();
                        return {
                            type: 'div',
                            children: sessions.map(function(session) {
                                return { type: 'span', children: [session.name + ' (' + session.clients + ')'] };
                            })
                        };
                    }
                });
            }
        "#;
        let extension = runner.load_live("session-manager-like", source, "activate")?;
        let vdom = extension.render_now()?.context("session manager vdom")?;
        assert!(vdom.contains("work (3)"), "vdom={vdom}");
        Ok(())
    }

    /// §5.6: 未声明的能力必须在运行时被拒绝，而不是静默放行。
    #[test]
    fn undeclared_capability_is_denied_at_runtime() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([(
            "mux.listSessions".to_string(),
            serde_json::json!([]),
        )]));
        let capabilities = ExtensionCapabilities {
            workspace: true,
            ..ExtensionCapabilities::default()
        };
        let runner = ExtensionRunner::with_defaults()
            .with_capabilities(capabilities)
            .with_bridge(bridge.clone());

        let result = runner.load_live(
            "no-mux-capability",
            "function activate(context) { context.mux.listSessions(); }",
            "activate",
        );
        let error = format!("{:#}", result.err().context("expected activation failure")?);
        assert!(error.contains("capability"), "error={error}");
        assert!(bridge.calls().is_empty(), "被拒绝的调用不应到达宿主");
        Ok(())
    }

    /// §5.6: JS 侧的检查可以被扩展绕过，Rust 侧必须仍然拦住。
    #[test]
    fn capability_is_enforced_in_rust_not_only_in_js() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([(
            "settings.get".to_string(),
            serde_json::json!("leaked"),
        )]));
        let runner = ExtensionRunner::with_defaults()
            .with_capabilities(ExtensionCapabilities::default())
            .with_bridge(bridge.clone());

        // 直接调用底层 __z3rm_host_call，跳过 context.settings 的 JS 校验。
        let source = r#"
            function activate(context) {
                var raw = globalThis.__z3rm_host_call('settings.get', JSON.stringify(['theme']));
                globalThis.__result = JSON.parse(raw);
            }
        "#;
        let extension = runner.load_live("capability-bypass", source, "activate")?;
        let denied: bool = extension.render_all_views().map(|_| true).unwrap_or(true);
        assert!(denied);
        assert!(bridge.calls().is_empty(), "Rust 侧必须在桥调用前拒绝");
        Ok(())
    }

    /// §5.6: 文件系统路径约束必须锚定相对路径、解析符号链接并拒绝一切越界。
    /// 同一个入口服务 `Home` 与 `Cwd` 两个声明范围——根可以是主目录, 也可以
    /// 是权威工作区/当前工作根。
    #[test]
    fn confine_to_root_anchors_relative_paths_and_denies_escapes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join("docs"))?;
        std::fs::write(home.join("docs").join("note.txt"), "secret")?;
        // 兄弟目录: 组件级 starts_with 必须拒绝, 前缀字符串比较会误放行。
        let sibling = temp.path().join("home-other");
        std::fs::create_dir_all(&sibling)?;

        // 相对路径锚定到主目录。
        assert_eq!(
            confine_to_root(&home, "docs/note.txt")?,
            home.join("docs").join("note.txt").canonicalize()?
        );
        // 主目录内的绝对路径直接放行。
        let note = home.join("docs").join("note.txt");
        assert_eq!(
            confine_to_root(&home, &note.to_string_lossy())?,
            note.canonicalize()?
        );
        // 主目录外的绝对路径拒绝。
        let error = confine_to_root(&home, "/etc/passwd").unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        // 兄弟目录拒绝 (不是字符串前缀匹配)。
        let error = confine_to_root(&home, &sibling.join("x").to_string_lossy()).unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        // 相对路径经 ".." 逃逸到主目录外拒绝。
        let error = confine_to_root(&home, "../home-other/x").unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        // 缺失文件的尾段: 解析最近存在的父目录, 返回主目录内的绝对路径
        // (之后读取报 NotFound, 而不是误导性的拒绝)。
        assert_eq!(
            confine_to_root(&home, "docs/missing.txt")?,
            home.join("docs").join("missing.txt")
        );
        // 符号链接逃逸: 主目录内的链接指向外部文件, canonicalize 必须暴露
        // 真实位置并拒绝。
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", home.join("docs").join("leak"))?;
            let error = confine_to_root(&home, "docs/leak").unwrap_err();
            assert!(error.to_string().contains("escapes"), "error={error}");
        }
        // Cwd 声明同样走这个入口: 根换成工作区目录后, 主目录与工作区互相
        // 隔离——工作区根内的路径放行, 主目录内的路径拒绝 (cwd 声明不能
        // 读取任意 HOME 文件)。
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src"))?;
        std::fs::write(workspace.join("src").join("main.js"), "code")?;
        assert_eq!(
            confine_to_root(&workspace, "src/main.js")?,
            workspace.join("src").join("main.js").canonicalize()?
        );
        let error = confine_to_root(&workspace, &note.to_string_lossy()).unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        let error = confine_to_root(&workspace, "../home/docs/note.txt").unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        Ok(())
    }

    /// §5.6: 声明的 workspace/filesystem/network/process_spawn 调用必须真正
    /// 落到宿主桥上 (而不是 "declared but unreachable"), 参数原样传递, 返回值
    /// 回到扩展可见。
    #[test]
    fn declared_capabilities_reach_the_host_bridge() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([
            ("workspace.getPath".to_string(), serde_json::json!("/work")),
            (
                "filesystem.readTextFile".to_string(),
                serde_json::json!("file contents"),
            ),
            (
                "filesystem.readDir".to_string(),
                serde_json::json!([{ "name": "a.txt", "kind": "file" }]),
            ),
            (
                "network.fetch".to_string(),
                serde_json::json!({ "status": 200 }),
            ),
            ("process.spawn".to_string(), serde_json::json!("pid-1")),
        ]));
        let runner = ExtensionRunner::with_defaults().with_bridge(bridge.clone());
        let source = r#"
            function activate(context) {
                context.registerChromeView('caps', {
                    render: function() {
                        return {
                            type: 'span',
                            children: [
                                context.workspace.getPath() + '|' +
                                context.filesystem.readTextFile('/home/u/note.txt') + '|' +
                                String(context.filesystem.readDir('/home/u').length) + '|' +
                                String(context.network.fetch('https://example.test/api', { method: 'GET' }).status) + '|' +
                                context.process.spawn('echo', ['hello'])
                            ]
                        };
                    }
                });
            }
        "#;
        let extension = runner.load_live("declared-caps", source, "activate")?;
        let vdom = extension.render_now()?.context("caps vdom")?;
        assert!(
            vdom.contains("/work|file contents|1|200|pid-1"),
            "声明能力的返回值必须一路回到扩展: vdom={vdom}"
        );

        let calls = bridge.calls();
        let methods = calls
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec![
                "workspace.getPath",
                "filesystem.readTextFile",
                "filesystem.readDir",
                "network.fetch",
                "process.spawn",
            ],
        );
        assert_eq!(calls[1].1, serde_json::json!(["/home/u/note.txt"]));
        assert_eq!(calls[2].1, serde_json::json!(["/home/u"]));
        assert_eq!(
            calls[3].1,
            serde_json::json!(["https://example.test/api", { "method": "GET" }])
        );
        assert_eq!(calls[4].1, serde_json::json!(["echo", ["hello"]]));
        Ok(())
    }

    /// §5.6: 未声明的能力在 JS 侧 requireCapability 拒绝; 绕过 JS 直接调用
    /// `__z3rm_host_call` 也在 Rust 侧 (allows) 被拒——桥永远收不到调用。
    #[test]
    fn undeclared_capabilities_are_denied_before_the_bridge() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([(
            "workspace.getPath".to_string(),
            serde_json::json!("leaked"),
        )]));
        let runner = ExtensionRunner::with_defaults()
            .with_capabilities(ExtensionCapabilities::default())
            .with_bridge(bridge.clone());
        let source = r#"
            function activate(context) {
                var errors = [];
                try { context.workspace.getPath(); } catch (e) { errors.push(String(e)); }
                try { context.filesystem.readTextFile('/tmp/x'); } catch (e) { errors.push(String(e)); }
                try { context.network.fetch('http://example.test'); } catch (e) { errors.push(String(e)); }
                try { context.process.spawn('echo'); } catch (e) { errors.push(String(e)); }
                // 绕过 JS 检查直接打底层: Rust 侧必须仍然拒绝。
                var raw = JSON.parse(globalThis.__z3rm_host_call('workspace.getPath', '[]'));
                if (raw.ok) { throw new Error('raw bypass unexpectedly succeeded'); }
                globalThis.__errors = errors;
            }
        "#;
        let extension = runner.load_live("undeclared-caps", source, "activate")?;
        drop(extension);
        assert_eq!(bridge.calls().len(), 0, "未声明的调用不得到达桥");
        Ok(())
    }

    /// §5.6: io_rate_limit 必须真正限制宿主调用频率。
    #[test]
    fn io_rate_limit_throttles_host_calls() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([(
            "mux.focusPane".to_string(),
            serde_json::json!(true),
        )]));
        // 速率 2/s → 容量 4，第 5 次调用必须被拒绝。
        let runner = ExtensionRunner::with_limits(ExtensionLimits::new(64, 50, 2.0))
            .with_bridge(bridge.clone());
        let source = r#"
            function activate(context) {
                globalThis.__failures = 0;
                for (var i = 0; i < 8; i++) {
                    try { context.mux.focusPane('p' + i); }
                    catch (error) { globalThis.__failures++; }
                }
            }
        "#;
        let extension = runner.load_live("io-limit", source, "activate")?;
        assert_eq!(bridge.calls().len(), 4, "只应放行容量内的调用");
        assert!(
            extension.take_io_violated(),
            "JS 捕获限流异常后，宿主仍必须能观察到违规"
        );
        assert!(!extension.take_io_violated(), "读取违规标志后必须清零");
        Ok(())
    }

    /// §5.6: 一次被 IO 速率上限拒绝的宿主调用必须置位持久违规标志，即使
    /// 扩展的 JS 用 try/catch 吞掉了异常——拒绝发生在 Rust 侧 (令牌桶),
    /// 只有该标志能证明违规，宿主据此挂起扩展。
    #[test]
    fn io_rate_limit_rejection_sets_io_violated_flag() -> Result<()> {
        let bridge = RecordingBridge::new(BTreeMap::from([(
            "mux.focusPane".to_string(),
            serde_json::json!(true),
        )]));
        let runner = ExtensionRunner::with_limits(ExtensionLimits::new(64, 50, 2.0))
            .with_bridge(bridge.clone());

        // 容量内的调用不置位违规标志。
        let source = r#"
            function activate(context) {
                for (var i = 0; i < 4; i++) {
                    try { context.mux.focusPane('p' + i); } catch (error) {}
                }
            }
        "#;
        let within = runner.load_live("io-within-limit", source, "activate")?;
        assert_eq!(bridge.calls().len(), 4, "容量内调用必须放行");
        assert!(
            !within.take_io_violated(),
            "容量内调用不得置位违规标志"
        );

        // 超过容量: 第 5 次起被拒绝，且异常被 JS 吞掉——只有持久标志能证明。
        let source = r#"
            function activate(context) {
                for (var i = 0; i < 8; i++) {
                    try { context.mux.focusPane('p' + i); } catch (error) {}
                }
            }
        "#;
        let over = runner.load_live("io-over-limit", source, "activate")?;
        assert_eq!(bridge.calls().len(), 8, "两次激活合计应放行 4 + 4 次调用");
        assert!(
            over.take_io_violated(),
            "被拒绝的宿主调用必须置位持久 IO 违规标志"
        );
        assert!(
            !over.take_io_violated(),
            "take_io_violated 必须像 memory_violated 一样读取后清零"
        );
        Ok(())
    }

    /// 测试扩展加载桩: 创建临时文件 → 加载 → 验证
    #[test]
    fn test_extension_loading_stub() -> Result<()> {
        let runner = ExtensionRunner::with_defaults();

        let source = r#"
        // 模拟扩展源码
        function activate(context) {
            return { status: "active" };
        }
        function deactivate() {}
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

    // -----------------------------------------------------------------------
    // §5.3 manifest 解析
    // -----------------------------------------------------------------------

    /// P1-2: `[capabilities]` 与 `io_rate_limit` 必须被真正解析。
    #[test]
    fn manifest_parses_capabilities_and_io_rate_limit() -> Result<()> {
        let manifest = parse_manifest_str(
            "z3rm-status-bar",
            r#"
                [extension]
                name = "z3rm-status-bar"
                version = "0.1.0"

                [runtime]
                side = "client"

                [capabilities]
                terminal = true
                mux = true
                workspace = true
                filesystem = "cwd"

                [resources]
                memory_limit_mb = 32
                cpu_budget_ms = 25
                io_rate_limit = 17
            "#,
        )?;

        assert_eq!(manifest.id, "z3rm-status-bar");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.side, ExtensionSide::Client);
        assert!(manifest.capabilities.terminal);
        assert!(manifest.capabilities.mux);
        assert!(manifest.capabilities.workspace);
        assert!(!manifest.capabilities.settings);
        assert_eq!(manifest.capabilities.filesystem, FilesystemAccess::Cwd);
        assert_eq!(manifest.limits.memory_limit_mb, 32);
        assert_eq!(manifest.limits.cpu_budget_ms, 25);
        assert_eq!(manifest.limits.io_rate_limit, 17.0);
        Ok(())
    }

    /// §5.6 fail closed: 没有 `[capabilities]` 就什么都不给。
    #[test]
    fn manifest_without_capabilities_grants_nothing() -> Result<()> {
        let manifest = parse_manifest_str(
            "plain",
            "[extension]\nname = \"plain\"\n[runtime]\nside = \"both\"\nsync = true\n",
        )?;
        assert_eq!(manifest.capabilities, ExtensionCapabilities::default());
        assert!(!manifest.capabilities.allows("mux.listSessions"));
        assert_eq!(manifest.side, ExtensionSide::Both);
        assert!(manifest.sync);
        assert_eq!(manifest.limits, ExtensionLimits::default());
        Ok(())
    }

    #[test]
    fn manifest_rejects_invalid_runtime_side() {
        let result = parse_manifest_str("bad", "[runtime]\nside = \"browser\"\n");
        assert!(result.is_err(), "非法 side 必须 fail closed");
    }

    #[test]
    fn manifest_requires_runtime_section() {
        let result = parse_manifest_str("bad", "[extension]\nname = \"bad\"\n");
        assert!(result.is_err(), "缺少 [runtime] 必须报错");
    }

    /// Zed 遗留 manifest 用 `[[capabilities]]` 数组，语义不同，不应被误读为授权。
    #[test]
    fn manifest_ignores_legacy_capability_arrays() -> Result<()> {
        let manifest = parse_manifest_str(
            "legacy",
            r#"
                id = "legacy"
                name = "Legacy"
                [runtime]
                side = "client"
                [[capabilities]]
                kind = "process:exec"
                command = "echo"
            "#,
        )?;
        assert_eq!(manifest.capabilities, ExtensionCapabilities::default());
        assert_eq!(manifest.name, "Legacy");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // P0-1: 内置扩展必须真的被发现并激活
    // -----------------------------------------------------------------------

    const BUILTIN_EXTENSION_IDS: [&str; 6] = [
        "z3rm-command-palette",
        "z3rm-layout-manager",
        "z3rm-session-manager",
        "z3rm-status-bar",
        "z3rm-tab-bar",
        "z3rm-which-key",
    ];

    /// 一个覆盖全部内置扩展所需方法的桥，让 activate 期的调用有真实返回值。
    struct BuiltinTestBridge;

    impl HostBridge for BuiltinTestBridge {
        fn call(&self, method: &str, _args: &serde_json::Value) -> Result<serde_json::Value> {
            match method {
                "mux.listSessions" => Ok(serde_json::json!([
                    { "id": "s1", "name": "work", "cwd": "/tmp", "clients": 1 }
                ])),
                "mux.currentSession" => {
                    Ok(serde_json::json!({ "id": "s1", "name": "work", "clients": 1 }))
                }
                "mux.focusedPane" => Ok(serde_json::json!({ "id": "p1", "title": "zsh" })),
                other => Ok(serde_json::json!({ "method": other })),
            }
        }
    }

    /// P0-1 的回归测试：仓库里的 6 个内置扩展必须能被发现并成功 activate。
    /// 这条用例同时兜住 P0-2 那类「bootstrap 缺 API → activate 抛异常」的问题。
    #[test]
    fn all_builtin_extensions_discover_and_activate() -> Result<()> {
        let roots = builtin_extension_roots();
        let discovered = discover_client_extensions(&roots);
        assert!(
            !discovered.is_empty(),
            "内置扩展未被发现，搜索根: {roots:?}"
        );

        let discovered_ids: Vec<&str> = discovered
            .iter()
            .map(|extension| extension.manifest.id.as_str())
            .collect();
        for expected in BUILTIN_EXTENSION_IDS {
            assert!(
                discovered_ids.contains(&expected),
                "内置扩展 {expected} 未被发现，实际: {discovered_ids:?}"
            );
        }

        let bridge: Arc<dyn HostBridge> = Arc::new(BuiltinTestBridge);
        for extension in &discovered {
            let identifier = extension.manifest.id.as_str();
            if !BUILTIN_EXTENSION_IDS.contains(&identifier) {
                continue;
            }
            let runner =
                ExtensionRunner::for_manifest(&extension.manifest).with_bridge(bridge.clone());
            let live = runner
                .load_live(identifier, &extension.source, "activate")
                .with_context(|| format!("{identifier} 必须能成功 activate"))?;

            assert!(
                live.needs_render()?,
                "{identifier} 应在 activate 时注册 chrome view"
            );
            // 渲染必须不抛异常（返回 None 表示是按需 chrome，当前处于隐藏态）。
            live.render_all_views()
                .with_context(|| format!("{identifier} 渲染失败"))?;
            let errors = live.take_errors()?;
            assert!(errors.is_empty(), "{identifier} 内部报错: {errors:?}");
        }
        Ok(())
    }

    /// 内置扩展声明的能力必须覆盖它们实际调用的宿主方法。
    #[test]
    fn builtin_manifests_declare_the_capabilities_they_use() -> Result<()> {
        let discovered = discover_client_extensions(&builtin_extension_roots());
        let required_mux = [
            "z3rm-layout-manager",
            "z3rm-session-manager",
            "z3rm-status-bar",
            "z3rm-tab-bar",
        ];
        for identifier in required_mux {
            let extension = discovered
                .iter()
                .find(|extension| extension.manifest.id == identifier)
                .with_context(|| format!("{identifier} 未被发现"))?;
            assert!(
                extension.manifest.capabilities.mux,
                "{identifier} 使用 context.mux.* 但未声明 mux 能力"
            );
        }
        Ok(())
    }

    /// 内置扩展的事件订阅必须真的能收到宿主事件 (P0-3 的 JS 侧契约)。
    #[test]
    fn builtin_status_bar_updates_from_pane_focus_event() -> Result<()> {
        let discovered = discover_client_extensions(&builtin_extension_roots());
        let status_bar = discovered
            .iter()
            .find(|extension| extension.manifest.id == "z3rm-status-bar")
            .context("z3rm-status-bar 未被发现")?;

        let runner = ExtensionRunner::for_manifest(&status_bar.manifest);
        let live = runner.load_live("z3rm-status-bar", &status_bar.source, "activate")?;

        let delivered = live.emit_event(
            "pane:focus",
            r#"{"title":"vim","sessionName":"work","paneId":"p1"}"#,
        )?;
        assert_eq!(delivered, 1, "pane:focus 必须投递到 status-bar 订阅者");

        let vdom = live.render_now()?.context("status-bar 应产生 VDOM")?;
        assert!(vdom.contains("vim"), "pane 标题应出现在 VDOM: {vdom}");
        assert!(vdom.contains("work"), "session 名应出现在 VDOM: {vdom}");
        Ok(())
    }

    /// tab-bar 依赖 `tab:title` 事件填充标签，事件断链时标签栏恒空。
    #[test]
    fn builtin_tab_bar_updates_from_tab_title_event() -> Result<()> {
        let discovered = discover_client_extensions(&builtin_extension_roots());
        let tab_bar = discovered
            .iter()
            .find(|extension| extension.manifest.id == "z3rm-tab-bar")
            .context("z3rm-tab-bar 未被发现")?;

        let runner = ExtensionRunner::for_manifest(&tab_bar.manifest);
        let live = runner.load_live("z3rm-tab-bar", &tab_bar.source, "activate")?;

        assert_eq!(
            live.emit_event(
                "tab:title",
                r#"{"tabId":"t1","title":"build","paneId":"p1","active":true}"#
            )?,
            1
        );
        let vdom = live.render_now()?.context("tab-bar 应产生 VDOM")?;
        assert!(vdom.contains("build"), "vdom={vdom}");
        Ok(())
    }

    /// command-palette 依赖 `commands.list()`；桩实现下它永远是空列表。
    #[test]
    fn builtin_command_palette_lists_registered_commands() -> Result<()> {
        let discovered = discover_client_extensions(&builtin_extension_roots());
        let palette = discovered
            .iter()
            .find(|extension| extension.manifest.id == "z3rm-command-palette")
            .context("z3rm-command-palette 未被发现")?;

        let runner = ExtensionRunner::for_manifest(&palette.manifest);
        let live = runner.load_live("z3rm-command-palette", &palette.source, "activate")?;

        let commands: serde_json::Value = serde_json::from_str(&live.list_commands()?)?;
        let ids: Vec<&str> = commands
            .as_array()
            .context("commands.list must be an array")?
            .iter()
            .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
            .collect();
        assert!(
            ids.contains(&"z3rm.command-palette.open"),
            "palette 命令未注册: {ids:?}"
        );

        assert!(live.execute_command("z3rm.command-palette.open", "[]")?);
        let vdom = live.render_now()?.context("打开后 palette 应产生 VDOM")?;
        assert!(vdom.contains("command-palette"), "vdom={vdom}");
        assert!(
            vdom.contains("z3rm.command-palette.open"),
            "palette 应列出自身命令: {vdom}"
        );

        let keymaps: serde_json::Value = serde_json::from_str(&live.list_keymaps()?)?;
        assert!(
            keymaps
                .as_array()
                .is_some_and(|entries| !entries.is_empty()),
            "keymaps.bind 必须被记录: {keymaps}"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // §16.8 运行侧分派
    // -----------------------------------------------------------------------

    /// §16.8: `side` 声明决定扩展在哪个进程运行；`Both` 两侧都跑。
    #[test]
    fn extension_side_dispatch_matrix() {
        assert!(ExtensionSide::Client.runs_on_client());
        assert!(!ExtensionSide::Client.runs_on_server());
        assert!(!ExtensionSide::Server.runs_on_client());
        assert!(ExtensionSide::Server.runs_on_server());
        assert!(ExtensionSide::Both.runs_on_client());
        assert!(ExtensionSide::Both.runs_on_server());
    }

    /// §16.8: 客户端/服务端发现必须各自只返回声明属于该侧的扩展。
    #[test]
    fn discover_extensions_dispatch_by_declared_side() -> Result<()> {
        let root = tempfile::tempdir()?;
        let write_extension = |id: &str, side: &str| -> Result<()> {
            let directory = root.path().join(id);
            std::fs::create_dir_all(&directory)?;
            std::fs::write(
                directory.join("extension.toml"),
                format!("[extension]\nname = \"{id}\"\n[runtime]\nside = \"{side}\"\n"),
            )?;
            std::fs::write(directory.join("main.js"), "function activate() {}")?;
            Ok(())
        };
        write_extension("client-only", "client")?;
        write_extension("server-only", "server")?;
        write_extension("both-sides", "both")?;

        let root_path = root.path().to_path_buf();
        let roots = std::slice::from_ref(&root_path);
        let client_discovered = discover_client_extensions(roots);
        let client_ids: Vec<&str> = client_discovered
            .iter()
            .map(|extension| extension.manifest.id.as_str())
            .collect();
        assert_eq!(
            client_ids,
            vec!["both-sides", "client-only"],
            "客户端不得发现 server-only 扩展"
        );

        let server_discovered = discover_server_extensions(roots);
        let server_ids: Vec<&str> = server_discovered
            .iter()
            .map(|extension| extension.manifest.id.as_str())
            .collect();
        assert_eq!(
            server_ids,
            vec!["both-sides", "server-only"],
            "服务端不得发现 client-only 扩展"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // §5.2/§5.6 资源声明强制
    // -----------------------------------------------------------------------

    /// §5.6: `io_rate_limit = 0` 表示不限流，不能静默套上默认速率。
    #[test]
    fn io_rate_limit_zero_declares_unlimited() -> Result<()> {
        let manifest = parse_manifest_str(
            "unlimited-io",
            "[extension]\nname = \"u\"\n[runtime]\nside = \"client\"\n[capabilities]\nmux = true\n[resources]\nio_rate_limit = 0\n",
        )?;
        assert_eq!(manifest.limits.io_rate_limit, 0.0);

        let bucket = IoTokenBucket::from_rate(0.0);
        for _ in 0..10_000 {
            assert!(bucket.try_acquire(1.0), "io_rate_limit = 0 必须永不拒绝");
        }

        // 端到端: 声明 0 的扩展宿主调用全部放行 (对照 io_rate_limit_throttles_host_calls)。
        let bridge = RecordingBridge::new(BTreeMap::from([(
            "mux.focusPane".to_string(),
            serde_json::json!(true),
        )]));
        let runner = ExtensionRunner::for_manifest(&manifest).with_bridge(bridge.clone());
        let source = r#"
            function activate(context) {
                globalThis.__failures = 0;
                for (var i = 0; i < 100; i++) {
                    try { context.mux.focusPane('p' + i); }
                    catch (error) { globalThis.__failures++; }
                }
            }
        "#;
        let extension = runner.load_live("io-unlimited", source, "activate")?;
        assert_eq!(bridge.calls().len(), 100, "io_rate_limit = 0 不应限流");
        drop(extension);
        Ok(())
    }

    /// §5.6: 内存超限被 bootstrap 的 try/catch 收进错误列表时，`take_errors`
    /// 必须把它翻成内存违规标志，宿主才能挂起扩展。
    #[test]
    fn memory_violation_is_recorded_and_taken() -> Result<()> {
        let runner = ExtensionRunner::new(1, 50); // 1MB 内存上限
        let source = r#"
            function activate(context) {
                context.registerChromeView('leaky', {
                    render: function() {
                        var blocks = [];
                        for (var i = 0; i < 10000000; i++) { blocks.push(new Array(1000)); }
                        return { type: 'span', children: ['x'] };
                    }
                });
            }
        "#;
        let extension = runner.load_live("leaky-ext", source, "activate")?;
        assert!(
            !extension.take_memory_violated(),
            "激活成功时不应当有内存违规"
        );

        let rendered = extension.render_all_views();
        let errors = extension.take_errors()?;
        let memory_exceeded = extension.memory_exceeded();
        let memory_violated = extension.take_memory_violated();
        assert!(
            rendered.is_err() || !errors.is_empty() || memory_exceeded,
            "the over-limit renderer must fail or report an error"
        );
        assert!(
            memory_violated
                || memory_exceeded
                || rendered
                    .as_ref()
                    .err()
                    .is_some_and(|error| format!("{error:#}").contains("out of memory"))
                || errors.iter().any(|error| error.contains("out of memory")),
            "the over-limit renderer must mark a memory violation"
        );
        Ok(())
    }
}
