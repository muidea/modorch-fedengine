use crate::{Analyzer, ObjectRef};
use modorch_fedengine_contract::{GuardedMutationPlan, PgType, ReturningPlan};
use modorch_fedengine_core::{DataSourceDefinition, DataSourceKind, EngineError, Result};
use sqlparser::ast::helpers::attached_token::AttachedToken;
use sqlparser::ast::*;
use std::ops::ControlFlow;

pub(crate) fn quote_dialect(name: &str, kind: DataSourceKind) -> String {
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

pub(crate) fn rewrite_relation(
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

fn oracle_temporal_wrapper(fn_name: &str, format: &str) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(fn_name))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                    Value::Placeholder("?".into()).into(),
                ))),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                    Value::SingleQuotedString(format.into()).into(),
                ))),
            ],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

/// Convert an analyzed, parameter-compacted single-source DML statement into a
/// dialect-local guarded mutation SQL statement for native scanner execution.
pub fn build_guarded_mutation_plan(
    compacted_sql: &str,
    source: &DataSourceDefinition,
    primary: &ObjectRef,
    returning: Option<&ReturningPlan>,
    param_types: &[PgType],
) -> Result<Option<GuardedMutationPlan>> {
    if source.kind == DataSourceKind::Starrocks {
        return Err(EngineError::unsupported(
            "StarRocks data sources are read-only; writes are not enabled",
        ));
    }
    let mut parsed = Analyzer::parse(compacted_sql)?;
    if parsed.len() != 1 {
        return Ok(None);
    }
    let question_marks = !source.kind.is_postgres_family();
    let mut seen_placeholders = Vec::new();
    let pg_family = source.kind.is_postgres_family();
    if let Some(ret) = returning {
        let supported = match &parsed[0] {
            Statement::Insert(_) => true,
            Statement::Update(_) => matches!(
                source.kind,
                DataSourceKind::Postgres
                    | DataSourceKind::Greenplum
                    | DataSourceKind::Kingbasees
                    | DataSourceKind::Oracle
                    | DataSourceKind::Sqlserver
                    | DataSourceKind::Dameng
                    | DataSourceKind::Db2
            ),
            Statement::Delete(_) => matches!(
                source.kind,
                DataSourceKind::Postgres
                    | DataSourceKind::Greenplum
                    | DataSourceKind::Kingbasees
                    | DataSourceKind::Mariadb
                    | DataSourceKind::Oracle
                    | DataSourceKind::Sqlserver
                    | DataSourceKind::Dameng
                    | DataSourceKind::Db2
            ),
            _ => false,
        };
        if !supported {
            return Err(EngineError::unsupported(
                "RETURNING is not enabled for this statement and data source dialect",
            ));
        }
        let _ = ret;
    }
    let is_delete = matches!(&parsed[0], Statement::Delete(_));
    let mut sqlserver_output_col: Option<(&'static str, &str)> = None;
    match &mut parsed[0] {
        Statement::Insert(i) => {
            if let TableObject::TableName(name) = &mut i.table {
                rewrite_relation(name, source.kind, &source.catalog_alias)?;
            }
            if !pg_family {
                if let Some(ret) = returning {
                    if matches!(source.kind, DataSourceKind::Oracle | DataSourceKind::Dameng)
                        && let Some(q) = &i.source
                        && let SetExpr::Values(v) = q.body.as_ref()
                        && v.rows.len() > 1
                    {
                        return Err(EngineError::unsupported(
                            "Oracle/Dameng RETURNING INTO only supports single-row inserts",
                        ));
                    }
                    if source.kind == DataSourceKind::Sqlserver {
                        i.output = Some(OutputClause::Output {
                            output_token: AttachedToken::empty(),
                            select_items: vec![SelectItem::UnnamedExpr(Expr::CompoundIdentifier(
                                vec![Ident::new("INSERTED"), Ident::with_quote('[', &ret.column)],
                            ))],
                            into_table: None,
                        });
                        sqlserver_output_col = Some(("INSERTED", &ret.column));
                    }
                }
                // Non-PG dialects either execute native SQL Server OUTPUT / DB2 FINAL TABLE,
                // Oracle/Dameng RETURNING INTO ?, or MySQL/HANA separate key retrieval.
                i.returning = None;
            }
        }
        Statement::Update(u) => {
            if let TableFactor::Table { name, .. } = &mut u.table.relation {
                rewrite_relation(name, source.kind, &source.catalog_alias)?;
            }
            if !pg_family {
                if source.kind == DataSourceKind::Sqlserver
                    && let Some(ret) = returning
                {
                    u.output = Some(OutputClause::Output {
                        output_token: AttachedToken::empty(),
                        select_items: vec![SelectItem::UnnamedExpr(Expr::CompoundIdentifier(
                            vec![Ident::new("INSERTED"), Ident::with_quote('[', &ret.column)],
                        ))],
                        into_table: None,
                    });
                    sqlserver_output_col = Some(("INSERTED", &ret.column));
                }
                u.returning = None;
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
            if !pg_family && source.kind != DataSourceKind::Mariadb {
                if source.kind == DataSourceKind::Sqlserver
                    && let Some(ret) = returning
                {
                    d.output = Some(OutputClause::Output {
                        output_token: AttachedToken::empty(),
                        select_items: vec![SelectItem::UnnamedExpr(Expr::CompoundIdentifier(
                            vec![Ident::new("DELETED"), Ident::with_quote('[', &ret.column)],
                        ))],
                        into_table: None,
                    });
                    sqlserver_output_col = Some(("DELETED", &ret.column));
                }
                d.returning = None;
            }
        }
        _ => return Ok(None),
    }

    let oracle = source.kind == DataSourceKind::Oracle;
    let res = visit_expressions_mut(&mut parsed[0], |expr| {
        if question_marks
            && let Expr::Cast { expr: inner, .. } = expr
            && matches!(inner.as_ref(), Expr::Value(v) if matches!(v.value, Value::Placeholder(_)))
        {
            // Non-PG guarded engines bind typed parameters out-of-band via struct_pack/ODBC and
            // reject PostgreSQL `::TYPE` placeholder cast syntax.
            *expr = (**inner).clone();
        }
        let mut replacement: Option<Expr> = None;
        if let Expr::Value(v) = expr
            && let Value::Placeholder(p) = &mut v.value
        {
            if let Some(rest) = p.strip_prefix('$')
                && let Ok(idx) = rest.parse::<usize>()
                && idx >= 1
            {
                let zero_idx = idx - 1;
                if zero_idx >= param_types.len() {
                    return ControlFlow::Break(EngineError::new(
                        "42P18",
                        "parameter placeholder index exceeds bound parameters",
                    ));
                }
                seen_placeholders.push(zero_idx);
                if question_marks {
                    *p = "?".into();
                }
                if oracle && let Some(ty) = param_types.get(zero_idx) {
                    match ty {
                        PgType::Date => {
                            replacement = Some(oracle_temporal_wrapper("TO_DATE", "YYYY-MM-DD"));
                        }
                        PgType::Timestamp => {
                            replacement = Some(oracle_temporal_wrapper(
                                "TO_TIMESTAMP",
                                "YYYY-MM-DD HH24:MI:SS.FF6",
                            ));
                        }
                        PgType::Timestamptz => {
                            replacement = Some(oracle_temporal_wrapper(
                                "TO_TIMESTAMP_TZ",
                                "YYYY-MM-DD HH24:MI:SS.FF6TZH:TZM",
                            ));
                        }
                        _ => {}
                    }
                }
            } else if question_marks && p != "?" {
                return ControlFlow::Break(EngineError::unsupported(
                    "unsupported placeholder form in guarded mutation",
                ));
            }
        }
        if let Some(new_expr) = replacement {
            *expr = new_expr;
        }
        ControlFlow::Continue(())
    });
    if let ControlFlow::Break(e) = res {
        return Err(e);
    }
    let _ = sqlserver_output_col;
    let _ = format_table;
    let inner_sql = if oracle
        && let Statement::Insert(i) = &parsed[0]
        && let Some(query) = &i.source
        && let SetExpr::Values(values) = query.body.as_ref()
        && values.rows.len() > 1
    {
        let table_sql = i.table.to_string();
        let cols_sql = if i.columns.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                i.columns
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let mut all = String::from("INSERT ALL");
        for row in &values.rows {
            let vals = row
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            all.push_str(&format!(" INTO {table_sql}{cols_sql} VALUES ({vals})"));
        }
        all.push_str(" SELECT 1 FROM DUAL");
        all
    } else {
        parsed[0].to_string()
    };
    let native_sql = if let Some(ret) = returning {
        match source.kind {
            DataSourceKind::Oracle | DataSourceKind::Dameng => format!(
                "{inner_sql} RETURNING {} INTO ?",
                quote_dialect(&ret.column, source.kind)
            ),
            DataSourceKind::Db2 => format!(
                "SELECT {} FROM {} TABLE ({inner_sql})",
                quote_dialect(&ret.column, source.kind),
                if is_delete { "OLD" } else { "FINAL" }
            ),
            _ => inner_sql,
        }
    } else {
        inner_sql
    };
    let param_map = if question_marks {
        seen_placeholders
    } else {
        Vec::new()
    };
    Ok(Some(GuardedMutationPlan {
        source: source.id.clone(),
        kind: source.kind,
        catalog: source.catalog_alias.clone(),
        schema: primary.schema.clone(),
        table: primary.name.clone(),
        native_sql,
        returning: returning.cloned(),
        param_map,
    }))
}
