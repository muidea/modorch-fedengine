use crate::{Analyzer, ObjectRef};
use modorch_fedengine_contract::{GuardedMutationPlan, ReturningPlan};
use modorch_fedengine_core::{DataSourceDefinition, DataSourceKind, EngineError, Result};
use sqlparser::ast::*;
use std::ops::ControlFlow;

fn quote_dialect(name: &str, kind: DataSourceKind) -> String {
    if kind.is_mysql_family() {
        format!("`{}`", name.replace('`', "``"))
    } else if kind == DataSourceKind::Sqlserver {
        format!("[{}]", name.replace(']', "]]"))
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn format_table(schema: &str, table: &str, kind: DataSourceKind) -> String {
    format!(
        "{}.{}",
        quote_dialect(schema, kind),
        quote_dialect(table, kind)
    )
}

fn rewrite_relation(
    name: &mut ObjectName,
    kind: DataSourceKind,
    expected_catalog: &str,
) -> Result<()> {
    let parts = Analyzer::parts(name)?;
    let (schema, table) = match parts.as_slice() {
        [cat, schema, table] if cat == expected_catalog => (schema.as_str(), table.as_str()),
        [schema, table] => (schema.as_str(), table.as_str()),
        _ => {
            return Err(EngineError::unsupported(
                "guarded mutation requires single-source table reference",
            ));
        }
    };
    let q = if kind.is_mysql_family() {
        '`'
    } else if kind == DataSourceKind::Sqlserver {
        '['
    } else {
        '"'
    };
    *name = ObjectName(vec![
        ObjectNamePart::Identifier(Ident::with_quote(q, schema)),
        ObjectNamePart::Identifier(Ident::with_quote(q, table)),
    ]);
    Ok(())
}

/// Convert an analyzed, parameter-compacted single-source DML statement into a
/// dialect-local guarded mutation SQL statement for native scanner execution.
pub fn build_guarded_mutation_plan(
    compacted_sql: &str,
    source: &DataSourceDefinition,
    primary: &ObjectRef,
    returning: Option<&ReturningPlan>,
) -> Result<Option<GuardedMutationPlan>> {
    // Guarded CRUD native functions exist for postgres/greenplum/kingbase/mysql/mariadb/oracle/sqlserver/dameng/db2.
    if matches!(
        source.kind,
        DataSourceKind::Starrocks | DataSourceKind::Hana
    ) {
        return Ok(None);
    }
    if returning.is_some()
        && !matches!(
            source.kind,
            DataSourceKind::Postgres | DataSourceKind::Mysql | DataSourceKind::Mariadb
        )
    {
        return Err(EngineError::unsupported(
            "INSERT ... RETURNING is only enabled on guarded PostgreSQL and MySQL sources",
        ));
    }
    let mut parsed = Analyzer::parse(compacted_sql)?;
    if parsed.len() != 1 {
        return Ok(None);
    }
    let question_marks = !source.kind.is_postgres_family();
    let mut seen_placeholders = Vec::new();
    match &mut parsed[0] {
        Statement::Insert(i) => {
            if let TableObject::TableName(name) = &mut i.table {
                rewrite_relation(name, source.kind, &source.catalog_alias)?;
            }
            if matches!(source.kind, DataSourceKind::Mysql | DataSourceKind::Mariadb) {
                // mysql_guarded_insert_returning fetches returning keys via LAST_INSERT_ID()/PK query
                // and requires mysql_stmt_field_count == 0 on the prepared INSERT.
                i.returning = None;
            }
        }
        Statement::Update(u) => {
            if let TableFactor::Table { name, .. } = &mut u.table.relation {
                rewrite_relation(name, source.kind, &source.catalog_alias)?;
            }
        }
        Statement::Delete(d) => {
            let from = match &mut d.from {
                FromTable::WithFromKeyword(v) | FromTable::WithoutKeyword(v) => v,
            };
            for item in from {
                if let TableFactor::Table { name, .. } = &mut item.relation {
                    rewrite_relation(name, source.kind, &source.catalog_alias)?;
                }
            }
        }
        _ => return Ok(None),
    }

    let res = visit_expressions_mut(&mut parsed[0], |expr| {
        if question_marks
            && let Expr::Cast { expr: inner, .. } = expr
            && matches!(inner.as_ref(), Expr::Value(v) if matches!(v.value, Value::Placeholder(_)))
        {
            // Non-PG guarded engines bind typed parameters out-of-band via struct_pack/ODBC and
            // reject PostgreSQL `::TYPE` placeholder cast syntax.
            *expr = (**inner).clone();
        }
        if let Expr::Value(v) = expr
            && let Value::Placeholder(p) = &mut v.value
        {
            if let Some(rest) = p.strip_prefix('$')
                && let Ok(idx) = rest.parse::<usize>()
                && idx >= 1
            {
                seen_placeholders.push(idx - 1);
                if question_marks {
                    *p = "?".into();
                }
            } else if question_marks && p != "?" {
                return ControlFlow::Break(EngineError::unsupported(
                    "unsupported placeholder form in guarded mutation",
                ));
            }
        }
        ControlFlow::Continue(())
    });
    if let ControlFlow::Break(e) = res {
        return Err(e);
    }
    if question_marks && seen_placeholders.len() > 1 {
        for pair in seen_placeholders.windows(2) {
            if pair[0] >= pair[1] {
                return Err(EngineError::unsupported(
                    "repeated or out-of-order parameter reuse requires explicit expressions in '?' dialects",
                ));
            }
        }
    }
    let _ = format_table;
    let native_sql = if source.kind == DataSourceKind::Sqlserver {
        // sqlparser formats `['x']` with `quote_style '['` automatically as `[x]`.
        parsed[0].to_string()
    } else {
        parsed[0].to_string()
    };
    Ok(Some(GuardedMutationPlan {
        source: source.id.clone(),
        kind: source.kind,
        catalog: source.catalog_alias.clone(),
        schema: primary.schema.clone(),
        table: primary.name.clone(),
        native_sql,
        returning: returning.cloned(),
    }))
}
