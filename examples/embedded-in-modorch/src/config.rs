//! Sample host policy, deliberately outside the library and its configuration DTO.
use modorch_fedengine::FedEngineConfig;
use std::{io, path::Path};

/// Freeze relative filesystem paths against an explicit host-owned base.
/// This performs no filesystem I/O and leaves secret references unchanged.
pub fn prepare(mut config: FedEngineConfig, base: &Path) -> io::Result<FedEngineConfig> {
    if !base.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host path base must be absolute",
        ));
    }
    fn resolve(path: &mut String, base: &Path) -> io::Result<()> {
        if Path::new(path).is_relative() {
            *path = base
                .join(&*path)
                .into_os_string()
                .into_string()
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "host path must be UTF-8")
                })?;
        }
        Ok(())
    }
    if config.duckdb.database_path != ":memory:" {
        resolve(&mut config.duckdb.database_path, base)?;
    }
    resolve(&mut config.duckdb.temp_directory, base)?;
    resolve(&mut config.management.credentials_directory, base)?;
    for path in [
        &mut config.duckdb.extension_directory,
        &mut config.pgwire.tls_cert,
        &mut config.pgwire.tls_key,
    ]
    .into_iter()
    .flatten()
    {
        resolve(path, base)?;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_paths_are_explicit_without_io_or_secret_rewriting() {
        let original = FedEngineConfig::load("application.toml").unwrap();
        assert_eq!(original.duckdb.database_path, "./data/fedengine.duckdb");
        let mut config = original.clone();
        config.duckdb.extension_directory = Some("extensions".into());
        config.pgwire.tls_cert = Some("tls/server.pem".into());
        config.pgwire.tls_key = Some("tls/server.key".into());
        let base = std::env::temp_dir().join(format!("fedengine-host-base-{}", std::process::id()));
        assert!(!base.exists());
        let prepared = prepare(config, &base).unwrap();
        assert_eq!(
            Path::new(&prepared.duckdb.database_path),
            base.join("data/fedengine.duckdb")
        );
        assert_eq!(
            Path::new(&prepared.duckdb.temp_directory),
            base.join("data/tmp")
        );
        assert_eq!(
            Path::new(&prepared.management.credentials_directory),
            base.join("secrets")
        );
        assert_eq!(
            Path::new(prepared.duckdb.extension_directory.as_ref().unwrap()),
            base.join("extensions")
        );
        assert_eq!(
            Path::new(prepared.pgwire.tls_cert.as_ref().unwrap()),
            base.join("tls/server.pem")
        );
        assert_eq!(
            Path::new(prepared.pgwire.tls_key.as_ref().unwrap()),
            base.join("tls/server.key")
        );
        assert_eq!(
            prepared.users[0].password_secret_ref,
            "FEDENGINE_EMBEDDED_PASSWORD"
        );
        assert!(!base.exists());
        assert!(prepare(original, Path::new("relative")).is_err());
    }

    #[test]
    fn memory_absolute_paths_and_source_references_are_preserved() {
        let mut config = FedEngineConfig::load("application.toml").unwrap();
        config.duckdb.database_path = ":memory:".into();
        let base = std::env::temp_dir();
        let absolute = base.join("owned-state/tmp").to_str().unwrap().to_owned();
        config.duckdb.temp_directory = absolute.clone();
        // Use a source from the public DTO without requiring access to a secret.
        let mut value = serde_json::to_value(&config).unwrap();
        value["datasources"] = serde_json::json!([{
            "id":"source", "kind":"postgres", "catalog_alias":"source",
            "secret_ref":"file:source.dsn", "default_schema":"public",
            "allowed_schemas":["public"], "read_only":true
        }]);
        let prepared = prepare(serde_json::from_value(value).unwrap(), &base).unwrap();
        assert_eq!(prepared.duckdb.database_path, ":memory:");
        assert_eq!(prepared.duckdb.temp_directory, absolute);
        assert_eq!(prepared.datasources[0].secret_ref, "file:source.dsn");
    }
}
