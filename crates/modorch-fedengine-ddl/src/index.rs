use crate::{Analyzer, IndexIntent};
use modorch_fedengine_core::*;
use sqlparser::ast::{ObjectName, ObjectType, Statement};
impl IndexIntent {
    /// Generate only the limited remote DDL template; never concatenate original client SQL.
    pub fn duckdb_adapter_sql(&self, kind: DataSourceKind) -> Result<String> {
        if !kind.is_postgres_family() && !kind.is_mysql_family() {
            return Err(EngineError::unsupported(
                "index DDL is not enabled for this data source dialect",
            ));
        }
        let q = |s: &str| {
            if kind.is_mysql_family() {
                format!("`{}`", s.replace('`', "``"))
            } else {
                quote_identifier(s)
            }
        };
        let table = format!("{}.{}", q(&self.table.schema), q(&self.table.name));
        let remote = if self.create {
            format!(
                "CREATE {}INDEX {}{} ON {} ({})",
                if self.unique { "UNIQUE " } else { "" },
                if self.if_exists { "IF NOT EXISTS " } else { "" },
                q(&self.name),
                table,
                self.columns
                    .iter()
                    .map(|c| q(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else if kind.is_mysql_family() {
            format!("DROP INDEX {} ON {}", q(&self.name), table)
        } else {
            format!("DROP INDEX {}.{}", q(&self.table.schema), q(&self.name))
        };
        Ok(format!(
            "CALL {}_execute({}, {})",
            kind.extension(),
            quote_literal(&self.table.catalog),
            quote_literal(&remote)
        ))
    }
}

pub fn drop_index_name(sql: &str) -> Result<Option<ObjectName>> {
    Ok(index_ownership_target(sql)?.map(|(name, _)| name))
}

pub fn index_ownership_target(sql: &str) -> Result<Option<(ObjectName, &'static str)>> {
    let parsed = match Analyzer::parse(sql) {
        Ok(p) => p,
        Err(err) => {
            if let Some(name) = crate::analysis::parse_reindex_index(sql)? {
                return Ok(Some((name, "REINDEX")));
            }
            return Err(err);
        }
    };
    Ok(match parsed.first() {
        Some(Statement::Drop {
            object_type: ObjectType::Index,
            names,
            ..
        }) if names.len() == 1 => Some((names[0].clone(), "DROP_INDEX")),
        Some(Statement::AlterIndex { name, .. }) => Some((name.clone(), "ALTER_INDEX")),
        _ => None,
    })
}
