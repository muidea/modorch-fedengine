use modorch_fedengine_core::{
    plugins::{
        canonical_extension_key as core_canonical_extension_key, extension_directories,
        extension_module_name as core_extension_module_name, find_manifest_entry,
    },
    *,
};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub(super) fn canonical_extension_key(logical: &str) -> &str {
    core_canonical_extension_key(logical)
}

pub(super) fn extension_module_name(logical: &str) -> String {
    core_extension_module_name(logical)
}

#[cfg(test)]
pub(super) fn extension_filename(logical: &str) -> String {
    modorch_fedengine_core::plugins::extension_filename(logical)
}

pub(super) fn clear_cache_sql(logical: &str, catalog_alias: &str) -> String {
    match canonical_extension_key(logical) {
        "postgres" => format!("CALL pg_clear_cache({})", quote_literal(catalog_alias)),
        "mysql" => format!("CALL mysql_clear_cache({})", quote_literal(catalog_alias)),
        "oracle" => "CALL oracle_clear_cache()".to_owned(),
        "sqlserver" => "CALL sqlserver_clear_cache()".to_owned(),
        "odbc_catalog" => "CALL odbc_catalog_clear_cache()".to_owned(),
        other => format!("CALL {other}_clear_cache()"),
    }
}

#[cfg(test)]
pub(super) fn provisioned(directory: &str, logical: &str) -> Result<bool> {
    let canonical = canonical_extension_key(logical);
    let entries =
        modorch_fedengine_core::plugins::load_directory_entries(Path::new(directory), false)?;
    Ok(entries.contains_key(canonical))
}

pub(super) fn provisioned_in_config(config: &DuckdbConfig, logical: &str) -> Result<bool> {
    Ok(find_manifest_entry(config, logical)?.is_some())
}

fn verify_file_sha256(path: &Path, expected_sha256: &str) -> Result<()> {
    let mut file =
        std::fs::File::open(path).map_err(|_| EngineError::config("cannot read extension"))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| EngineError::config("cannot read extension"))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let digest = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if digest != expected_sha256 {
        return Err(EngineError::config("extension SHA256 mismatch"));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn verify_extension(directory: &str, logical: &str) -> Result<()> {
    let canonical = canonical_extension_key(logical);
    let root = std::fs::canonicalize(directory)
        .map_err(|_| EngineError::config("extension directory is unavailable"))?;
    let entries = modorch_fedengine_core::plugins::load_directory_entries(&root, false)?;
    let (relative, expected_sha256) = entries
        .get(canonical)
        .ok_or_else(|| EngineError::config("extension manifest entry missing"))?;
    let path = modorch_fedengine_core::plugins::resolve_extension_file(&root, canonical, relative)?;
    let expected_name = modorch_fedengine_core::plugins::extension_filename(canonical);
    if path.file_name().and_then(|n| n.to_str()) != Some(expected_name.as_str()) {
        return Err(EngineError::config("extension manifest path is invalid"));
    }
    verify_file_sha256(&path, expected_sha256)
}

pub(super) fn verify_extension_path(config: &DuckdbConfig, logical: &str) -> Result<PathBuf> {
    let canonical = canonical_extension_key(logical);
    if extension_directories(config).is_empty() {
        return Err(EngineError::config("extension directory is unavailable"));
    }
    let Some((entry, _source)) = find_manifest_entry(config, logical)? else {
        return Err(EngineError::config(format!(
            "database extension '{canonical}' is not installed (enable the matching database plugin)"
        )));
    };
    verify_file_sha256(&entry.resolved_path, &entry.sha256)?;
    Ok(entry.resolved_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modorch_fedengine_core::plugins::{PluginCapability, inspect_plugins_with_ini};

    fn test_duckdb_config() -> DuckdbConfig {
        Config::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/application.toml"))
            .unwrap()
            .duckdb
    }

    #[test]
    fn provisioned_files_are_verified_across_buffer_boundaries() {
        let root =
            std::env::temp_dir().join(format!("fedengine-extension-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("postgres_scanner.duckdb_extension");
        let data = vec![42_u8; 2 * 64 * 1024 + 1];
        std::fs::write(&path, &data).unwrap();
        let digest: String = Sha256::digest(&data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let odbc_path = root.join("odbc_catalog_scanner.duckdb_extension");
        let odbc_data = vec![7_u8; 1024];
        std::fs::write(&odbc_path, &odbc_data).unwrap();
        let odbc_digest: String = Sha256::digest(&odbc_data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "duckdb_version":"v2.0.0", "extensions":{
                    "postgres":{
                        "path":"postgres_scanner.duckdb_extension", "sha256":digest
                    },
                    "odbc_catalog":{
                        "path":"odbc_catalog_scanner.duckdb_extension", "sha256":odbc_digest
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let directory = root.to_str().unwrap();
        assert_eq!(
            extension_filename("postgres"),
            "postgres_scanner.duckdb_extension"
        );
        assert!(provisioned(directory, "postgres").unwrap());
        assert!(provisioned(directory, "greenplum").unwrap());
        assert!(provisioned(directory, "dameng").unwrap());
        assert!(provisioned(directory, "db2").unwrap());
        assert!(!provisioned(directory, "mysql").unwrap());
        verify_extension(directory, "postgres").unwrap();
        verify_extension(directory, "greenplum").unwrap();
        verify_extension(directory, "dameng").unwrap();
        assert_eq!(extension_module_name("hana"), "odbc_catalog_scanner");
        assert_eq!(
            clear_cache_sql("postgres", "pg_crm"),
            "CALL pg_clear_cache('pg_crm')"
        );
        assert_eq!(
            clear_cache_sql("mysql", "my_erp"),
            "CALL mysql_clear_cache('my_erp')"
        );
        assert_eq!(
            clear_cache_sql("dameng", "dm_src"),
            "CALL odbc_catalog_clear_cache()"
        );
        std::fs::write(&path, &data[..data.len() - 1]).unwrap();
        assert!(verify_extension(directory, "postgres").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multi_directory_manifest_fragments_and_plugin_inspection() {
        let root = std::env::temp_dir().join(format!("fedengine-plugins-{}", uuid::Uuid::new_v4()));
        let builtin = root.join("builtin");
        let plugins = root.join("plugins/extensions");
        let manifest_d = plugins.join("manifest.d");
        std::fs::create_dir_all(&builtin).unwrap();
        std::fs::create_dir_all(&manifest_d).unwrap();

        let write_ext = |base: &Path, file: &str, bytes: &[u8]| -> String {
            let p = base.join(file);
            std::fs::write(&p, bytes).unwrap();
            Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };

        let pg_sha = write_ext(&builtin, "postgres_scanner.duckdb_extension", b"pg-ext");
        let my_sha = write_ext(&builtin, "mysql_scanner.duckdb_extension", b"my-ext");
        std::fs::write(
            builtin.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "duckdb_version": "v2.0.0",
                "extensions": {
                    "postgres": {"path": "postgres_scanner.duckdb_extension", "sha256": pg_sha},
                    "mysql": {"path": "mysql_scanner.duckdb_extension", "sha256": my_sha}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let ora_sha = write_ext(&plugins, "oracle_scanner.duckdb_extension", b"ora-ext");
        std::fs::write(
            manifest_d.join("oracle.json"),
            serde_json::to_vec(&serde_json::json!({
                "duckdb_version": "v2.0.0",
                "extensions": {
                    "oracle": {"path": "oracle_scanner.duckdb_extension", "sha256": ora_sha}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let driver_so = root.join("libsqora.so.23.1");
        std::fs::write(&driver_so, b"fake-driver").unwrap();
        let odbc_ini = root.join("odbcinst.ini");
        std::fs::write(
            &odbc_ini,
            format!(
                "[Oracle 23 ODBC driver]\nDriver={}\n\n[HDBODBC]\nDriver=/nonexistent/libodbcHDB.so\n",
                driver_so.display()
            ),
        )
        .unwrap();

        let mut cfg = test_duckdb_config();
        cfg.extension_directory = Some(builtin.to_string_lossy().into());
        cfg.plugin_extension_directory =
            Some(root.join("missing-optional").to_string_lossy().into());
        // Missing optional plugin dir returns built-ins without error.
        assert!(provisioned_in_config(&cfg, "postgres").unwrap());
        assert!(!provisioned_in_config(&cfg, "oracle").unwrap());
        assert!(
            verify_extension_path(&cfg, "oracle")
                .unwrap_err()
                .to_string()
                .contains("is not installed")
        );

        cfg.plugin_extension_directory = Some(plugins.to_string_lossy().into());
        assert_eq!(
            verify_extension_path(&cfg, "oracle").unwrap(),
            plugins
                .canonicalize()
                .unwrap()
                .join("oracle_scanner.duckdb_extension")
        );
        assert_eq!(
            verify_extension_path(&cfg, "postgres").unwrap(),
            builtin
                .canonicalize()
                .unwrap()
                .join("postgres_scanner.duckdb_extension")
        );

        let report: Vec<PluginCapability> = inspect_plugins_with_ini(&cfg, &[odbc_ini]);
        let pg = report.iter().find(|p| p.kind == "postgres").unwrap();
        assert!(pg.ready && pg.extension_source == Some("builtin"));
        let ora = report.iter().find(|p| p.kind == "oracle").unwrap();
        assert!(ora.ready && ora.extension_source == Some("plugin"));
        assert_eq!(ora.odbc_driver_installed, Some(true));
        let hana = report.iter().find(|p| p.kind == "hana").unwrap();
        assert!(!hana.ready);
        assert_eq!(hana.odbc_driver_installed, Some(false));

        // Conflicting digest inside manifest.d is rejected.
        std::fs::write(
            manifest_d.join("conflict.json"),
            serde_json::to_vec(&serde_json::json!({
                "duckdb_version": "v2.0.0",
                "extensions": {
                    "oracle": {"path": "oracle_scanner.duckdb_extension", "sha256": "0".repeat(64)}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(find_manifest_entry(&cfg, "oracle").is_err());

        std::fs::remove_dir_all(root).unwrap();
    }
}
