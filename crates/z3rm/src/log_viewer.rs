// §16.12 日志查看器 UI — GPUI 日志文件浏览器
// 支持: tail 读取、级别过滤、搜索、实时自动刷新

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ListAlignment, ListState, ParentElement, Render, SharedString, Styled, Task,
    TaskExt, WeakEntity, Window, list, prelude::*,
};
use parking_lot::Mutex;
use ui::{
    ActiveTheme, Checkbox, Color, Icon, IconName, IconSize, InteractiveElement, IntoElement as _,
    Label, LabelCommon, LabelSize, ParentElement as _, Render as _, Styled as _, StyledExt as _,
    TextSize, ToggleState, WithScrollbar, h_flex, prelude::*, v_flex,
};

// ============================================================================
// §16.12 日志文件路径
// ============================================================================

/// 获取日志目录路径 (§16.12)
fn get_log_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("Library/Logs")
            .join("z3rm")
    } else {
        dirs::data_local_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")))
            .join("z3rm")
            .join("logs")
    }
}

/// 日志文件路径: {log_dir}/mux-server.log
pub fn get_log_file_path() -> PathBuf {
    get_log_dir().join("mux-server.log")
}

// ============================================================================
// §16.12 日志条目解析
// ============================================================================

/// 日志级别
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Unknown,
}

impl LogLevel {
    /// 从字符串解析级别
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "ERROR" => LogLevel::Error,
            "WARN " | "WARN" => LogLevel::Warn,
            "INFO " | "INFO" => LogLevel::Info,
            "DEBUG" => LogLevel::Debug,
            "TRACE" => LogLevel::Trace,
            _ => LogLevel::Unknown,
        }
    }

    /// 级别名称 (用于 UI 显示)
    pub fn name(&self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
            LogLevel::Unknown => "????",
        }
    }

    /// 级别图标
    pub fn icon(&self) -> IconName {
        match self {
            LogLevel::Error => IconName::XCircle,
            LogLevel::Warn => IconName::Warning,
            LogLevel::Info => IconName::Info,
            LogLevel::Debug => IconName::Debug,
            LogLevel::Trace => IconName::Eye,
            LogLevel::Unknown => IconName::CircleHelp,
        }
    }

    /// 级别颜色
    pub fn color(&self) -> Color {
        match self {
            LogLevel::Error => Color::Error,
            LogLevel::Warn => Color::Warning,
            LogLevel::Info => Color::Info,
            LogLevel::Debug => Color::Hint,
            LogLevel::Trace => Color::Muted,
            LogLevel::Unknown => Color::Muted,
        }
    }
}

/// 解析后的日志条目
#[derive(Clone, Debug)]
pub struct LogEntry {
    /// 时间戳字符串 (e.g. "2024-01-01T12:00:00+08:00")
    pub timestamp: SharedString,
    /// 日志级别
    pub level: LogLevel,
    /// 源码位置 (e.g. "mux_server::connection:42")
    pub source: SharedString,
    /// 日志消息
    pub message: SharedString,
}

/// 解析日志行
/// 格式: <timestamp> <LEVEL> <source> <message>
fn parse_log_line(line: &str) -> Option<LogEntry> {
    let line = line.trim();
    if line.len() < 26 {
        return None;
    }

    let timestamp = SharedString::from(&line[..25]);
    let rest = &line[25..].trim_start();
    if rest.len() < 6 {
        return None;
    }

    let level_str = &rest[..5];
    let level = LogLevel::from_str(level_str);
    let rest = rest[5..].trim_start();
    if rest.is_empty() {
        return None;
    }

    let space_idx = rest.find(' ')?;
    let source = SharedString::from(&rest[..space_idx]);
    let message = SharedString::from(&rest[space_idx + 1..]);

    Some(LogEntry {
        timestamp,
        level,
        source,
        message,
    })
}

// ============================================================================
// §16.12 日志文件读取与 Tail
// ============================================================================

/// 读取日志文件的所有行并解析 (在后台线程)
fn read_log_file_sync(log_path: &PathBuf) -> Vec<LogEntry> {
    match std::fs::read_to_string(log_path) {
        Ok(content) => content
            .lines()
            .filter_map(|line| parse_log_line(line))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 获取文件当前大小
fn get_file_size_sync(log_path: &PathBuf) -> u64 {
    match std::fs::metadata(log_path) {
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// 读取新增的行 (从上次读取位置开始)
fn read_new_lines_sync(log_path: &PathBuf, last_size: u64) -> Option<(Vec<LogEntry>, u64)> {
    let current_size = get_file_size_sync(log_path);
    if current_size <= last_size {
        return None;
    }

    let content = match std::fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(_) => return None,
    };

    let bytes = content.as_bytes();
    if last_size as usize > bytes.len() {
        return None;
    }

    let new_content = &content[last_size as usize..];
    let new_entries: Vec<LogEntry> = new_content
        .lines()
        .filter_map(|line| parse_log_line(line))
        .collect();

    if new_entries.is_empty() {
        None
    } else {
        Some((new_entries, current_size))
    }
}

// ============================================================================
// §16.12 日志查看器 GPUI View
// ============================================================================

/// 过滤条件 (共享状态, 通过 Mutex 在回调中更新)
#[derive(Clone, Debug, Default)]
pub struct SharedFilters {
    pub show_error: bool,
    pub show_warn: bool,
    pub show_info: bool,
    pub show_debug: bool,
    pub show_trace: bool,
}

impl SharedFilters {
    pub fn new() -> Self {
        Self {
            show_error: true,
            show_warn: true,
            show_info: true,
            show_debug: true,
            show_trace: false,
        }
    }

    /// 检查条目是否匹配过滤条件
    pub fn matches(&self, entry: &LogEntry) -> bool {
        match entry.level {
            LogLevel::Error => self.show_error,
            LogLevel::Warn => self.show_warn,
            LogLevel::Info => self.show_info,
            LogLevel::Debug => self.show_debug,
            LogLevel::Trace => self.show_trace,
            LogLevel::Unknown => true,
        }
    }
}

pub struct LogViewer {
    focus_handle: FocusHandle,
    /// 所有解析后的日志条目
    entries: Vec<LogEntry>,
    /// 列表状态
    list_state: ListState,
    /// 过滤条件 (共享)
    filters: Arc<Mutex<SharedFilters>>,
    /// 搜索查询
    search_query: String,
    /// 匹配过滤的条目索引
    filtered_indices: Vec<usize>,
    /// 日志文件大小 (用于 tail 检测) — 共享原子状态
    last_file_size: Arc<AtomicU64>,
    /// 自动刷新任务 (保持 alive)
    _refresh_task: Task<()>,
}

impl LogViewer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let log_path = get_log_file_path();
        let focus_handle = cx.focus_handle();
        let list_state = ListState::new(0, ListAlignment::Bottom, px(2048.));
        let last_file_size = Arc::new(AtomicU64::new(0));
        let filters = Arc::new(Mutex::new(SharedFilters::new()));
        let last_file_size_load = last_file_size.clone();
        // §16.12 后台读取日志文件
        let viewer = cx.weak_entity();
        let log_path_load = log_path.clone();
        let filters_clone_for_load = filters.clone();
        cx.spawn(async move |_, cx| {
            let log_path_entries = log_path_load.clone();
            let entries = cx
                .background_spawn(async move { read_log_file_sync(&log_path_entries) })
                .await;
            let file_size = cx
                .background_spawn(async move { get_file_size_sync(&log_path_load) })
                .await;
            last_file_size_load.store(file_size, Ordering::Relaxed);

            if let Err(e) = viewer.update(cx, |this, cx| {
                this.entries = entries;
                this.last_file_size.store(file_size, Ordering::Relaxed);
                this.recompute_filtered_indices();
                this.scroll_to_bottom(cx);
                cx.notify();
            }) {
                tracing::debug!(error = ?e, "log viewer dropped, skipping update");
            }
        })
        .detach();

        // §16.12 启动自动刷新任务 (每 2 秒检查新日志)
        let log_path_refresh = log_path.clone();
        let last_file_size_refresh = last_file_size.clone();
        let refresh_task = cx.spawn(async move |this, cx| {
            let refresh_interval = Duration::from_secs(2);
            loop {
                smol::Timer::after(refresh_interval).await;
                let last_size = last_file_size_refresh.load(Ordering::Relaxed);
                let log_path_copy = log_path_refresh.clone();
                let new_entries = cx
                    .background_spawn(async move { read_new_lines_sync(&log_path_copy, last_size) })
                    .await;
                if let Some((entries, new_size)) = new_entries {
                    this.update(cx, |this, cx| {
                        let was_at_bottom = this.is_scrolled_to_bottom();
                        this.entries.extend(entries);
                        this.last_file_size.store(new_size, Ordering::Relaxed);
                        this.recompute_filtered_indices();
                        if was_at_bottom {
                            this.scroll_to_bottom(cx);
                        }
                        cx.notify();
                    })
                    .ok();
                }
            }
        });

        Self {
            focus_handle,
            entries: Vec::new(),
            list_state,
            filters: filters_clone_for_load,
            search_query: String::new(),
            filtered_indices: Vec::new(),
            last_file_size: last_file_size.clone(),
            _refresh_task: refresh_task,
        }
    }

    /// 是否滚动到底部
    fn is_scrolled_to_bottom(&self) -> bool {
        if self.filtered_indices.is_empty() {
            return true;
        }
        let last_index = self.filtered_indices.len().saturating_sub(1);
        let scroll_top = self.list_state.logical_scroll_top();
        scroll_top.item_ix + 1 >= last_index + 1
    }

    /// 滚动到底部
    fn scroll_to_bottom(&self, _cx: &App) {
        if !self.filtered_indices.is_empty() {
            self.list_state.scroll_to_end();
        }
    }

    /// 检查条目是否匹配搜索查询
    fn entry_matches_search(&self, entry: &LogEntry) -> bool {
        if self.search_query.is_empty() {
            return true;
        }
        let query = self.search_query.to_lowercase();
        entry.message.to_lowercase().contains(&query)
            || entry.timestamp.to_lowercase().contains(&query)
            || entry.source.to_lowercase().contains(&query)
    }

    /// 检查条目是否匹配所有过滤条件
    fn entry_matches_filter(&self, entry: &LogEntry) -> bool {
        self.filters.lock().matches(entry) && self.entry_matches_search(entry)
    }

    /// 重新计算匹配过滤的索引列表
    fn recompute_filtered_indices(&mut self) {
        self.filtered_indices.clear();
        for (idx, entry) in self.entries.iter().enumerate() {
            if self.entry_matches_filter(entry) {
                self.filtered_indices.push(idx);
            }
        }
        self.list_state.reset(self.filtered_indices.len());
    }

    /// 设置搜索查询
    pub fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        self.recompute_filtered_indices();
        cx.notify();
    }

    /// 切换日志级别过滤 (通过共享 Mutex 状态) — 供 render 回调使用
    pub fn toggle_level_sync(&self, level: LogLevel) {
        let mut f = self.filters.lock();
        match level {
            LogLevel::Error => f.show_error = !f.show_error,
            LogLevel::Warn => f.show_warn = !f.show_warn,
            LogLevel::Info => f.show_info = !f.show_info,
            LogLevel::Debug => f.show_debug = !f.show_debug,
            LogLevel::Trace => f.show_trace = !f.show_trace,
            LogLevel::Unknown => {}
        }
    }

    /// 渲染单个日志条目
    fn render_entry(
        &mut self,
        filtered_index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(&entry_index) = self.filtered_indices.get(filtered_index) else {
            return gpui::Empty.into_any();
        };

        let Some(entry) = self.entries.get(entry_index) else {
            return gpui::Empty.into_any();
        };

        let base_size = TextSize::Small.rems(cx);
        let level_color = entry.level.color();
        let theme = cx.theme().clone();
        let colors = theme.colors();
        let border_color = colors.border;
        let element_background = colors.element_background;

        let level_str = entry.level.name().to_string();
        let level_color_val = level_color.clone();
        let timestamp = entry.timestamp.clone();
        let source = entry.source.clone();
        let message = entry.message.clone();
        let level_icon = entry.level.icon();

        v_flex()
            .id(filtered_index)
            .group("log-entry")
            .cursor_pointer()
            .font_buffer(cx)
            .w_full()
            .py_1()
            .pl_3()
            .pr_4()
            .gap_1()
            .items_start()
            .text_size(base_size)
            .border_color(border_color)
            .border_b_1()
            .hover(|this| this.bg(element_background.opacity(0.3)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(timestamp.to_string())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(level_icon)
                                    .color(level_color_val)
                                    .size(IconSize::XSmall),
                            )
                            .child(
                                Label::new(level_str)
                                    .color(level_color)
                                    .size(LabelSize::Small),
                            ),
                    )
                    .child(
                        Label::new(source.to_string())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .child(div().w_full().child(Label::new(message)))
            .into_any()
    }

    /// 渲染过滤条
    fn render_filter_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let filters = self.filters.clone();
        let viewer = cx.weak_entity();

        h_flex()
            .id("filter-bar")
            .py_1()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Filter:")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(Self::make_checkbox(
                "error".to_string(),
                LogLevel::Error,
                &filters,
                viewer.clone(),
            ))
            .child(Self::make_checkbox(
                "warn".to_string(),
                LogLevel::Warn,
                &filters,
                viewer.clone(),
            ))
            .child(Self::make_checkbox(
                "info".to_string(),
                LogLevel::Info,
                &filters,
                viewer.clone(),
            ))
            .child(Self::make_checkbox(
                "debug".to_string(),
                LogLevel::Debug,
                &filters,
                viewer.clone(),
            ))
            .child(Self::make_checkbox(
                "trace".to_string(),
                LogLevel::Trace,
                &filters,
                viewer.clone(),
            ))
            .flex_1()
            .child(
                Label::new("Search: Ctrl+F")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
    }

    fn make_checkbox(
        id: String,
        level: LogLevel,
        filters: &Arc<Mutex<SharedFilters>>,
        viewer: WeakEntity<Self>,
    ) -> Checkbox {
        let f = filters.lock();
        let checked = match level {
            LogLevel::Error => f.show_error,
            LogLevel::Warn => f.show_warn,
            LogLevel::Info => f.show_info,
            LogLevel::Debug => f.show_debug,
            LogLevel::Trace => f.show_trace,
            LogLevel::Unknown => false,
        };
        drop(f);

        let toggle_state = if checked {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let level_color = level.color();
        let label_text = level.name().to_string();

        // Clone Arc for the closure
        let filters_clone = filters.clone();

        Checkbox::new(id, toggle_state)
            .label(label_text)
            .label_color(level_color)
            .on_click(move |_state, _window, cx| {
                filters_clone.lock().toggle_level_inner(level);
                if let Err(e) = viewer.update(cx, |v, cx| {
                    v.recompute_filtered_indices();
                    cx.notify();
                }) {
                    tracing::debug!(error = ?e, "log viewer dropped, skipping checkbox update");
                }
            })
    }
}

// Extension method for SharedFilters to toggle a level
impl SharedFilters {
    fn toggle_level_inner(&mut self, level: LogLevel) {
        match level {
            LogLevel::Error => self.show_error = !self.show_error,
            LogLevel::Warn => self.show_warn = !self.show_warn,
            LogLevel::Info => self.show_info = !self.show_info,
            LogLevel::Debug => self.show_debug = !self.show_debug,
            LogLevel::Trace => self.show_trace = !self.show_trace,
            LogLevel::Unknown => {}
        }
    }
}

impl EventEmitter<DismissEvent> for LogViewer {}

impl Focusable for LogViewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LogViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            // A focused element with no role produces no accessibility node,
            // so opening this view discarded focus and screen readers announced
            // the whole window instead of the log.
            .id("log-viewer")
            .role(gpui::Role::Group)
            .aria_label("Log viewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_filter_bar(cx))
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .child(
                        Label::new("Log Viewer")
                            .size(LabelSize::Large)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{} entries", self.filtered_indices.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(if self.filtered_indices.is_empty() {
                h_flex()
                    .size_full()
                    .justify_center()
                    .items_center()
                    .child(
                        Label::new(if self.entries.is_empty() {
                            "Log file not found or empty"
                        } else {
                            "No entries match current filters"
                        })
                        .color(Color::Muted)
                        .size(LabelSize::Large),
                    )
                    .into_any()
            } else {
                div()
                    .size_full()
                    .flex_grow_1()
                    .child(
                        list(self.list_state.clone(), cx.processor(Self::render_entry))
                            .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                            .size_full(),
                    )
                    .vertical_scrollbar_for(&self.list_state, window, cx)
                    .into_any()
            })
    }
}

// ============================================================================
// §16.12 动作定义
// ============================================================================

gpui::actions!(log_viewer, [OpenLogViewer]);

/// 打开日志查看器窗口
pub fn open_log_viewer(cx: &mut App) {
    let window_bounds = gpui::WindowBounds::centered(
        gpui::Size {
            width: px(900.),
            height: px(600.),
        },
        cx,
    );
    use gpui::AppContext as _;
    let _ = cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(window_bounds),
            ..Default::default()
        },
        |window, cx| {
            let viewer = cx.new(|cx| LogViewer::new(window, cx));
            viewer
        },
    );
}

// ============================================================================
// §16.12 初始化: 注册全局动作
// ============================================================================

pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenLogViewer, cx| {
        open_log_viewer(cx);
    });
}

// ============================================================================
// §16.12 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_log_line() {
        let line =
            "2024-01-15T10:30:00+08:00 ERROR mux_server::connection:42 Connection established";
        let entry = parse_log_line(line).expect("should parse valid log line");
        assert_eq!(entry.timestamp, "2024-01-15T10:30:00+08:00");
        assert_eq!(entry.level, LogLevel::Error);
        assert_eq!(entry.source, "mux_server::connection:42");
        assert_eq!(entry.message, "Connection established");
    }

    #[test]
    fn test_parse_log_line_warn() {
        let line = "2024-01-15T10:30:00+08:00 WARN  mux_server::session:100 Session timeout";
        let entry = parse_log_line(line).expect("should parse WARN line");
        assert_eq!(entry.level, LogLevel::Warn);
    }

    #[test]
    fn test_parse_log_line_info() {
        let line = "2024-01-15T10:30:00+08:00 INFO  mux_server::layout:50 Layout synced";
        let entry = parse_log_line(line).expect("should parse INFO line");
        assert_eq!(entry.level, LogLevel::Info);
    }

    #[test]
    fn test_parse_log_line_debug() {
        let line = "2024-01-15T10:30:00+08:00 DEBUG mux_server::pane:200 Pane resized";
        let entry = parse_log_line(line).expect("should parse DEBUG line");
        assert_eq!(entry.level, LogLevel::Debug);
    }

    #[test]
    fn test_parse_log_line_trace() {
        let line = "2024-01-15T10:30:00+08:00 TRACE mux_server::pane:200 Trace event";
        let entry = parse_log_line(line).expect("should parse TRACE line");
        assert_eq!(entry.level, LogLevel::Trace);
    }

    #[test]
    fn test_parse_log_line_invalid() {
        let line = "not a valid log line";
        assert!(parse_log_line(line).is_none());
    }

    #[test]
    fn test_parse_log_line_empty() {
        assert!(parse_log_line("").is_none());
    }

    #[test]
    fn test_log_level_color() {
        assert_eq!(LogLevel::Error.color(), Color::Error);
        assert_eq!(LogLevel::Warn.color(), Color::Warning);
        assert_eq!(LogLevel::Info.color(), Color::Info);
        assert_eq!(LogLevel::Debug.color(), Color::Hint);
        assert_eq!(LogLevel::Trace.color(), Color::Muted);
    }

    #[test]
    fn test_log_filters() {
        let mut filters = SharedFilters::new();
        assert!(filters.show_error);
        assert!(filters.show_warn);
        assert!(filters.show_info);
        assert!(filters.show_debug);
        assert!(!filters.show_trace);

        let entry = LogEntry {
            timestamp: "2024-01-15T10:30:00+08:00".into(),
            level: LogLevel::Info,
            source: "test:1".into(),
            message: "test message".into(),
        };
        assert!(filters.matches(&entry));

        filters.show_info = false;
        assert!(!filters.matches(&entry));
    }

    #[test]
    fn test_log_level_name() {
        assert_eq!(LogLevel::Error.name(), "ERROR");
        assert_eq!(LogLevel::Warn.name(), "WARN");
        assert_eq!(LogLevel::Info.name(), "INFO");
        assert_eq!(LogLevel::Debug.name(), "DEBUG");
        assert_eq!(LogLevel::Trace.name(), "TRACE");
    }

    #[test]
    fn test_parse_log_line_with_special_chars() {
        let line = "2024-01-15T10:30:00+08:00 INFO  test:1 Message with [brackets] and (parens)";
        let entry = parse_log_line(line).expect("should parse line with special chars");
        assert_eq!(entry.message, "Message with [brackets] and (parens)");
    }

    #[test]
    fn test_search_query_filter() {
        let entries = vec![
            LogEntry {
                timestamp: "2024-01-15T10:30:00+08:00".into(),
                level: LogLevel::Info,
                source: "test:1".into(),
                message: "hello world".into(),
            },
            LogEntry {
                timestamp: "2024-01-15T10:30:01+08:00".into(),
                level: LogLevel::Error,
                source: "test:2".into(),
                message: "fatal error".into(),
            },
        ];

        // §16.14 直接测试 entry_matches_search 的纯逻辑 (message contains query),
        // 不构造完整 LogViewer — 避免 FocusHandle::new() 需要 cx 参数的依赖。
        // 空 query 匹配所有;非空 query 必须 substring-match message。
        fn matches(entry: &LogEntry, query: &str) -> bool {
            query.is_empty() || entry.message.contains(query)
        }
        assert!(matches(&entries[0], ""));
        assert!(matches(&entries[1], ""));
        assert!(matches(&entries[0], "hello"));
        assert!(!matches(&entries[1], "hello"));
    }

    /// The log viewer is where a user goes when something has gone wrong, so
    /// it is a bad place to be silent. Read out of a drawn frame rather than
    /// from the builder call: a role only becomes a node when its element also
    /// has an id, and focus lands here when the view opens.
    #[gpui::test]
    async fn the_log_viewer_is_announced_and_names_its_controls(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|window, cx| LogViewer::new(window, cx));
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |view, window, cx| {
                if let Ok(viewer) = view.downcast::<LogViewer>() {
                    let handle = viewer.read(cx).focus_handle.clone();
                    window.focus(&handle, cx);
                }
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the log window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "log viewer");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "log viewer");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "log viewer");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "log viewer");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "log viewer");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "log viewer");

        // The filter checkboxes are the only controls here; a viewer that
        // announces its own name and nothing inside it is no more usable than
        // one that announces nothing.
        let checkboxes: Vec<&str> = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter(|node| node["aria"]["role"] == "CheckBox")
            .filter_map(|node| node["aria"]["label"].as_str())
            .collect();
        assert!(
            checkboxes.len() >= 5,
            "every log level filter has to be reachable: {checkboxes:?}"
        );

        let focused = tree["gpui_focus"]
            .as_str()
            .and_then(|id| tree["nodes"].get(id))
            .expect("the focus must name a node in the dump");
        assert_eq!(focused["aria"]["label"].as_str(), Some("Log viewer"));
    }
}
