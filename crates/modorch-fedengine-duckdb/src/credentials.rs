use modorch_fedengine_core::*;
use std::path::Path;

pub(super) fn resolve(reference: &str, directory: &str) -> Result<String> {
    if !valid_secret_ref(reference) {
        return Err(EngineError::config("invalid credential reference"));
    }
    let value = if let Some(relative) = reference.strip_prefix("file:") {
        let base = Path::new(directory)
            .canonicalize()
            .map_err(|_| EngineError::config("credential directory unavailable"))?;
        let path = base
            .join(relative)
            .canonicalize()
            .map_err(|_| EngineError::config("credential file unavailable"))?;
        if !path.starts_with(&base) || !path.is_file() {
            return Err(EngineError::config(
                "credential file must remain within the configured directory",
            ));
        }
        let metadata = std::fs::metadata(&path)
            .map_err(|_| EngineError::config("credential file unavailable"))?;
        if metadata.len() > 65_536 {
            return Err(EngineError::config("credential file exceeds size limit"));
        }
        std::fs::read_to_string(path)
            .map_err(|_| EngineError::config("credential file unavailable"))?
    } else {
        std::env::var(reference)
            .map_err(|_| EngineError::config("data source credential reference is missing"))?
    };
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(EngineError::config(
            "data source credential reference is empty",
        ));
    }
    Ok(value)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn file_references_are_reloaded_and_cannot_escape_through_symlinks() {
        let root =
            std::env::temp_dir().join(format!("fedengine-credentials-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let directory = root.join("secrets");
        std::fs::create_dir(&directory).unwrap();
        let reference = directory.join("connection");
        std::fs::write(&reference, "first\n").unwrap();
        assert_eq!(
            resolve("file:connection", directory.to_str().unwrap()).unwrap(),
            "first"
        );
        std::fs::write(&reference, "second").unwrap();
        assert_eq!(
            resolve("file:connection", directory.to_str().unwrap()).unwrap(),
            "second"
        );
        std::fs::write(root.join("outside"), "must-not-read").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), directory.join("escape")).unwrap();
        assert!(resolve("file:escape", directory.to_str().unwrap()).is_err());
        assert!(resolve("file:../outside", directory.to_str().unwrap()).is_err());
    }
}

/// Organized connections use credential JSON, never DSN rewriting or endpoint guessing.
pub(super) fn target_dsn(
    reference: &str,
    directory: &str,
    kind: DataSourceKind,
    instance: &DatabaseInstance,
    database: Option<&str>,
) -> Result<String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Credentials {
        username: String,
        password: String,
        #[serde(default)]
        options: std::collections::BTreeMap<String, String>,
    }
    instance.validate()?;
    let credentials: Credentials = serde_json::from_str(&resolve(reference, directory)?)
        .map_err(|_| EngineError::config("organized connection requires credential JSON with username, password and optional options"))?;
    if credentials.username.is_empty()
        || credentials.username.contains('\0')
        || credentials.password.contains('\0')
    {
        return Err(EngineError::config("invalid database credentials"));
    }
    let value = |s: &str| -> String {
        match kind {
            DataSourceKind::Postgres => {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
            }
            DataSourceKind::Mysql => {
                format!("\"{}\"", s.replace('\\', "\\\\").replace('\"', "\\\""))
            }
        }
    };
    let mut fields = vec![
        format!("host={}", value(&instance.host)),
        format!("port={}", instance.port),
        format!("user={}", value(&credentials.username)),
        format!("password={}", value(&credentials.password)),
    ];
    if let Some(database) = database {
        if database.is_empty() || database.contains('\0') {
            return Err(EngineError::config("invalid target database"));
        }
        fields.push(format!(
            "{}={}",
            if kind == DataSourceKind::Postgres {
                "dbname"
            } else {
                "database"
            },
            value(database)
        ));
    }
    for (key, option) in credentials.options {
        let allowed = match kind {
            DataSourceKind::Postgres => [
                "sslmode",
                "sslcert",
                "sslkey",
                "sslrootcert",
                "connect_timeout",
                "application_name",
            ]
            .contains(&key.as_str()),
            DataSourceKind::Mysql => [
                "ssl_mode",
                "ssl_ca",
                "ssl_cert",
                "ssl_key",
                "connect_timeout",
            ]
            .contains(&key.as_str()),
        };
        if !allowed || option.contains('\0') {
            return Err(EngineError::config(
                "unsupported database credential option",
            ));
        }
        fields.push(format!("{key}={}", value(&option)));
    }
    if !fields.iter().any(|s| s.starts_with("connect_timeout=")) {
        fields.push("connect_timeout=10".into());
    }
    Ok(fields.join(" "))
}

#[cfg(test)]
mod structured_tests {
    use super::*;
    #[test]
    fn target_credentials_escape_values_and_cannot_override_instance_or_database() {
        let root = std::env::temp_dir().join(format!("fedengine-target-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let file = root.join("credentials.json");
        std::fs::write(&file,r#"{"username":"u","password":"a'b\\c@? dbname=wrong","options":{"sslmode":"disable"}}"#).unwrap();
        let instance = DatabaseInstance {
            host: "db.example".into(),
            port: 5432,
            identity: None,
        };
        let dsn = target_dsn(
            "file:credentials.json",
            root.to_str().unwrap(),
            DataSourceKind::Postgres,
            &instance,
            Some("actual"),
        )
        .unwrap();
        assert!(dsn.contains("password='a\\'b\\\\c@? dbname=wrong'"));
        assert!(dsn.contains("dbname='actual'"));
        assert!(dsn.contains("host='db.example'"));
        std::fs::write(
            &file,
            r#"{"username":"u","password":"p","options":{"host":"other"}}"#,
        )
        .unwrap();
        assert!(
            target_dsn(
                "file:credentials.json",
                root.to_str().unwrap(),
                DataSourceKind::Postgres,
                &instance,
                Some("actual")
            )
            .is_err()
        );
        std::fs::write(&file, "host=old dbname=old user=old password=old").unwrap();
        assert!(
            target_dsn(
                "file:credentials.json",
                root.to_str().unwrap(),
                DataSourceKind::Postgres,
                &instance,
                Some("actual")
            )
            .is_err()
        );
    }
}
