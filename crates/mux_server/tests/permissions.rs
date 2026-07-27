//! # Permission Tests
//!
//! §3.3 客户端角色与权限控制测试 (Plan 33)

use mux_server::session::{ClientRole, AttachedClient, AttachMode};

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
    assert!(check_permission(ClientRole::ReadWrite, ClientRole::ReadOnly));
    assert!(check_permission(ClientRole::ReadWrite, ClientRole::ReadWrite));
    // ReadWrite 不能执行 Admin 操作
    assert!(!check_permission(ClientRole::ReadWrite, ClientRole::Admin));
}

/// §3.3 check_permission: ReadOnly 只能执行 ReadOnly 操作 (Plan 33)
#[test]
fn test_readonly_only_readonly() {
    use mux_server::connection::check_permission;
    assert!(check_permission(ClientRole::ReadOnly, ClientRole::ReadOnly));
    // ReadOnly 不能执行 ReadWrite 操作
    assert!(!check_permission(ClientRole::ReadOnly, ClientRole::ReadWrite));
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
    assert!(matches!(proto_role_to_client_role(2), ClientRole::ReadWrite));
    // 3 = ADMIN
    assert!(matches!(proto_role_to_client_role(3), ClientRole::Admin));
    // 0 或其他值 = 默认 ReadWrite
    assert!(matches!(proto_role_to_client_role(0), ClientRole::ReadWrite));
    assert!(matches!(proto_role_to_client_role(99), ClientRole::ReadWrite));
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
    assert!(!check_permission(ClientRole::ReadOnly, ClientRole::ReadWrite));
    assert!(!check_permission(ClientRole::ReadOnly, ClientRole::Admin));

    // ReadWrite
    assert!(check_permission(ClientRole::ReadWrite, ClientRole::ReadOnly));
    assert!(check_permission(ClientRole::ReadWrite, ClientRole::ReadWrite));
    assert!(!check_permission(ClientRole::ReadWrite, ClientRole::Admin));

    // Admin
    assert!(check_permission(ClientRole::Admin, ClientRole::ReadOnly));
    assert!(check_permission(ClientRole::Admin, ClientRole::ReadWrite));
    assert!(check_permission(ClientRole::Admin, ClientRole::Admin));
}
