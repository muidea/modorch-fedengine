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

fn oracle_custom_type(raw: &str) -> DataType {
    DataType::Custom(
        ObjectName(vec![ObjectNamePart::Identifier(Ident::new(raw))]),
        vec![],
    )
}

fn pg_catalog_date_type() -> DataType {
    DataType::Custom(
        ObjectName(vec![
            ObjectNamePart::Identifier(Ident::new("pg_catalog")),
            ObjectNamePart::Identifier(Ident::new("date")),
        ]),
        vec![],
    )
}

fn is_unqualified_date(dt: &DataType) -> bool {
    match dt {
        DataType::Date => true,
        DataType::Custom(name, mods) if mods.is_empty() => {
            Analyzer::parts(name).is_ok_and(|p| p.len() == 1 && p[0].eq_ignore_ascii_case("date"))
        }
        _ => false,
    }
}

fn oracle_scalar_cast_target(dt: &DataType) -> Option<DataType> {
    match dt {
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => {
            Some(oracle_custom_type("NUMBER(10,0)"))
        }
        DataType::BigInt(_) | DataType::Int8(_) => Some(oracle_custom_type("NUMBER(19,0)")),
        DataType::Real | DataType::Float4 | DataType::Float(ExactNumberInfo::None) => {
            Some(oracle_custom_type("BINARY_FLOAT"))
        }
        DataType::Double(_) | DataType::DoublePrecision | DataType::Float8 => {
            Some(oracle_custom_type("BINARY_DOUBLE"))
        }
        DataType::Custom(name, mods) if mods.is_empty() => {
            let parts = Analyzer::parts(name).ok()?;
            let base = match parts.as_slice() {
                [t] => t.as_str(),
                [schema, t] if schema.eq_ignore_ascii_case("pg_catalog") => t.as_str(),
                _ => return None,
            };
            match base.to_ascii_lowercase().as_str() {
                "int" | "integer" | "int4" => Some(oracle_custom_type("NUMBER(10,0)")),
                "bigint" | "int8" => Some(oracle_custom_type("NUMBER(19,0)")),
                "real" | "float4" => Some(oracle_custom_type("BINARY_FLOAT")),
                "double" | "float8" => Some(oracle_custom_type("BINARY_DOUBLE")),
                _ => None,
            }
        }
        _ => None,
    }
}

fn cast_placeholder(placeholder: &str, data_type: DataType) -> Expr {
    Expr::Cast {
        kind: CastKind::Cast,
        expr: Box::new(Expr::Value(Value::Placeholder(placeholder.into()).into())),
        data_type,
        array: false,
        format: None,
    }
}

fn mysql_time_placeholder() -> Expr {
    Expr::Cast {
        kind: CastKind::Cast,
        expr: Box::new(cast_placeholder("?", DataType::Char(None))),
        data_type: DataType::Time(Some(6), TimezoneInfo::None),
        array: false,
        format: None,
    }
}

/// Validate SQL text literals in a KingbaseES DML statement against `SQL_ASCII` and
/// `empty_str_is_null` / Oracle-mode constraints.
pub fn validate_kingbase_text_literals(
    native_sql: &str,
    ascii_only_text: bool,
    reject_empty_text: bool,
) -> Result<()> {
    if !ascii_only_text && !reject_empty_text {
        return Ok(());
    }
    let parsed = Analyzer::parse(native_sql)?;
    for stmt in &parsed {
        let res = visit_expressions(stmt, |expr| {
            if let Expr::Value(v) = expr
                && let Value::SingleQuotedString(s)
                | Value::DollarQuotedString(DollarQuotedString { value: s, .. }) = &v.value
            {
                if reject_empty_text && s.is_empty() {
                    return ControlFlow::Break(EngineError::unsupported(
                        "empty string is not certified in Oracle-compatible SQL mode",
                    ));
                }
                if ascii_only_text && !s.is_ascii() {
                    return ControlFlow::Break(EngineError::unsupported(
                        "non-ASCII text is not certified under SQL_ASCII encoding",
                    ));
                }
            }
            ControlFlow::Continue(())
        });
        if let ControlFlow::Break(err) = res {
            return Err(err);
        }
    }
    Ok(())
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
    let mysql = source.kind.is_mysql_family();
    let kingbasees = source.kind == DataSourceKind::Kingbasees;
    // Strip PostgreSQL placeholder casts BEFORE rewriting `$N` placeholders because
    // `visit_expressions_mut` visits children bottom-up before parent `Expr::Cast` nodes.
    let _ = visit_expressions_mut(&mut parsed[0], |expr| {
        if question_marks
            && let Expr::Cast { expr: inner, .. } = expr
            && matches!(inner.as_ref(), Expr::Value(v) if matches!(&v.value, Value::Placeholder(p) if p.starts_with('$')))
        {
            // Non-PG guarded engines bind typed parameters out-of-band via struct_pack/ODBC and
            // reject PostgreSQL `::TYPE` placeholder cast syntax.
            *expr = (**inner).clone();
        } else if kingbasees
            && let Expr::Cast {
                expr: inner,
                data_type,
                ..
            } = expr
            && is_unqualified_date(data_type)
            && let Expr::Value(v) = inner.as_ref()
            && let Value::Placeholder(p) = &v.value
            && let Some(rest) = p.strip_prefix('$')
            && let Ok(idx) = rest.parse::<usize>()
            && idx >= 1
            && param_types.get(idx - 1) == Some(&PgType::Date)
        {
            *expr = (**inner).clone();
        }
        ControlFlow::Continue::<()>(())
    });
    let res = visit_expressions_mut(&mut parsed[0], |expr| {
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
                        PgType::Int4 => {
                            replacement =
                                Some(cast_placeholder("?", oracle_custom_type("NUMBER(10,0)")));
                        }
                        PgType::Int8 => {
                            replacement =
                                Some(cast_placeholder("?", oracle_custom_type("NUMBER(19,0)")));
                        }
                        PgType::Float4 => {
                            replacement =
                                Some(cast_placeholder("?", oracle_custom_type("BINARY_FLOAT")));
                        }
                        PgType::Float8 => {
                            replacement =
                                Some(cast_placeholder("?", oracle_custom_type("BINARY_DOUBLE")));
                        }
                        _ => {}
                    }
                } else if mysql && param_types.get(zero_idx) == Some(&PgType::Time) {
                    replacement = Some(mysql_time_placeholder());
                } else if kingbasees && param_types.get(zero_idx) == Some(&PgType::Date) {
                    replacement = Some(cast_placeholder(p, pg_catalog_date_type()));
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
    if oracle || kingbasees {
        let cast_res = visit_expressions_mut(&mut parsed[0], |expr| {
            if let Expr::Cast {
                kind, data_type, ..
            } = expr
            {
                if oracle {
                    if *kind == CastKind::DoubleColon {
                        *kind = CastKind::Cast;
                    }
                    if let Some(target) = oracle_scalar_cast_target(data_type) {
                        *data_type = target;
                    }
                } else if kingbasees && is_unqualified_date(data_type) {
                    *data_type = pg_catalog_date_type();
                }
            }
            ControlFlow::Continue::<EngineError>(())
        });
        if let ControlFlow::Break(e) = cast_res {
            return Err(e);
        }
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
    let mut text_literals = Vec::new();
    if kingbasees {
        let _ = visit_expressions(&parsed[0], |expr| {
            if let Expr::Value(v) = expr
                && let Value::SingleQuotedString(s)
                | Value::DollarQuotedString(DollarQuotedString { value: s, .. })
                | Value::EscapedStringLiteral(s) = &v.value
            {
                text_literals.push(s.clone());
            }
            ControlFlow::Continue::<()>(())
        });
    }
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
        text_literals,
    }))
}
