//! Native target identity is independent of credentials and outward names.
use crate::*;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseInstance {
    pub host: String,
    pub port: u16,
    /// Explicitly associates proxy/alias endpoints. No DNS inference is performed.
    #[serde(default)]
    pub identity: Option<String>,
}
impl DatabaseInstance {
    pub fn validate(&self) -> Result<()> {
        if self.host.is_empty()
            || self.host.len() > 253
            || self.port == 0
            || self.host.chars().any(|c| {
                c.is_whitespace() || c.is_control() || matches!(c, '\'' | '"' | '/' | '\\' | '=')
            })
            || self
                .identity
                .as_ref()
                .is_some_and(|i| !identifier(i) || i.len() > 128)
        {
            return Err(EngineError::config(
                "invalid database instance endpoint or identity",
            ));
        }
        Ok(())
    }
    pub fn key(&self) -> String {
        match &self.identity {
            Some(id) => format!("instance:{id}"),
            None => format!(
                "endpoint:{}:{}",
                self.host.trim_end_matches('.').to_ascii_lowercase(),
                self.port
            ),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConnection {
    pub id: DataSourceId,
    pub kind: DataSourceKind,
    pub instance: DatabaseInstance,
    pub secret_ref: String,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub bootstrap_database: Option<String>,
}
impl SourceConnection {
    pub fn validate(&self) -> Result<()> {
        self.instance.validate()?;
        if !identifier(&self.id.0)
            || self.id.0.len() > 96
            || !valid_secret_ref(&self.secret_ref)
            || [&self.database, &self.bootstrap_database].iter().any(|v| {
                v.as_ref().is_some_and(|s| {
                    s.is_empty() || s.len() > 128 || s.chars().any(char::is_control)
                })
            })
        {
            return Err(EngineError::config("invalid source connection"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceOrganization {
    pub connection_id: DataSourceId,
    pub instance: DatabaseInstance,
    pub database: String,
    pub virtual_database: String,
    pub owner: String,
    #[serde(default)]
    pub connect: Vec<String>,
}
impl SourceOrganization {
    pub fn native_key(&self, kind: DataSourceKind) -> String {
        serde_json::to_string(&(kind.extension(), self.instance.key(), &self.database))
            .expect("string tuple")
    }
    pub fn validate(&self, kind: DataSourceKind) -> Result<()> {
        self.instance.validate()?;
        if !identifier(&self.connection_id.0)
            || self.connection_id.0.len() > 96
            || !allowed_namespace(&self.database)
            || self.database.len() > 128
            || system_database(kind, &self.database)
            || !identifier(&self.virtual_database)
            || self.virtual_database.len() > 63
            || !allowed_namespace(&self.virtual_database)
            || ["memory", "main", "temp", "system"]
                .contains(&self.virtual_database.to_ascii_lowercase().as_str())
            || !identifier(&self.owner)
            || self.connect.len() > 1024
            || self.connect.iter().any(|u| !identifier(u))
        {
            return Err(EngineError::config(
                "invalid native target, virtual database or owner",
            ));
        }
        Ok(())
    }
}
pub fn system_database(kind: DataSourceKind, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    match kind {
        DataSourceKind::Postgres => matches!(
            name.as_str(),
            "postgres" | "template0" | "template1" | "_fedengine"
        ),
        DataSourceKind::Mysql => matches!(
            name.as_str(),
            "mysql" | "information_schema" | "performance_schema" | "sys" | "_fedengine"
        ),
    }
}
/// A desired target reserves its outward identity before asynchronous activation.
pub fn validate_organization(
    state: &ManagementState,
    definition: &DataSourceDefinition,
) -> Result<()> {
    let Some(target) = &definition.organization else {
        for source in state.sources.values().filter(|s| !s.deleted) {
            if state
                .revision(&source.incarnation, source.desired_revision)?
                .definition
                .organization
                .as_ref()
                .is_some_and(|target| {
                    target
                        .virtual_database
                        .eq_ignore_ascii_case(&definition.catalog_alias)
                })
            {
                return Err(EngineError::new(
                    "40001",
                    "execution alias conflicts with an automatic database reservation",
                ));
            }
        }
        return Ok(());
    };
    target.validate(definition.kind)?;
    for source in state
        .sources
        .values()
        .filter(|s| !s.deleted && s.id != definition.id)
    {
        let other = &state
            .revision(&source.incarnation, source.desired_revision)?
            .definition;
        let Some(existing) = &other.organization else {
            if source
                .catalog_alias
                .eq_ignore_ascii_case(&target.virtual_database)
            {
                return Err(EngineError::new(
                    "40001",
                    "virtual database name conflicts with a source",
                ));
            }
            continue;
        };
        if target.connection_id == existing.connection_id
            && (definition.kind != other.kind || target.instance.key() != existing.instance.key())
        {
            return Err(EngineError::new(
                "40001",
                "connection registration id belongs to another instance",
            ));
        }
        let same_target = target.native_key(definition.kind) == existing.native_key(other.kind);
        let same_name = target
            .virtual_database
            .eq_ignore_ascii_case(&existing.virtual_database);
        if same_target && target.virtual_database != existing.virtual_database {
            return Err(EngineError::new(
                "40001",
                format!(
                    "native database already belongs to virtual database {}",
                    existing.virtual_database
                ),
            ));
        }
        if same_name
            && (target.virtual_database != existing.virtual_database
                || definition.kind != other.kind
                || target.instance.key() != existing.instance.key()
                || !same_target)
        {
            return Err(EngineError::new(
                "40001",
                "virtual database name belongs to another native target",
            ));
        }
        if same_name && (target.owner != existing.owner || target.connect != existing.connect) {
            return Err(EngineError::new(
                "40001",
                "merged database must retain its owner and CONNECT principals",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseDiscoveryStatus {
    Available,
    Forbidden,
    Unavailable,
    Excluded,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DatabaseCandidate {
    pub database: String,
    pub schemas: Vec<String>,
    pub status: DatabaseDiscoveryStatus,
    pub error: Option<String>,
    pub virtual_database: Option<String>,
    pub merged: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_identity_preserves_database_case_and_distinguishes_ports_without_dns() {
        let target = SourceOrganization {
            connection_id: DataSourceId("one".into()),
            instance: DatabaseInstance {
                host: "DB.Example.".into(),
                port: 5432,
                identity: None,
            },
            database: "Sales".into(),
            virtual_database: "business".into(),
            owner: "owner".into(),
            connect: vec![],
        };
        let mut alias = target.clone();
        alias.instance.host = "db.example".into();
        alias.connection_id = DataSourceId("other".into());
        alias.virtual_database = "different".into();
        assert_eq!(
            target.native_key(DataSourceKind::Postgres),
            alias.native_key(DataSourceKind::Postgres)
        );
        alias.instance.port = 5433;
        assert_ne!(
            target.native_key(DataSourceKind::Postgres),
            alias.native_key(DataSourceKind::Postgres)
        );
        alias = target.clone();
        alias.database = "sales".into();
        assert_ne!(
            target.native_key(DataSourceKind::Postgres),
            alias.native_key(DataSourceKind::Postgres)
        );
        alias = target.clone();
        alias.instance.identity = Some("cluster".into());
        let mut proxy = alias.clone();
        proxy.instance.host = "proxy".into();
        proxy.instance.port = 6432;
        assert_eq!(
            alias.native_key(DataSourceKind::Postgres),
            proxy.native_key(DataSourceKind::Postgres)
        );
    }
}
