//! Structured, privacy-preserving audit logging for room operations.

use serde::Serialize;
use sqlx::PgPool;
use tracing::warn;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AuditLogEntry {
    pub id: i64,
    pub room_id: String,
    pub actor_member_id: String,
    pub actor_role: String,
    pub action: String,
    pub details: Option<serde_json::Value>,
    pub client_ip: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Asynchronously record an audit log entry while stripping any sensitive tokens, passwords, or filesystem paths.
pub async fn record_audit_event(
    pool: &PgPool,
    room_id: &str,
    actor_member_id: &str,
    actor_role: &str,
    action: &str,
    details: Option<serde_json::Value>,
    client_ip: Option<&str>,
) {
    let sanitized_details = details.map(sanitize_value);

    let res = sqlx::query(
        "INSERT INTO room_audit_logs (room_id, actor_member_id, actor_role, action, details, client_ip, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, NOW())",
    )
    .bind(room_id)
    .bind(actor_member_id)
    .bind(actor_role)
    .bind(action)
    .bind(&sanitized_details)
    .bind(client_ip)
    .execute(pool)
    .await;

    if let Err(e) = res {
        warn!(room_id = %room_id, action = %action, error = %e, "Failed to persist audit log entry");
    }
}

/// Recursively sanitizes JSON values to remove credentials, tokens, and filesystem paths.
pub fn sanitize_value(val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => {
            let mut sanitized = serde_json::Map::new();
            for (k, v) in map {
                let lower_k = k.to_lowercase();
                if lower_k.contains("pass")
                    || lower_k.contains("token")
                    || lower_k.contains("grant")
                    || lower_k.contains("secret")
                    || lower_k.contains("key")
                {
                    sanitized.insert(k, serde_json::Value::String("[REDACTED]".to_string()));
                } else if lower_k.contains("path") || lower_k.contains("dir") || lower_k.contains("file") {
                    sanitized.insert(k, serde_json::Value::String("[REDACTED_PATH]".to_string()));
                } else {
                    sanitized.insert(k, sanitize_value(v));
                }
            }
            serde_json::Value::Object(sanitized)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sanitize_value).collect())
        }
        serde_json::Value::String(s) => {
            // If string looks like an absolute path or bearer token, redact it
            if s.contains(":\\") || s.starts_with("/home/") || s.starts_with("/opt/") || s.starts_with("/data/") || s.starts_with("/var/") {
                serde_json::Value::String("[REDACTED_PATH]".to_string())
            } else if s.starts_with("Bearer ") {
                serde_json::Value::String("[REDACTED_TOKEN]".to_string())
            } else {
                serde_json::Value::String(s)
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sanitization_removes_sensitive_data() {
        let input = json!({
            "revision": 1,
            "mod_count": 2,
            "password": "super_secret_password",
            "owner_token": "8e7f8484f15b353cb04647ef",
            "storage_path": "C:\\Users\\admin\\file.dat",
            "linux_path": "/opt/ltk/secret.json",
            "nested": {
                "member_token": "abc12345",
                "safe_field": "ok"
            }
        });

        let sanitized = sanitize_value(input);
        assert_eq!(sanitized["revision"], 1);
        assert_eq!(sanitized["mod_count"], 2);
        assert_eq!(sanitized["password"], "[REDACTED]");
        assert_eq!(sanitized["owner_token"], "[REDACTED]");
        assert_eq!(sanitized["storage_path"], "[REDACTED_PATH]");
        assert_eq!(sanitized["linux_path"], "[REDACTED_PATH]");
        assert_eq!(sanitized["nested"]["member_token"], "[REDACTED]");
        assert_eq!(sanitized["nested"]["safe_field"], "ok");
    }
}
