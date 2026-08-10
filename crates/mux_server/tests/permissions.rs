//! # Permission Tests
//!
//! §3.3 客户端角色与权限控制测试 (Plan 33)

use mux_protocol::request::Body as RequestBody;
use mux_server::connection::{ConnectionTrust, pre_attach_role};
use mux_server::session::{AttachMode, AttachedClient, ClientRole};

// ============================================================
// §3.3 ClientRole 枚举测试
// ============================================================

/// §3.3 ClientRole 默认值为 ReadWrite
#[test]
fn test_client_role_default() {
    let role = ClientRole::default();
    assert!(matches!(role, ClientRole::ReadWrite));
}

/// §3.3 ClientRole 可 Clone/Copy
#[test]
fn test_client_role_clone_copy() {
    let role1 = ClientRole::Admin;
    let role2 = role1; // Copy
    let role3 = role1.clone(); // Clone
    assert_eq!(role1, role2);
    assert_eq!(role1, role3);
}

// ============================================================
// §3.3 AttachedClient 角色字段测试
// ============================================================

/// §3.3 AttachedClient 包含 role 字段 (Plan 33)
#[test]
fn test_attached_client_has_role() {
    let client = AttachedClient {
        client_id: "test-client".to_string(),
        mode: AttachMode::Shared,
        window_id: Some("win-1".to_string()),
        role: ClientRole::ReadOnly,
    };
    assert_eq!(client.client_id, "test-client");
    assert!(matches!(client.role, ClientRole::ReadOnly));
    assert_eq!(client.window_id.as_deref(), Some("win-1"));
}

/// §3.3 AttachedClient 可 Clone
#[test]
fn test_attached_client_clone() {
    let client1 = AttachedClient {
        client_id: "c1".to_string(),
        mode: AttachMode::Shared,
        window_id: None,
        role: ClientRole::Admin,
    };
    let client2 = client1.clone();
    assert_eq!(client1.client_id, client2.client_id);
    assert_eq!(client1.role, client2.role);
}

// ============================================================
// §3.3 权限检查逻辑测试
// ============================================================

/// §3.3 check_permission: Admin 可执行所有操作 (Plan 33)
#[test]
fn test_admin_allows_all() {
    use mux_server::connection::check_permission;
    assert!(check_permission(ClientRole::Admin, ClientRole::ReadOnly));
    assert!(check_permission(ClientRole::Admin, ClientRole::ReadWrite));
    assert!(check_permission(ClientRole::Admin, ClientRole::Admin));
}

/// §3.3 check_permission: ReadWrite 可执行 ReadWrite 和 ReadOnly 操作 (Plan 33)
#[test]
fn test_readwrite_allows_readwrite() {
    use mux_server::connection::check_permission;
    assert!(check_permission(
        ClientRole::ReadWrite,
        ClientRole::ReadOnly
    ));
    assert!(check_permission(
        ClientRole::ReadWrite,
        ClientRole::ReadWrite
    ));
    // ReadWrite 不能执行 Admin 操作
    assert!(!check_permission(ClientRole::ReadWrite, ClientRole::Admin));
}

/// §3.3 check_permission: ReadOnly 只能执行 ReadOnly 操作 (Plan 33)
#[test]
fn test_readonly_only_readonly() {
    use mux_server::connection::check_permission;
    assert!(check_permission(ClientRole::ReadOnly, ClientRole::ReadOnly));
    // ReadOnly 不能执行 ReadWrite 操作
    assert!(!check_permission(
        ClientRole::ReadOnly,
        ClientRole::ReadWrite
    ));
    // ReadOnly 不能执行 Admin 操作
    assert!(!check_permission(ClientRole::ReadOnly, ClientRole::Admin));
}

// ============================================================
// §3.3 proto_role_to_client_role 映射测试
// ============================================================

/// §3.3 proto 角色值映射到内部角色 (Plan 33)
#[test]
fn test_proto_role_mapping() {
    use mux_server::connection::proto_role_to_client_role;
    // 1 = READ_ONLY
    assert!(matches!(proto_role_to_client_role(1), ClientRole::ReadOnly));
    // 2 = READ_WRITE
    assert!(matches!(
        proto_role_to_client_role(2),
        ClientRole::ReadWrite
    ));
    // 3 = ADMIN
    assert!(matches!(proto_role_to_client_role(3), ClientRole::Admin));
}

/// §3.3 CLIENT_ROLE_UNSPECIFIED 与未知枚举值必须 fail-closed。
/// 这个整数由对端提供,当成 ReadWrite 等于让客户端自选权限。
#[test]
fn test_proto_role_mapping_is_fail_closed() {
    use mux_server::connection::proto_role_to_client_role;
    assert_eq!(proto_role_to_client_role(0), ClientRole::ReadOnly);
    assert_eq!(proto_role_to_client_role(99), ClientRole::ReadOnly);
    assert_eq!(proto_role_to_client_role(-1), ClientRole::ReadOnly);
}

#[test]
fn test_readonly_attach_mode_downgrades_effective_role() {
    use mux_server::connection::effective_attach_role;

    assert_eq!(
        effective_attach_role(ClientRole::Admin, AttachMode::ReadOnly),
        ClientRole::ReadOnly
    );
    assert_eq!(
        effective_attach_role(ClientRole::ReadWrite, AttachMode::ReadOnly),
        ClientRole::ReadOnly
    );
    assert_eq!(
        effective_attach_role(ClientRole::Admin, AttachMode::Shared),
        ClientRole::Admin
    );
}

// ============================================================
// §3.3 权限矩阵测试
// ============================================================

/// §3.3 完整的权限矩阵: 3 roles × 3 required levels = 9 组合 (Plan 33)
#[test]
fn test_permission_matrix() {
    use mux_server::connection::check_permission;

    // ReadOnly
    assert!(check_permission(ClientRole::ReadOnly, ClientRole::ReadOnly));
    assert!(!check_permission(
        ClientRole::ReadOnly,
        ClientRole::ReadWrite
    ));
    assert!(!check_permission(ClientRole::ReadOnly, ClientRole::Admin));

    // ReadWrite
    assert!(check_permission(
        ClientRole::ReadWrite,
        ClientRole::ReadOnly
    ));
    assert!(check_permission(
        ClientRole::ReadWrite,
        ClientRole::ReadWrite
    ));
    assert!(!check_permission(ClientRole::ReadWrite, ClientRole::Admin));

    // Admin
    assert!(check_permission(ClientRole::Admin, ClientRole::ReadOnly));
    assert!(check_permission(ClientRole::Admin, ClientRole::ReadWrite));
    assert!(check_permission(ClientRole::Admin, ClientRole::Admin));
}

// ============================================================
// §3.3 未 attach 连接的角色 (transport 信任 + 显式白名单)
// ============================================================

fn kill_session() -> RequestBody {
    RequestBody::KillSession(mux_protocol::KillSessionRequest {
        id: "session-1".to_string(),
    })
}

fn shutdown() -> RequestBody {
    RequestBody::Shutdown(mux_protocol::ShutdownRequest {})
}

fn list_recovery_candidates() -> RequestBody {
    RequestBody::ListRecoveryCandidates(mux_protocol::ListRecoveryCandidatesRequest {})
}

fn confirm_recovery() -> RequestBody {
    RequestBody::ConfirmRecovery(mux_protocol::ConfirmRecoveryRequest {
        session_id: "session-1".to_string(),
    })
}

fn send_input() -> RequestBody {
    RequestBody::SendInput(mux_protocol::SendInputRequest {
        pane_id: "pane-1".to_string(),
        data: b"ls\n".to_vec(),
    })
}

fn spawn_pane() -> RequestBody {
    RequestBody::SpawnPane(mux_protocol::SpawnPaneRequest {
        session_id: "session-1".to_string(),
        tab_id: "tab-0".to_string(),
        size: None,
        command: None,
        cwd: None,
    })
}

fn read_file() -> RequestBody {
    RequestBody::ReadFile(mux_protocol::ReadFileRequest {
        path: "/etc/passwd".to_string(),
        offset_line: None,
        max_lines: None,
        offset_bytes: None,
        max_bytes: None,
    })
}

fn list_sessions() -> RequestBody {
    RequestBody::ListSessions(mux_protocol::ListSessionsRequest {})
}

fn install_extension() -> RequestBody {
    RequestBody::InstallExtension(mux_protocol::InstallExtensionRequest {
        name: "demo".to_string(),
        manifest: Vec::new(),
        source: Vec::new(),
    })
}

/// §3.3 attach 时未声明 identity 的默认角色由 transport 决定。
/// 本地 socket 靠 §9 的 0600 ACL 保证同 UID;网络 transport 没有这个保证。
#[test]
fn test_attach_default_role_follows_transport_trust() {
    assert_eq!(
        ConnectionTrust::LocalSocket.attach_default_role(),
        ClientRole::Admin
    );
    assert_eq!(
        ConnectionTrust::Unauthenticated.attach_default_role(),
        ClientRole::ReadOnly
    );
}

/// §3.3 没有对端认证的 transport 上,未 attach 的连接一条写操作都拿不到。
/// 这正是旧代码 `unwrap_or(ClientRole::Admin)` 的提权风险。
#[test]
fn test_unauthenticated_transport_never_grants_more_than_read_only() {
    for body in [
        kill_session(),
        shutdown(),
        list_recovery_candidates(),
        confirm_recovery(),
        send_input(),
        spawn_pane(),
        read_file(),
        list_sessions(),
        install_extension(),
    ] {
        assert_eq!(
            pre_attach_role(ConnectionTrust::Unauthenticated, &body),
            ClientRole::ReadOnly,
            "unauthenticated transport must stay fail-closed for {body:?}"
        );
    }
}

/// §3.3 本地 socket 上的一次性 CLI 命令 (`z3rm kill` / `kill-server` /
/// `send-keys`) 从不 attach,由显式白名单放行;白名单之外仍然是 ReadOnly。
#[test]
fn test_local_socket_pre_attach_whitelist() {
    let trust = ConnectionTrust::LocalSocket;

    assert_eq!(pre_attach_role(trust, &kill_session()), ClientRole::Admin);
    assert_eq!(pre_attach_role(trust, &shutdown()), ClientRole::Admin);
    assert_eq!(
        pre_attach_role(trust, &list_recovery_candidates()),
        ClientRole::Admin
    );
    assert_eq!(
        pre_attach_role(trust, &confirm_recovery()),
        ClientRole::Admin
    );

    assert_eq!(pre_attach_role(trust, &send_input()), ClientRole::ReadWrite);
    assert_eq!(pre_attach_role(trust, &spawn_pane()), ClientRole::ReadWrite);

    // 白名单之外的请求默认 fail-closed。
    assert_eq!(
        pre_attach_role(trust, &list_sessions()),
        ClientRole::ReadOnly
    );
    assert_eq!(
        pre_attach_role(trust, &install_extension()),
        ClientRole::ReadOnly
    );
}

/// §16.6 文件 RPC 永远不会因为"未 attach"而被提权:沙箱根来自 attach 的
/// session cwd,所以未 attach 的连接读不到任何文件。
#[test]
fn test_file_requests_are_never_privileged_pre_attach() {
    for trust in [
        ConnectionTrust::LocalSocket,
        ConnectionTrust::Unauthenticated,
    ] {
        assert_eq!(pre_attach_role(trust, &read_file()), ClientRole::ReadOnly);
        assert_eq!(
            pre_attach_role(
                trust,
                &RequestBody::ListDir(mux_protocol::ListDirRequest {
                    path: "/".to_string(),
                })
            ),
            ClientRole::ReadOnly
        );
        assert_eq!(
            pre_attach_role(
                trust,
                &RequestBody::StatFile(mux_protocol::StatFileRequest {
                    path: "/etc/shadow".to_string(),
                })
            ),
            ClientRole::ReadOnly
        );
    }
}

/// §3.3 白名单授予的角色必须真的够用:否则 CLI 短命连接会被自己的
/// 权限检查挡住。
#[test]
fn test_pre_attach_roles_satisfy_their_handlers() {
    use mux_server::connection::check_permission;
    let trust = ConnectionTrust::LocalSocket;

    assert!(check_permission(
        pre_attach_role(trust, &kill_session()),
        ClientRole::Admin
    ));
    assert!(check_permission(
        pre_attach_role(trust, &shutdown()),
        ClientRole::Admin
    ));
    assert!(check_permission(
        pre_attach_role(trust, &list_recovery_candidates()),
        ClientRole::Admin
    ));
    assert!(check_permission(
        pre_attach_role(trust, &confirm_recovery()),
        ClientRole::Admin
    ));
    assert!(check_permission(
        pre_attach_role(trust, &send_input()),
        ClientRole::ReadWrite
    ));
    assert!(check_permission(
        pre_attach_role(trust, &spawn_pane()),
        ClientRole::ReadWrite
    ));
    assert!(check_permission(
        pre_attach_role(trust, &read_file()),
        ClientRole::ReadOnly
    ));
}
