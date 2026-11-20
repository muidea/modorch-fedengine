use modorch_fedengine_core::*;
use sha2::{Digest, Sha256};
use std::io::Read;

pub(super) fn canonical_extension_key(logical: &str) -> &str {
    match logical {
        "postgres" | "greenplum" | "kingbasees" => "postgres",
        "mysql" | "mariadb" | "starrocks" => "mysql",
        "oracle" => "oracle",
        "sqlserver" => "sqlserver",
        "odbc_catalog" | "dameng" | "db2" | "hana" => "odbc_catalog",
        _ => logical,
    }
}

pub(super) fn extension_module_name(logical: &str) -> String {
    format!("{}_scanner", canonical_extension_key(logical))
}

pub(super) fn extension_filename(logical: &str) -> String {
    format!("{}.duckdb_extension", extension_module_name(logical))
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

pub(super) fn provisioned(directory: &str, logical: &str) -> Result<bool> {
    let canonical = canonical_extension_key(logical);
    Ok(read_manifest(std::path::Path::new(directory))?["extensions"][canonical].is_object())
}

fn read_manifest(root: &std::path::Path) -> Result<serde_json::Value> {
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("manifest.json"))
            .map_err(|_| EngineError::config("extension manifest is missing"))?,
    )
    .map_err(|_| EngineError::config("extension manifest is invalid"))?;
    if manifest["duckdb_version"] != "v2.0.0" {
        return Err(EngineError::config(
            "extension manifest core version mismatch",
        ));
    }
    Ok(manifest)
}

pub(super) fn verify_extension(directory: &str, logical: &str) -> Result<()> {
    let canonical = canonical_extension_key(logical);
    let root = std::fs::canonicalize(directory)
        .map_err(|_| EngineError::config("extension directory is unavailable"))?;
    let manifest = read_manifest(&root)?;
    let relative = manifest["extensions"][canonical]["path"]
        .as_str()
        .ok_or_else(|| EngineError::config("extension manifest entry missing"))?;
    let path = std::fs::canonicalize(root.join(relative))
        .map_err(|_| EngineError::config("extension file unavailable"))?;
    let expected_name = extension_filename(canonical);
    if !path.starts_with(&root)
        || path.file_name().and_then(|n| n.to_str()) != Some(expected_name.as_str())
    {
        return Err(EngineError::config("extension manifest path is invalid"));
    }
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
    if hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        != manifest["extensions"][canonical]["sha256"]
            .as_str()
            .unwrap_or("")
    {
        return Err(EngineError::config("extension SHA256 mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
