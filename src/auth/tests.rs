use super::*;

#[test]
fn configuration_is_strict_and_does_not_disclose_password_hashes() {
    let hash = hash_password("test-password").unwrap();
    let account = format!("admin:{hash}");
    let config = AuthConfig::new(&[&account]).unwrap();
    assert!(config.has_users());
    assert!(!format!("{config:?}").contains(&hash));
    assert!(!format!("{config:?}").contains("test-password"));
    assert!(AuthConfig::new(&[&account, &account]).is_err());
    for username in ["Admin", " admin", "a", "bad@name", "用户"] {
        assert!(AuthConfig::new(&[&format!("{username}:{hash}")]).is_err());
    }
    for account in ["admin:plaintext-secret", "admin:", "missing-separator"] {
        assert!(AuthConfig::new(&[account]).is_err());
    }
    assert!(
        AuthConfig::new(&vec![
            &*account;
            sarmg_admin_core::STATIC_ADMINISTRATORS_MAX + 1
        ])
        .is_err()
    );
}

#[tokio::test]
async fn independent_static_services_drop_sessions_and_disallow_configuration_mutation() {
    use sarmg_admin_core::{AdministratorStore, LoginContext};
    let hash = hash_password("test-password").unwrap();
    let config = AuthConfig::new(&[&format!("admin:{hash}")]).unwrap();
    let first = config.administrator_service().unwrap();
    let second = config.administrator_service().unwrap();
    let context = LoginContext {
        source: "127.0.0.1".into(),
        request_id: None,
        now_micros: 1,
    };
    let login = first
        .login("admin", "test-password", &context)
        .await
        .unwrap();
    assert_eq!(login.administrator.user_id.as_str(), "admin");
    assert!(
        first
            .authenticate_session(&login.session_token, 2)
            .await
            .is_ok()
    );
    assert!(
        second
            .authenticate_session(&login.session_token, 2)
            .await
            .is_err()
    );
    assert!(!first.store().supports_management());
}
