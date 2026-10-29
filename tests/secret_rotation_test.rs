use stellar_k8s::backup::SecretRotationConfig;

#[tokio::test]
async fn test_secret_rotation_config_default() {
    let config = SecretRotationConfig::default();

    assert!(!config.enabled);
    assert_eq!(config.schedule, "0 0 1 * *");
    assert_eq!(config.password_length, 32);
    assert_eq!(config.db_timeout_seconds, 30);
    assert_eq!(config.max_retries, 3);
    assert!(!config.audit_logging_enabled);
    assert!(config.audit_log_destination.is_none());
    assert!(config.notification_webhook.is_none());
}

#[tokio::test]
async fn test_secret_rotation_config_round_trip() {
    let config = SecretRotationConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: SecretRotationConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(config, deserialized);
}
