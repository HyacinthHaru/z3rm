// §3.10 Session 模块 — 会话生命周期、标签页、附加客户端。
// 每个 session 包含多个 tab，每个 tab 包含多个 pane。

use crate::layout::LayoutTree;
use crate::pane::Pane;
use std::collections::HashMap;
use std::sync::Arc;
use mux_protocol::proto::envelope::Payload as EnvelopePayload;
use mux_protocol::{Envelope, Notification};

/// 会话状态 (§3.2)
#[derive(Clone)]
pub struct Session {
    /// 会话唯一 ID
    pub id: String,
    /// 会话名称 (§3.10 SessionInfo.name)
    pub name: String,
    /// 工作目录 (§3.10 SessionInfo.cwd)
    pub cwd: String,
    /// 创建时间戳 (Unix 毫秒)
    pub created_timestamp: u64,
    /// 标签页集合: tab_id → Tab
    pub tabs: HashMap<String, Tab>,
    /// 布局树 (§3.10 LayoutTree)
    pub layout: LayoutTree,
    /// 当前焦点 pane 的 ID
    pub focused_pane: Option<String>,
    /// 当前焦点 tab 的 ID
    pub focused_tab: Option<String>,
    /// 已附加的客户端列表
    pub attached_clients: Arc<parking_lot::RwLock<Vec<AttachedClient>>>,
    /// Pane 注册表: pane_id → Arc<Pane> (Arc 因为 PTY read 线程 + 多订阅者持有)
    pub panes: Arc<parking_lot::RwLock<HashMap<String, std::sync::Arc<crate::pane::Pane>>>>,
    /// §16.9 会话级同步滚动状态
    pub sync_scrollback: Arc<parking_lot::RwLock<SyncScrollbackState>>,
    /// §3.3 已连接的窗口 ID 列表 (多窗口支持，Plan 32)
    pub connected_windows: Arc<parking_lot::RwLock<Vec<String>>>,
    /// §3.4 会话级 lifecycle 通知订阅者: client_id → 该连接的 outbound channel。
    /// 承载 PaneAdded / PaneRemoved / SessionLayoutChanged (§3.4 at-least-once 路径)。
    /// 与 `attached_clients` 分离: attached_clients 是 §3.10 客户端状态 (role / mode),
    /// lifecycle_subscribers 是 §3.4 通知投递通道, 必须在 attach 时注册、断连时退订,
    /// 否则单连接广播会漏掉其他 attached 客户端。
    pub lifecycle_subscribers:
        Arc<parking_lot::RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<Envelope>>>>,
    /// §4 Shadow snapshot watcher handle: cwd file changes → snapshot engine.
    /// `None` means this session has no live watcher (cwd unusable / recovered /
    /// test session). Arc so it survives Session derive(Clone) clones; the last
    /// clone dropped stops the watcher + recorder (see snapshot::SnapshotWatch).
    pub snapshot_watch: Option<std::sync::Arc<crate::snapshot::SnapshotWatch>>,
}

/// 标签页 (§3.10 TabInfo)
#[derive(Clone, Debug)]
pub struct Tab {
    /// 标签 ID
    pub id: String,
    /// 标签标题 (§3.10 TabInfo.title)
    pub title: String,
    /// Pane ID 列表
    pub pane_ids: Vec<String>,
}

/// 附加客户端 (§3.10 AttachRequest)
#[derive(Clone, Debug)]
pub struct AttachedClient {
    /// 客户端唯一 ID
    pub client_id: String,
    /// 连接模式: shared / steal / read_only
    pub mode: AttachMode,
    /// §3.3 窗口 ID (多窗口支持，Plan 32)
    pub window_id: Option<String>,
    /// §3.3 客户端角色 (Plan 33)
    pub role: ClientRole,
}

/// §3.3 客户端角色 (Plan 33)
/// ReadOnly: 只能读取，不能修改 pane
/// ReadWrite: 可执行 pane 操作
/// Admin: 所有操作包括 kill/rename/install
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Default)]
pub enum ClientRole {
    /// 只读: 只能读取，不能写入
    ReadOnly,
    /// 读写: 可执行 pane 操作 (默认)
    #[default]
    ReadWrite,
    /// 管理员: 所有操作包括管理命令
    Admin,
}

/// 连接模式 (§3.10 AttachRequest.AttachMode)
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AttachMode {
    /// 共享模式: 多个客户端可同时连接
    Shared,
    /// 抢占模式: 断开其他客户端
    Steal,
    /// 只读模式: 只能读取，不能写入
    ReadOnly,
}

/// §16.9 会话级同步滚动状态
#[derive(Clone, Debug, Default)]
pub struct SyncScrollbackState {
    /// 当前同步滚动 pane 的 ID
    pub pane_id: Option<String>,
    /// 同步滚动偏移量
    pub scroll_offset: u32,
    /// 是否启用同步滚动
    pub enabled: bool,
}

impl Session {
    /// 创建新 session (§3.2)
    pub fn new(id: String, name: String, cwd: String) -> Self {
        Self {
            id,
            name,
            cwd,
            created_timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            tabs: HashMap::new(),
            layout: LayoutTree::empty(),
            focused_pane: None,
            focused_tab: None,
            attached_clients: Arc::new(parking_lot::RwLock::new(Vec::new())),
            panes: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            sync_scrollback: Arc::new(parking_lot::RwLock::new(SyncScrollbackState::default())),
            connected_windows: Arc::new(parking_lot::RwLock::new(Vec::new())),
            lifecycle_subscribers: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            snapshot_watch: None,
        }
    }

    pub fn add_tab(&mut self, id: String, title: String) {
        let tab = Tab {
            id: id.clone(),
            title,
            pane_ids: Vec::new(),
        };
        self.tabs.insert(id, tab);
    }

    /// 获取焦点 pane 的 ID
    pub fn get_focused_pane(&self) -> Option<&str> {
        self.focused_pane.as_deref()
    }

    /// 设置焦点 pane (§3.10 FocusPaneRequest)
    pub fn set_focused_pane(&mut self, pane_id: String) {
        self.focused_pane = Some(pane_id);
    }

    /// Remove a pane from every server-authoritative session index.
    /// Returns `false` when another cleanup path already removed it.
    pub fn remove_pane(&mut self, pane_id: &str) -> anyhow::Result<bool> {
        if !self.panes.read().contains_key(pane_id) {
            return Ok(false);
        }

        let layout_panes = self.layout.pane_ids();
        if layout_panes.len() == 1 && layout_panes[0] == pane_id {
            self.layout = crate::layout::LayoutTree::empty();
        } else if layout_panes.iter().any(|id| id == pane_id) {
            self.layout.remove_pane(pane_id)?;
        }

        self.panes.write().remove(pane_id);
        for tab in self.tabs.values_mut() {
            tab.pane_ids.retain(|id| id != pane_id);
        }

        let focused_is_valid = self
            .focused_pane
            .as_ref()
            .is_some_and(|focused| self.panes.read().contains_key(focused));
        if !focused_is_valid {
            self.focused_pane = self
                .layout
                .pane_ids()
                .into_iter()
                .find(|id| !id.is_empty() && self.panes.read().contains_key(id));
        }
        self.focused_tab = self.focused_pane.as_ref().and_then(|focused| {
            self.tabs
                .iter()
                .find(|(_, tab)| tab.pane_ids.contains(focused))
                .map(|(id, _)| id.clone())
        });

        Ok(true)
    }

    /// 添加附加客户端 (§3.10 AttachRequest)
    ///
    /// §3.3 `window_id` 是该连接所代表的 GUI 窗口 (Plan 32)。窗口成员资格靠
    /// 这个字段与连接绑定, 断连时才知道该释放哪个窗口。
    pub fn add_attached_client(
        &mut self,
        client_id: String,
        mode: AttachMode,
        role: ClientRole,
        window_id: Option<String>,
    ) {
        let clients = self.attached_clients.clone();
        clients.write().push(AttachedClient {
            client_id,
            mode,
            window_id,
            role,
        });
    }

    /// 移除附加客户端 (§3.10 DetachRequest)
    ///
    /// 返回该客户端声明的 window_id (§3.3 Plan 32), 调用方据此决定是否释放窗口。
    pub fn remove_attached_client(&mut self, client_id: &str) -> Option<String> {
        let clients = self.attached_clients.clone();
        let mut clients_w = clients.write();
        let window_id = clients_w
            .iter()
            .find(|client| client.client_id == client_id)
            .and_then(|client| client.window_id.clone());
        clients_w.retain(|c| c.client_id != client_id);
        window_id
    }

    /// §3.4 注册 lifecycle 订阅者: attach 时把该连接的 outbound channel 加入会话级注册表。
    /// 同一 client_id 重复 attach (幂等 / steal) 时直接替换旧的 sender, 旧的 outbound
    /// channel 因无引用而关闭——旧连接若仍存活, 其读循环会在下次发信失败时退出。
    /// 因此统计 attached_client_count 时不会因为残留 sender 而偏高。
    pub fn add_lifecycle_subscriber(
        &self,
        client_id: String,
        outbound_tx: tokio::sync::mpsc::UnboundedSender<Envelope>,
    ) {
        self.lifecycle_subscribers.write().insert(client_id, outbound_tx);
    }

    /// §3.4 退订 lifecycle 通知: detach / 断连 / steal 清场时调用。
    /// 移除该 client_id 对应的 outbound sender; 通道关闭由连接层写循环检测到。
    pub fn remove_lifecycle_subscriber(&self, client_id: &str) {
        self.lifecycle_subscribers.write().remove(client_id);
    }

    /// §3.4 清空 lifecycle 订阅 (steal 抢占踢出所有旧客户端时调用)。
    /// 返回被踢出的 client_id 列表, 由调用方决定是否额外断开连接的 transport。
    pub fn clear_lifecycle_subscribers(&self) -> Vec<String> {
        let mut subs = self.lifecycle_subscribers.write();
        let kicked: Vec<String> = subs.keys().cloned().collect();
        subs.clear();
        kicked
    }

    /// §3.4 向所有 attached 连接 fan-out 一条 lifecycle 通知 (at-least-once)。
    ///
    /// 与 pane 维度 lossy 的 PaneDirty / PaneOutput 路径不同: lifecycle 通知
    /// (PaneAdded / PaneRemoved / SessionLayoutChanged) 必须保证送达每个
    /// attached 客户端, 不只是发起方——这正是 §3.4 multi-client semantics 的核心。
    ///
    /// outbound channel 是 tokio::mpsc::unbounded, 因此不会因 subscriber 慢而丢弃
    /// (会背压到整个连接的内存), closed channel 的 send 失败时立即清理对应订阅。
    pub fn broadcast_lifecycle(&self, notification: Notification) {
        let envelope = Envelope {
            version: Some(mux_protocol::PROTOCOL_VERSION.clone()),
            payload: Some(EnvelopePayload::Notification(notification)),
        };
        let mut subs = self.lifecycle_subscribers.write();
        subs.retain(|_client_id, tx| {
            if tx.send(envelope.clone()).is_ok() {
                true
            } else {
                // §3.4 连接断开或写循环退出: 清理该 subscription, 不再尝试投递。
                false
            }
        });
    }

    /// 附加客户端数量 (§3.10 SessionInfo.attached_clients)
    pub fn attached_client_count(&self) -> u32 {
        self.attached_clients.read().len() as u32
    }

    /// 检查 session 是否为空 (§3.7 idle behavior)
    pub fn is_empty(&self) -> bool {
        self.panes.read().is_empty()
    }

    /// §16.9 设置同步滚动偏移 (触发广播)
    pub fn set_sync_scrollback_offset(&self, pane_id: String, offset: u32) {
        let mut state = self.sync_scrollback.write();
        state.pane_id = Some(pane_id);
        state.scroll_offset = offset;
        state.enabled = true;
    }

    /// §16.9 获取当前同步滚动状态
    pub fn get_sync_scrollback(&self) -> SyncScrollbackState {
        self.sync_scrollback.read().clone()
    }

    /// §16.9 禁用同步滚动
    pub fn disable_sync_scrollback(&self) {
        let mut state = self.sync_scrollback.write();
        state.enabled = false;
        state.pane_id = None;
        state.scroll_offset = 0;
    }

    // ========================================================================
    // §3.3 窗口管理方法 (多窗口支持，Plan 32)
    // ========================================================================

    /// §3.3 添加窗口到会话的已连接窗口列表。
    /// 返回 true 表示这是一次真正的新增 (调用方据此决定是否广播 WindowAdded)。
    pub fn add_window(&self, window_id: String) -> bool {
        let mut windows = self.connected_windows.write();
        if windows.contains(&window_id) {
            return false;
        }
        windows.push(window_id);
        true
    }

    /// §3.3 从会话移除窗口
    pub fn remove_window(&self, window_id: &str) {
        let mut windows = self.connected_windows.write();
        windows.retain(|w| w != window_id);
    }

    /// §3.3 释放一个窗口引用 (Plan 32), 返回 true 表示窗口确实离开了会话。
    ///
    /// 窗口归属记录在 `AttachedClient.window_id` 上, 只有在最后一个引用它的
    /// attached 客户端离开之后才把窗口从 `connected_windows` 摘掉。§15.4 的
    /// 原地重连会先用同一个 window_id 建立新连接再清理旧连接, 这段交叠期间
    /// 引用式判断保证窗口不会被旧连接的清理误删。
    pub fn release_window(&self, window_id: &str) -> bool {
        let still_claimed = self
            .attached_clients
            .read()
            .iter()
            .any(|client| client.window_id.as_deref() == Some(window_id));
        if still_claimed {
            return false;
        }
        let mut windows = self.connected_windows.write();
        let before = windows.len();
        windows.retain(|window| window != window_id);
        windows.len() != before
    }

    /// §3.3 获取会话已连接的窗口 ID 列表
    pub fn get_windows(&self) -> Vec<String> {
        self.connected_windows.read().clone()
    }

    /// §3.3 获取已连接窗口数量
    pub fn window_count(&self) -> usize {
        self.connected_windows.read().len()
    }

    /// §3.3 检查窗口是否在会话中
    pub fn has_window(&self, window_id: &str) -> bool {
        self.connected_windows.read().contains(&window_id.to_string())
    }

    /// §3.3 / §3.4 广播布局变更到所有连接的窗口 (Plan 32)。
    ///
    /// 走的是 §3.4 at-least-once 的 lifecycle 路径, 因此每个 attached 连接
    /// (即每个窗口) 都会收到, 而不只是触发变更的那一个。返回收到通知的窗口
    /// ID 列表, 供调用方记录/断言。
    pub fn broadcast_layout_change(&self, layout: mux_protocol::LayoutTree) -> Vec<String> {
        self.broadcast_lifecycle(Notification {
            event: Some(mux_protocol::notification::Event::SessionLayoutChanged(
                mux_protocol::SessionLayoutChanged {
                    layout: Some(layout),
                },
            )),
        });
        self.get_windows()
    }
}
