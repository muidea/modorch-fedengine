use crate::{DataSourceKind, DuckdbConfig, EngineError, Result};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionDirectory {
    pub path: String,
    pub source: &'static str,
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: String,
    pub sha256: String,
    pub source_root: PathBuf,
    pub resolved_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginCapability {
    pub kind: &'static str,
    pub extension: &'static str,
    pub extension_installed: bool,
    pub extension_source: Option<&'static str>,
    pub odbc_driver: Option<&'static str>,
    pub odbc_driver_installed: Option<bool>,
    pub ready: bool,
    pub plugin_name: Option<&'static str>,
}

pub fn canonical_extension_key(logical: &str) -> &str {
    match logical {
        "postgres" | "greenplum" | "kingbasees" => "postgres",
        "mysql" | "mariadb" | "starrocks" => "mysql",
        "oracle" => "oracle",
        "sqlserver" => "sqlserver",
        "odbc_catalog" | "dameng" | "db2" | "hana" => "odbc_catalog",
        _ => logical,
    }
}

pub fn extension_module_name(logical: &str) -> String {
    format!("{}_scanner", canonical_extension_key(logical))
}

pub fn extension_filename(logical: &str) -> String {
    format!("{}.duckdb_extension", extension_module_name(logical))
}

pub fn required_odbc_driver(kind: DataSourceKind) -> Option<&'static str> {
    match kind {
        DataSourceKind::Sqlserver => Some("FreeTDS"),
        DataSourceKind::Oracle => Some("Oracle 23 ODBC driver"),
        DataSourceKind::Db2 => Some("IBM DB2 ODBC DRIVER"),
        DataSourceKind::Hana => Some("HDBODBC"),
        DataSourceKind::Dameng => Some("DM8 ODBC DRIVER"),
        _ => None,
    }
}

pub fn plugin_name_for_kind(kind: DataSourceKind) -> Option<&'static str> {
    match kind {
        DataSourceKind::Sqlserver => Some("sqlserver"),
        DataSourceKind::Oracle => Some("oracle"),
        DataSourceKind::Db2 => Some("db2"),
        DataSourceKind::Hana => Some("hana"),
        DataSourceKind::Dameng => Some("dameng"),
        _ => None,
    }
}

/// Returns extension directories in priority order: plugin directory first, then built-in directory.
pub fn extension_directories(config: &DuckdbConfig) -> Vec<ExtensionDirectory> {
    let mut dirs = Vec::new();
    let plugin_env = std::env::var("FEDENGINE_PLUGIN_EXTENSION_DIRECTORY")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if let Some(dir) = config
        .plugin_extension_directory
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or(plugin_env)
    {
        dirs.push(ExtensionDirectory {
            path: dir,
            source: "plugin",
            optional: true,
        });
    }

    let builtin_env = std::env::var("FEDENGINE_EXTENSION_DIRECTORY")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if let Some((dir, optional)) = config
        .extension_directory
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| (s.to_owned(), false))
        .or_else(|| builtin_env.map(|s| (s, true)))
        && !dirs.iter().any(|existing| existing.path == dir)
    {
        dirs.push(ExtensionDirectory {
            path: dir,
            source: "builtin",
            optional,
        });
    }
    dirs
}

pub fn resolve_extension_file(root: &Path, logical: &str, relative: &str) -> Result<PathBuf> {
    let path = std::fs::canonicalize(root.join(relative))
        .map_err(|_| EngineError::config("extension file unavailable"))?;
    let expected_name = extension_filename(canonical_extension_key(logical));
    if !path.starts_with(root)
        || path.file_name().and_then(|n| n.to_str()) != Some(expected_name.as_str())
    {
        return Err(EngineError::config("extension manifest path is invalid"));
    }
    Ok(path)
}

pub fn load_directory_entries(
    directory: &Path,
    optional: bool,
) -> Result<BTreeMap<String, (String, String)>> {
    let root = match std::fs::canonicalize(directory) {
        Ok(root) => root,
        Err(_) if optional => return Ok(BTreeMap::new()),
        Err(_) => return Err(EngineError::config("extension directory is unavailable")),
    };
    let mut manifest_files = Vec::new();
    let main_manifest = root.join("manifest.json");
    if let Ok(meta) = std::fs::symlink_metadata(&main_manifest) {
        if meta.file_type().is_symlink() {
            return Err(EngineError::config("extension manifest is invalid"));
        }
        if meta.is_file() {
            manifest_files.push(main_manifest);
        }
    }
    let manifest_dir = root.join("manifest.d");
    if let Ok(meta) = std::fs::symlink_metadata(&manifest_dir) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(EngineError::config(
                "extension manifest directory is invalid",
            ));
        }
        let mut fragments = Vec::new();
        for entry in std::fs::read_dir(&manifest_dir)
            .map_err(|_| EngineError::config("cannot read extension manifest directory"))?
        {
            let entry = entry
                .map_err(|_| EngineError::config("cannot read extension manifest directory"))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let entry_meta = std::fs::symlink_metadata(&path)
                .map_err(|_| EngineError::config("extension manifest is invalid"))?;
            if entry_meta.file_type().is_symlink() || !entry_meta.is_file() {
                return Err(EngineError::config("extension manifest is invalid"));
            }
            fragments.push(path);
        }
        fragments.sort();
        manifest_files.extend(fragments);
    }

    if manifest_files.is_empty() {
        if optional {
            return Ok(BTreeMap::new());
        }
        return Err(EngineError::config("extension manifest is missing"));
    }

    let mut merged: BTreeMap<String, (String, String)> = BTreeMap::new();
    for file_path in manifest_files {
        let bytes = std::fs::read(&file_path)
            .map_err(|_| EngineError::config("extension manifest is missing"))?;
        let manifest: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| EngineError::config("extension manifest is invalid"))?;
        if manifest["duckdb_version"] != "v2.0.0" {
            return Err(EngineError::config(
                "extension manifest core version mismatch",
            ));
        }
        let extensions = manifest["extensions"]
            .as_object()
            .ok_or_else(|| EngineError::config("extension manifest is invalid"))?;
        for (key, value) in extensions {
            let rel = value["path"]
                .as_str()
                .ok_or_else(|| EngineError::config("extension manifest entry invalid"))?;
            let sha256 = value["sha256"]
                .as_str()
                .ok_or_else(|| EngineError::config("extension manifest entry invalid"))?;
            if let Some((existing_rel, existing_sha)) = merged.get(key)
                && (existing_rel != rel || existing_sha != sha256)
            {
                return Err(EngineError::config(
                    "conflicting extension manifest entry in directory",
                ));
            }
            merged.insert(key.clone(), (rel.to_owned(), sha256.to_owned()));
        }
    }
    Ok(merged)
}

pub fn find_manifest_entry(
    config: &DuckdbConfig,
    logical: &str,
) -> Result<Option<(ManifestEntry, &'static str)>> {
    let canonical = canonical_extension_key(logical);
    for dir in extension_directories(config) {
        let root = match std::fs::canonicalize(&dir.path) {
            Ok(root) => root,
            Err(_) if dir.optional => continue,
            Err(_) => return Err(EngineError::config("extension directory is unavailable")),
        };
        let entries = load_directory_entries(&root, dir.optional)?;
        if let Some((rel, sha256)) = entries.get(canonical) {
            let resolved_path = resolve_extension_file(&root, canonical, rel)?;
            return Ok(Some((
                ManifestEntry {
                    path: rel.clone(),
                    sha256: sha256.clone(),
                    source_root: root,
                    resolved_path,
                },
                dir.source,
            )));
        }
    }
    Ok(None)
}

pub fn parse_installed_odbc_drivers(ini_paths: &[PathBuf]) -> BTreeMap<String, Option<String>> {
    let mut drivers = BTreeMap::new();
    for path in ini_paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut current: Option<String> = None;
        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let name = line[1..line.len() - 1].trim().to_owned();
                if !name.is_empty() {
                    drivers.entry(name.clone()).or_insert(None);
                    current = Some(name);
                } else {
                    current = None;
                }
                continue;
            }
            if let Some(section) = &current
                && let Some((key, value)) = line.split_once('=')
                && key.trim().eq_ignore_ascii_case("driver")
            {
                let driver_path = value.trim().to_owned();
                if !driver_path.is_empty() {
                    drivers.insert(section.clone(), Some(driver_path));
                }
            }
        }
    }
    drivers
}

fn default_odbc_ini_paths() -> Vec<PathBuf> {
    if let Ok(sys_ini) = std::env::var("ODBCSYSINI")
        && !sys_ini.trim().is_empty()
    {
        vec![PathBuf::from(sys_ini.trim()).join("odbcinst.ini")]
    } else {
        vec![PathBuf::from("/etc/odbcinst.ini")]
    }
}

pub fn inspect_plugins(config: &DuckdbConfig) -> Vec<PluginCapability> {
    let ini_paths = default_odbc_ini_paths();
    inspect_plugins_with_ini(config, &ini_paths)
}

pub fn inspect_plugins_with_ini(
    config: &DuckdbConfig,
    ini_paths: &[PathBuf],
) -> Vec<PluginCapability> {
    let installed_odbc = parse_installed_odbc_drivers(ini_paths);
    let kinds = [
        DataSourceKind::Postgres,
        DataSourceKind::Greenplum,
        DataSourceKind::Kingbasees,
        DataSourceKind::Mysql,
        DataSourceKind::Mariadb,
        DataSourceKind::Starrocks,
        DataSourceKind::Sqlserver,
        DataSourceKind::Oracle,
        DataSourceKind::Db2,
        DataSourceKind::Hana,
        DataSourceKind::Dameng,
    ];
    kinds
        .into_iter()
        .map(|kind| {
            let ext = kind.extension();
            let canonical = canonical_extension_key(ext);
            let found = find_manifest_entry(config, ext).ok().flatten();
            let extension_installed = found
                .as_ref()
                .is_some_and(|(entry, _)| entry.resolved_path.is_file());
            let extension_source = if extension_installed {
                found.map(|(_, src)| src)
            } else {
                None
            };
            let odbc_driver = required_odbc_driver(kind);
            let odbc_driver_installed =
                odbc_driver.map(|driver_name| match installed_odbc.get(driver_name) {
                    Some(Some(driver_path)) => {
                        let p = Path::new(driver_path);
                        !p.is_absolute() || p.is_file()
                    }
                    Some(None) => false,
                    None => false,
                });
            let ready = extension_installed && odbc_driver_installed.unwrap_or(true);
            PluginCapability {
                kind: kind.dialect_name(),
                extension: canonical,
                extension_installed,
                extension_source,
                odbc_driver,
                odbc_driver_installed,
                ready,
                plugin_name: plugin_name_for_kind(kind),
            }
        })
        .collect()
}
