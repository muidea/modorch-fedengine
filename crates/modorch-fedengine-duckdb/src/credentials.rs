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
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizedCredentials {
    username: String,
    password: String,
    #[serde(default)]
    options: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StandaloneCredentials {
    host: String,
    port: u16,
    database: String,
    username: String,
    password: String,
    #[serde(default)]
    options: std::collections::BTreeMap<String, String>,
}

pub(super) struct AttachPlan {
    pub(super) secret_sql: Option<String>,
    pub(super) attach_sql: String,
}

fn validate_secret_field(v: &str, name: &str) -> Result<()> {
    if v.is_empty() || v.contains('\0') {
        return Err(EngineError::config(format!("invalid {name}")));
    }
    Ok(())
}

fn secret_identifier(alias: &str) -> String {
    let mut name = String::from("__fed_sec_");
    for ch in alias.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_lowercase());
        } else {
            name.push('_');
        }
    }
    quote_identifier(&name)
}

fn default_driver(
    kind: DataSourceKind,
    options: &std::collections::BTreeMap<String, String>,
) -> String {
    if let Some(driver) = options
        .get("driver")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return driver.to_owned();
    }
    let (env_key, fallback) = match kind {
        DataSourceKind::Oracle => ("OracleODBCDriver", "Oracle 23 ODBC driver"),
        DataSourceKind::Sqlserver => ("SqlServerODBCDriver", "FreeTDS"),
        DataSourceKind::Hana => ("HanaODBCDriver", "HDBODBC"),
        DataSourceKind::Dameng => ("DamengODBCDriver", "DM8 ODBC DRIVER"),
        DataSourceKind::Db2 => ("DB2ODBCDriver", "IBM DB2 ODBC DRIVER"),
        _ => return String::new(),
    };
    std::env::var(env_key)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn format_attach_options(kind: DataSourceKind, read_only: bool, extra: &[String]) -> String {
    let mut opts = vec![format!("TYPE {}", kind.attach_type())];
    if read_only || kind.is_read_only_scanner() {
        opts.push("READ_ONLY".into());
    }
    match kind {
        DataSourceKind::Greenplum => {
            opts.extend([
                "ISOLATION_LEVEL 'READ COMMITTED'".into(),
                "TRANSACTION_READ_ONLY false".into(),
                "USE_ENUM_SORT_ORDER false".into(),
                "USE_TEXT_PROTOCOL true".into(),
                "USE_CURSOR false".into(),
            ]);
        }
        DataSourceKind::Kingbasees => {
            opts.push("USE_TEXT_PROTOCOL true".into());
        }
        _ => {}
    }
    opts.extend(extra.iter().cloned());
    opts.join(", ")
}

pub(super) fn attach_plan(
    reference: &str,
    directory: &str,
    kind: DataSourceKind,
    alias: &str,
    read_only: bool,
    default_schema: &str,
    organization: Option<&SourceOrganization>,
) -> Result<AttachPlan> {
    if !kind.is_read_only_scanner() {
        let dsn = if let Some(target) = organization {
            target_dsn(
                reference,
                directory,
                kind,
                &target.instance,
                Some(&target.database),
            )?
        } else {
            resolve(reference, directory)?
        };
        let mut extra = Vec::new();
        if kind.is_postgres_family() && !default_schema.is_empty() {
            extra.push(format!("SCHEMA {}", quote_literal(default_schema)));
        }
        return Ok(AttachPlan {
            secret_sql: None,
            attach_sql: format!(
                "ATTACH {} AS {} ({})",
                quote_literal(&dsn),
                quote_identifier(alias),
                format_attach_options(kind, read_only, &extra)
            ),
        });
    }
    let raw = resolve(reference, directory)?;
    let (host, port, database, username, password, options) = if let Some(target) = organization {
        target.instance.validate()?;
        let creds: OrganizedCredentials = serde_json::from_str(&raw).map_err(|_| {
            EngineError::config(
                "organized connection requires credential JSON with username, password and optional options",
            )
        })?;
        (
            target.instance.host.clone(),
            target.instance.port,
            target.database.clone(),
            creds.username,
            creds.password,
            creds.options,
        )
    } else {
        let creds: StandaloneCredentials = serde_json::from_str(&raw).map_err(|_| {
            EngineError::config(
                "ODBC/Oracle connection requires credential JSON with host, port, database, username, password and optional options",
            )
        })?;
        if creds.port == 0 {
            return Err(EngineError::config("invalid target port"));
        }
        (
            creds.host,
            creds.port,
            creds.database,
            creds.username,
            creds.password,
            creds.options,
        )
    };
    validate_secret_field(&host, "host")?;
    validate_secret_field(&database, "database")?;
    validate_secret_field(&username, "username")?;
    if password.contains('\0') {
        return Err(EngineError::config("invalid database credentials"));
    }
    for (key, value) in &options {
        if key != "driver" || value.contains('\0') {
            return Err(EngineError::config(
                "unsupported database credential option",
            ));
        }
    }
    let driver = default_driver(kind, &options);
    let sec = secret_identifier(alias);
    let mut extra = vec![format!("SECRET {sec}")];
    let secret_sql = match kind {
        DataSourceKind::Oracle => {
            let clean_host = host.trim();
            let endpoint = if clean_host.contains(':') && !clean_host.starts_with('[') {
                format!("[{clean_host}]:{port}")
            } else {
                format!("{clean_host}:{port}")
            };
            let dbq = format!("//{endpoint}/{}", database.trim());
            let schema = if !default_schema.trim().is_empty() {
                default_schema.trim()
            } else {
                username.trim()
            };
            extra.push(format!("SCHEMA {}", quote_literal(schema)));
            format!(
                "CREATE OR REPLACE TEMPORARY SECRET {sec} (TYPE oracle, DRIVER {}, DBQ {}, UID {}, PWD {})",
                quote_literal(&driver),
                quote_literal(&dbq),
                quote_literal(&username),
                quote_literal(&password),
            )
        }
        DataSourceKind::Sqlserver => {
            let server = host
                .split('\\')
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(host.trim());
            let schema = if !default_schema.trim().is_empty()
                && !default_schema.eq_ignore_ascii_case(database.trim())
            {
                default_schema.trim()
            } else {
                "dbo"
            };
            extra.push(format!("SCHEMA {}", quote_literal(schema)));
            format!(
                "CREATE OR REPLACE TEMPORARY SECRET {sec} (TYPE sqlserver, DRIVER {}, SERVER {}, PORT {}, DATABASE {}, UID {}, PWD {})",
                quote_literal(&driver),
                quote_literal(server),
                quote_literal(&port.to_string()),
                quote_literal(&database),
                quote_literal(&username),
                quote_literal(&password),
            )
        }
        DataSourceKind::Dameng | DataSourceKind::Db2 | DataSourceKind::Hana => {
            let dialect = kind.dialect_name();
            extra.push(format!("DIALECT {}", quote_literal(dialect)));
            if !default_schema.trim().is_empty() {
                extra.push(format!("SCHEMA {}", quote_literal(default_schema.trim())));
            }
            format!(
                "CREATE OR REPLACE TEMPORARY SECRET {sec} (TYPE odbc_catalog, DIALECT {}, DRIVER {}, HOST {}, PORT {port}, DATABASE {}, USER {}, PASSWORD {})",
                quote_literal(dialect),
                quote_literal(&driver),
                quote_literal(&host),
                quote_literal(&database),
                quote_literal(&username),
                quote_literal(&password),
            )
        }
        _ => unreachable!("handled above"),
    };
    Ok(AttachPlan {
        secret_sql: Some(secret_sql),
        attach_sql: format!(
            "ATTACH '' AS {} ({})",
            quote_identifier(alias),
            format_attach_options(kind, read_only, &extra)
        ),
    })
}

pub(super) fn target_dsn(
    reference: &str,
    directory: &str,
    kind: DataSourceKind,
    instance: &DatabaseInstance,
    database: Option<&str>,
) -> Result<String> {
    instance.validate()?;
    let credentials: OrganizedCredentials = serde_json::from_str(&resolve(reference, directory)?)
        .map_err(|_| EngineError::config("organized connection requires credential JSON with username, password and optional options"))?;
    if credentials.username.is_empty()
        || credentials.username.contains('\0')
        || credentials.password.contains('\0')
    {
        return Err(EngineError::config("invalid database credentials"));
    }
    let value = |s: &str| -> String {
        if kind.is_postgres_family() {
            format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
        } else {
            format!("\"{}\"", s.replace('\\', "\\\\").replace('\"', "\\\""))
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
            if kind.is_postgres_family() {
                "dbname"
            } else {
                "database"
            },
            value(database)
        ));
    }
    for (key, option) in credentials.options {
        let allowed = if kind.is_postgres_family() {
            [
                "sslmode",
                "sslcert",
                "sslkey",
                "sslrootcert",
                "connect_timeout",
                "application_name",
            ]
            .contains(&key.as_str())
        } else if kind.is_mysql_family() {
            [
                "ssl_mode",
                "ssl_ca",
                "ssl_cert",
                "ssl_key",
                "connect_timeout",
            ]
            .contains(&key.as_str())
        } else {
            false
        };
        if !allowed || option.contains('\0') {
            return Err(EngineError::config(
                "unsupported database credential option",
            ));
        }
        // duckdb-mysql rejects `connect_timeout` in its ATTACH DSN parser; keep accepting it
        // in credential JSON options for organized connections, and omit it from MySQL DSNs.
        if kind.is_mysql_family() && key == "connect_timeout" {
            continue;
        }
        fields.push(format!("{key}={}", value(&option)));
    }
    if kind.is_postgres_family() && !fields.iter().any(|s| s.starts_with("connect_timeout=")) {
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
        assert!(dsn.contains("connect_timeout=10"));

        std::fs::write(
            &file,
            r#"{"username":"u","password":"p","options":{"connect_timeout":"10","ssl_mode":"disabled"}}"#,
        )
        .unwrap();
        let mysql_dsn = target_dsn(
            "file:credentials.json",
            root.to_str().unwrap(),
            DataSourceKind::Mysql,
            &instance,
            Some("LWLTEST"),
        )
        .unwrap();
        assert!(!mysql_dsn.contains("connect_timeout"), "{mysql_dsn}");
        assert!(mysql_dsn.contains("ssl_mode=\"disabled\""), "{mysql_dsn}");
        assert!(mysql_dsn.contains("database=\"LWLTEST\""), "{mysql_dsn}");

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

    #[test]
    fn attach_plan_builds_secrets_and_options_for_all_dialects() {
        let root = std::env::temp_dir().join(format!("fedengine-attach-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let file = root.join("creds.json");
        std::fs::write(&file, r#"{"username":"u","password":"p"}"#).unwrap();

        let org = SourceOrganization {
            connection_id: DataSourceId("c1".into()),
            instance: DatabaseInstance {
                host: "db.local".into(),
                port: 1521,
                identity: None,
            },
            database: "ORCL".into(),
            virtual_database: "vdb".into(),
            owner: "owner".into(),
            connect: vec![],
        };

        let gp = attach_plan(
            "file:creds.json",
            root.to_str().unwrap(),
            DataSourceKind::Greenplum,
            "gp_alias",
            true,
            "public",
            Some(&org),
        )
        .unwrap();
        assert!(gp.secret_sql.is_none());
        assert!(gp.attach_sql.contains("ISOLATION_LEVEL 'READ COMMITTED'"));
        assert!(gp.attach_sql.contains("USE_TEXT_PROTOCOL true"));

        let kb = attach_plan(
            "file:creds.json",
            root.to_str().unwrap(),
            DataSourceKind::Kingbasees,
            "kb_alias",
            true,
            "public",
            Some(&org),
        )
        .unwrap();
        assert!(kb.attach_sql.contains("USE_TEXT_PROTOCOL true"));

        let ora = attach_plan(
            "file:creds.json",
            root.to_str().unwrap(),
            DataSourceKind::Oracle,
            "ora_alias",
            true,
            "APP",
            Some(&org),
        )
        .unwrap();
        assert!(ora.secret_sql.as_deref().unwrap().contains("TYPE oracle"));
        assert!(
            ora.secret_sql
                .as_deref()
                .unwrap()
                .contains("//db.local:1521/ORCL")
        );
        assert!(ora.attach_sql.contains("TYPE oracle_scanner"));

        let sqlsrv = attach_plan(
            "file:creds.json",
            root.to_str().unwrap(),
            DataSourceKind::Sqlserver,
            "ss_alias",
            true,
            "dbo",
            Some(&org),
        )
        .unwrap();
        assert!(
            sqlsrv
                .secret_sql
                .as_deref()
                .unwrap()
                .contains("TYPE sqlserver")
        );
        assert!(sqlsrv.attach_sql.contains("TYPE sqlserver_scanner"));

        for kind in [
            DataSourceKind::Dameng,
            DataSourceKind::Db2,
            DataSourceKind::Hana,
        ] {
            let plan = attach_plan(
                "file:creds.json",
                root.to_str().unwrap(),
                kind,
                "odbc_alias",
                true,
                "APP",
                Some(&org),
            )
            .unwrap();
            let secret = plan.secret_sql.unwrap();
            assert!(secret.contains("TYPE odbc_catalog"), "{secret}");
            assert!(secret.contains(kind.dialect_name()), "{secret}");
            assert!(
                plan.attach_sql.contains("TYPE odbc_catalog"),
                "{}",
                plan.attach_sql
            );
            assert!(
                plan.attach_sql.contains(kind.dialect_name()),
                "{}",
                plan.attach_sql
            );
        }
    }
}
