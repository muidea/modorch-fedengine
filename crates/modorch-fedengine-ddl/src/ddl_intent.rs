use crate::model::{DdlIntent, IndexIntent, ObjectRef};
use modorch_fedengine_contract::{DdlColumnSpec, DdlConstraintSpec, GuardedDdlPlan};
use modorch_fedengine_core::{DataSourceDefinition, DataSourceKind, EngineError, Result};
use sqlparser::ast::*;

fn ident_value(i: &Ident) -> String {
    i.value.clone()
}

fn default_col_spec(
    name: String,
    type_name: impl Into<String>,
    length: i32,
    precision: i32,
    scale: i32,
) -> DdlColumnSpec {
    DdlColumnSpec {
        name,
        type_name: type_name.into(),
        length,
        precision,
        scale,
        not_null: false,
        default_kind: None,
        default_value: None,
        primary_key: false,
        unique: false,
    }
}

fn fk_ref_parts(foreign_table: &ObjectName, default_schema: &str) -> Result<(String, String)> {
    let parts = foreign_table
        .0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => Ok(ident_value(i)),
            _ => Err(EngineError::unsupported("invalid referenced table")),
        })
        .collect::<Result<Vec<_>>>()?;
    match parts.as_slice() {
        [_cat, schema, table] => Ok((schema.clone(), table.clone())),
        [schema, table] => Ok((schema.clone(), table.clone())),
        [table] => Ok((default_schema.to_string(), table.clone())),
        _ => Err(EngineError::unsupported("invalid referenced table")),
    }
}

fn column_spec(
    col: &ColumnDef,
    mut extra_constraints: Option<&mut Vec<DdlConstraintSpec>>,
    default_schema: &str,
) -> Result<Option<DdlColumnSpec>> {
    let Some(mut spec) = column_spec_from_type(ident_value(&col.name), &col.data_type)? else {
        return Ok(None);
    };
    for opt in &col.options {
        match &opt.option {
            ColumnOption::NotNull => spec.not_null = true,
            ColumnOption::Null => spec.not_null = false,
            ColumnOption::Default(expr) => {
                let (kind, val) = default_literal(expr)?;
                spec.default_kind = Some(kind);
                spec.default_value = Some(val);
            }
            ColumnOption::PrimaryKey(_) => spec.primary_key = true,
            ColumnOption::Unique(_) => spec.unique = true,
            ColumnOption::ForeignKey(fk) => {
                let Some(constraints) = extra_constraints.as_deref_mut() else {
                    return Ok(None);
                };
                let name = fk.name.as_ref().map(ident_value).unwrap_or_default();
                let (reference_schema, reference_table) =
                    fk_ref_parts(&fk.foreign_table, default_schema)?;
                constraints.push(DdlConstraintSpec {
                    name,
                    kind: "foreign_key".into(),
                    columns: vec![spec.name.clone()],
                    reference_schema,
                    reference_table,
                    reference_columns: fk.referred_columns.iter().map(ident_value).collect(),
                    check_column: String::new(),
                    check_operator: String::new(),
                    check_value: String::new(),
                });
            }
            ColumnOption::Check(chk) => {
                let Some(constraints) = extra_constraints.as_deref_mut() else {
                    return Ok(None);
                };
                let name = chk.name.as_ref().map(ident_value).unwrap_or_default();
                let (check_column, check_operator, check_value) = parse_check_expr(&chk.expr)?;
                constraints.push(DdlConstraintSpec {
                    name,
                    kind: "check".into(),
                    columns: Vec::new(),
                    reference_schema: String::new(),
                    reference_table: String::new(),
                    reference_columns: Vec::new(),
                    check_column,
                    check_operator,
                    check_value,
                });
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(spec))
}

fn column_spec_from_type(name: String, data_type: &DataType) -> Result<Option<DdlColumnSpec>> {
    let spec = match data_type {
        DataType::SmallInt(_) => default_col_spec(name, "smallint", 0, 0, 0),
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => {
            default_col_spec(name, "integer", 0, 0, 0)
        }
        DataType::BigInt(_) | DataType::Int8(_) => default_col_spec(name, "bigint", 0, 0, 0),
        DataType::Varchar(len) | DataType::CharacterVarying(len) => match len {
            Some(CharacterLength::IntegerLength { length, .. }) => default_col_spec(
                name,
                "varchar",
                i32::try_from(*length)
                    .map_err(|_| EngineError::unsupported("VARCHAR length out of range"))?,
                0,
                0,
            ),
            _ => default_col_spec(name, "text", 0, 0, 0),
        },
        DataType::Text => default_col_spec(name, "text", 0, 0, 0),
        DataType::Numeric(info) | DataType::Decimal(info) => {
            let (precision, scale) = match info {
                ExactNumberInfo::None => return Ok(None),
                ExactNumberInfo::Precision(p) => (
                    i32::try_from(*p)
                        .map_err(|_| EngineError::unsupported("DECIMAL precision out of range"))?,
                    0,
                ),
                ExactNumberInfo::PrecisionAndScale(p, s) => (
                    i32::try_from(*p)
                        .map_err(|_| EngineError::unsupported("DECIMAL precision out of range"))?,
                    i32::try_from(*s)
                        .map_err(|_| EngineError::unsupported("DECIMAL scale out of range"))?,
                ),
            };
            default_col_spec(name, "numeric", 0, precision, scale)
        }
        DataType::Date => default_col_spec(name, "date", 0, 0, 0),
        DataType::Time(p, _) => default_col_spec(name, "time", 0, p.unwrap_or(0) as i32, 0),
        DataType::Timestamp(p, tz) => {
            let with_tz = matches!(tz, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz);
            default_col_spec(
                name,
                if with_tz { "timestamptz" } else { "timestamp" },
                0,
                p.unwrap_or(0) as i32,
                0,
            )
        }
        DataType::Real | DataType::Float4 => default_col_spec(name, "real", 0, 0, 0),
        DataType::Double(_) | DataType::DoublePrecision | DataType::Float8 => {
            default_col_spec(name, "double", 0, 0, 0)
        }
        DataType::Boolean => default_col_spec(name, "boolean", 0, 0, 0),
        DataType::Uuid => default_col_spec(name, "uuid", 0, 0, 0),
        DataType::Bytea => default_col_spec(name, "bytea", 0, 0, 0),
        _ => return Ok(None),
    };
    Ok(Some(spec))
}

fn empty_intent(action: &str, table_name: &str) -> DdlIntent {
    DdlIntent {
        action: action.to_string(),
        table_name: table_name.to_string(),
        index_name: String::new(),
        owner: String::new(),
        new_name: String::new(),
        column_name: String::new(),
        default_kind: String::new(),
        default_value: String::new(),
        constraint_name: String::new(),
        constraint: None,
        constraints: Vec::new(),
        identity_mode: String::new(),
        columns: Vec::new(),
        keys: Vec::new(),
        conditional: false,
        only: false,
        query_sql: None,
    }
}

fn default_literal(expr: &Expr) -> Result<(String, String)> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Null => Ok(("null".into(), String::new())),
            Value::Number(n, false) => Ok(("integer".into(), n.clone())),
            Value::Boolean(b) => Ok(("boolean".into(), b.to_string())),
            Value::SingleQuotedString(s) => Ok(("string".into(), s.clone())),
            _ => Err(EngineError::unsupported("default literal is not enabled")),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => {
            if let Expr::Value(v) = expr.as_ref()
                && let Value::Number(n, false) = &v.value
            {
                return Ok(("integer".into(), format!("-{n}")));
            }
            Err(EngineError::unsupported("default literal is not enabled"))
        }
        _ => Err(EngineError::unsupported(
            "default expression is not enabled",
        )),
    }
}

fn parse_check_expr(expr: &Expr) -> Result<(String, String, String)> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return Err(EngineError::unsupported(
            "CHECK constraint must compare a column to an integer literal",
        ));
    };
    let Expr::Identifier(col) = left.as_ref() else {
        return Err(EngineError::unsupported(
            "CHECK left operand must be a column identifier",
        ));
    };
    let op_str = match op {
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "<>",
        BinaryOperator::Lt => "<",
        BinaryOperator::Gt => ">",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::GtEq => ">=",
        _ => return Err(EngineError::unsupported("CHECK operator is not enabled")),
    };
    let val = match right.as_ref() {
        Expr::Value(v) => match &v.value {
            Value::Number(n, false) => n.clone(),
            _ => return Err(EngineError::unsupported("CHECK value must be integer")),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => {
            if let Expr::Value(v) = expr.as_ref()
                && let Value::Number(n, false) = &v.value
            {
                format!("-{n}")
            } else {
                return Err(EngineError::unsupported("CHECK value must be integer"));
            }
        }
        _ => return Err(EngineError::unsupported("CHECK value must be integer")),
    };
    Ok((ident_value(col), op_str.into(), val))
}

fn index_column_names(columns: &[IndexColumn]) -> Result<Vec<String>> {
    columns
        .iter()
        .map(|c| {
            c.column
                .expr
                .expr_ident()
                .as_ref()
                .map(ident_value)
                .ok_or_else(|| EngineError::unsupported("expression keys are disabled"))
        })
        .collect()
}

fn constraint_spec(
    c: &TableConstraint,
    default_schema: &str,
    require_name: bool,
) -> Result<DdlConstraintSpec> {
    match c {
        TableConstraint::PrimaryKey(pk) => {
            let name = pk.name.as_ref().map(ident_value).unwrap_or_else(|| {
                if require_name {
                    "PRIMARY".into()
                } else {
                    String::new()
                }
            });
            let columns = index_column_names(&pk.columns)?;
            Ok(DdlConstraintSpec {
                name,
                kind: "primary_key".into(),
                columns,
                reference_schema: String::new(),
                reference_table: String::new(),
                reference_columns: Vec::new(),
                check_column: String::new(),
                check_operator: String::new(),
                check_value: String::new(),
            })
        }
        TableConstraint::Unique(u) => {
            let name = match u.name.as_ref() {
                Some(id) => ident_value(id),
                None if require_name => {
                    return Err(EngineError::unsupported(
                        "UNIQUE constraint name is required",
                    ));
                }
                None => String::new(),
            };
            let columns = index_column_names(&u.columns)?;
            Ok(DdlConstraintSpec {
                name,
                kind: "unique".into(),
                columns,
                reference_schema: String::new(),
                reference_table: String::new(),
                reference_columns: Vec::new(),
                check_column: String::new(),
                check_operator: String::new(),
                check_value: String::new(),
            })
        }
        TableConstraint::ForeignKey(fk) => {
            let name = match fk.name.as_ref() {
                Some(id) => ident_value(id),
                None if require_name => {
                    return Err(EngineError::unsupported(
                        "FOREIGN KEY constraint name is required",
                    ));
                }
                None => String::new(),
            };
            let (ref_schema, ref_table) = fk_ref_parts(&fk.foreign_table, default_schema)?;
            Ok(DdlConstraintSpec {
                name,
                kind: "foreign_key".into(),
                columns: fk.columns.iter().map(ident_value).collect(),
                reference_schema: ref_schema,
                reference_table: ref_table,
                reference_columns: fk.referred_columns.iter().map(ident_value).collect(),
                check_column: String::new(),
                check_operator: String::new(),
                check_value: String::new(),
            })
        }
        TableConstraint::Check(chk) => {
            let name = match chk.name.as_ref() {
                Some(id) => ident_value(id),
                None if require_name => {
                    return Err(EngineError::unsupported(
                        "CHECK constraint name is required",
                    ));
                }
                None => String::new(),
            };
            let (check_column, check_operator, check_value) = parse_check_expr(&chk.expr)?;
            Ok(DdlConstraintSpec {
                name,
                kind: "check".into(),
                columns: Vec::new(),
                reference_schema: String::new(),
                reference_table: String::new(),
                reference_columns: Vec::new(),
                check_column,
                check_operator,
                check_value,
            })
        }
        _ => Err(EngineError::unsupported(
            "constraint variant is not enabled",
        )),
    }
}

trait ExprIdentExt {
    fn expr_ident(&self) -> Option<Ident>;
}
impl ExprIdentExt for Expr {
    fn expr_ident(&self) -> Option<Ident> {
        match self {
            Expr::Identifier(id) => Some(id.clone()),
            _ => None,
        }
    }
}

pub(crate) fn extract_ddl_intent(
    statement: &Statement,
    primary: Option<&ObjectRef>,
    index: Option<&IndexIntent>,
) -> Result<Option<DdlIntent>> {
    let Some(target) = primary else {
        return Ok(None);
    };
    if let Some(idx) = index {
        let mut intent = if idx.create {
            let mut i = empty_intent(
                if idx.unique {
                    "create_unique_index"
                } else {
                    "create_index"
                },
                &idx.table.name,
            );
            i.index_name = idx.name.clone();
            i.keys = idx.columns.clone();
            i
        } else {
            let mut i = empty_intent("drop_index", &idx.name);
            i.owner = idx.table.name.clone();
            i
        };
        intent.conditional = idx.if_exists;
        return Ok(Some(intent));
    }
    match statement {
        Statement::CreateSchema { if_not_exists, .. } => {
            let mut intent = empty_intent("create_schema", &target.schema);
            intent.conditional = *if_not_exists;
            Ok(Some(intent))
        }
        Statement::CreateTable(t) => {
            if let Some(q) = &t.query {
                if !t.columns.is_empty() || !t.constraints.is_empty() {
                    return Ok(None);
                }
                let mut intent = empty_intent("create_table_as", &target.name);
                intent.conditional = t.if_not_exists;
                intent.query_sql = Some(q.to_string());
                return Ok(Some(intent));
            }
            if t.columns.is_empty() {
                return Ok(None);
            }
            let mut columns = Vec::with_capacity(t.columns.len());
            let mut constraints = Vec::with_capacity(t.constraints.len());
            for c in &t.columns {
                let Some(spec) = column_spec(c, Some(&mut constraints), &target.schema)? else {
                    return Ok(None);
                };
                columns.push(spec);
            }
            for c in &t.constraints {
                constraints.push(constraint_spec(c, &target.schema, false)?);
            }
            let mut intent = empty_intent("create_table", &target.name);
            intent.columns = columns;
            intent.constraints = constraints;
            intent.conditional = t.if_not_exists;
            Ok(Some(intent))
        }
        Statement::Drop {
            object_type: ObjectType::Schema,
            if_exists,
            ..
        } => {
            let mut intent = empty_intent("drop_schema", &target.schema);
            intent.conditional = *if_exists;
            Ok(Some(intent))
        }
        Statement::Drop {
            object_type: ObjectType::Table,
            if_exists,
            ..
        } => {
            let mut intent = empty_intent("drop_table", &target.name);
            intent.conditional = *if_exists;
            Ok(Some(intent))
        }
        Statement::Truncate(tr) => {
            let mut intent = empty_intent("truncate", &target.name);
            intent.conditional = tr.if_exists;
            intent.identity_mode = match tr.identity {
                Some(TruncateIdentityOption::Restart) => "restart".into(),
                Some(TruncateIdentityOption::Continue) => "continue".into(),
                None => String::new(),
            };
            Ok(Some(intent))
        }
        Statement::AlterIndex {
            name,
            operation: AlterIndexOperation::RenameIndex { index_name },
        } => {
            let last_old = name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .ok_or_else(|| EngineError::unsupported("invalid index name"))?;
            let last_new = index_name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .ok_or_else(|| EngineError::unsupported("invalid new index name"))?;
            let mut intent = empty_intent("rename_index", &ident_value(last_old));
            intent.new_name = ident_value(last_new);
            intent.owner = target.name.clone();
            Ok(Some(intent))
        }
        Statement::AlterTable(t) if t.operations.len() == 1 => {
            let mut intent = match &t.operations[0] {
                AlterTableOperation::AddColumn {
                    if_not_exists,
                    column_def,
                    ..
                } => {
                    let Some(spec) = column_spec(column_def, None, &target.schema)? else {
                        return Ok(None);
                    };
                    let mut i = empty_intent("add_column", &target.name);
                    i.columns = vec![spec];
                    i.conditional = *if_not_exists;
                    i
                }
                AlterTableOperation::DropColumn {
                    if_exists,
                    column_names,
                    ..
                } => {
                    let [column_name] = column_names.as_slice() else {
                        return Ok(None);
                    };
                    let mut i = empty_intent("drop_column", &target.name);
                    i.column_name = ident_value(column_name);
                    i.conditional = *if_exists;
                    i
                }
                AlterTableOperation::RenameColumn {
                    old_column_name,
                    new_column_name,
                } => {
                    let mut i = empty_intent("rename_column", &target.name);
                    i.column_name = ident_value(old_column_name);
                    i.new_name = ident_value(new_column_name);
                    i
                }
                AlterTableOperation::RenameTable { table_name } => {
                    let new_name_obj = match table_name {
                        RenameTableNameKind::To(n) | RenameTableNameKind::As(n) => n,
                    };
                    let ident = new_name_obj
                        .0
                        .last()
                        .and_then(|p| p.as_ident())
                        .ok_or_else(|| EngineError::unsupported("invalid renamed table name"))?;
                    let mut i = empty_intent("rename_table", &target.name);
                    i.new_name = ident_value(ident);
                    i
                }
                AlterTableOperation::AlterColumn { column_name, op } => match op {
                    AlterColumnOperation::SetDefault { value } => {
                        let (kind, val) = default_literal(value)?;
                        let mut i = empty_intent("set_default", &target.name);
                        i.column_name = ident_value(column_name);
                        i.default_kind = kind;
                        i.default_value = val;
                        i
                    }
                    AlterColumnOperation::DropDefault => {
                        let mut i = empty_intent("drop_default", &target.name);
                        i.column_name = ident_value(column_name);
                        i
                    }
                    AlterColumnOperation::SetNotNull => {
                        let mut i = empty_intent("set_not_null", &target.name);
                        i.column_name = ident_value(column_name);
                        i
                    }
                    AlterColumnOperation::DropNotNull => {
                        let mut i = empty_intent("drop_not_null", &target.name);
                        i.column_name = ident_value(column_name);
                        i
                    }
                    AlterColumnOperation::SetDataType { data_type, .. } => {
                        let col = ident_value(column_name);
                        let Some(spec) = column_spec_from_type(col.clone(), data_type)? else {
                            return Ok(None);
                        };
                        let mut i = empty_intent("alter_column_type", &target.name);
                        i.column_name = col;
                        i.columns = vec![spec];
                        i
                    }
                    _ => return Ok(None),
                },
                AlterTableOperation::AddConstraint { constraint, .. } => {
                    let spec = constraint_spec(constraint, &target.schema, true)?;
                    let mut i = empty_intent("add_constraint", &target.name);
                    i.constraint_name = spec.name.clone();
                    i.constraint = Some(spec);
                    i
                }
                AlterTableOperation::DropConstraint {
                    if_exists, name, ..
                } => {
                    let mut i = empty_intent("drop_constraint", &target.name);
                    i.constraint_name = ident_value(name);
                    i.conditional = *if_exists;
                    i
                }
                _ => return Ok(None),
            };
            intent.only = t.only;
            Ok(Some(intent))
        }
        _ => Ok(None),
    }
}

fn has_v6_column_modifiers(columns: &[DdlColumnSpec]) -> bool {
    columns
        .iter()
        .any(|c| c.not_null || c.default_kind.is_some() || c.primary_key || c.unique)
}

fn rewrite_query_for_dialect(query_sql: &str, source: &DataSourceDefinition) -> Result<String> {
    let mut parsed = crate::Analyzer::parse(query_sql)?;
    if parsed.len() != 1 {
        return Err(EngineError::unsupported(
            "CREATE TABLE AS query must contain a single SELECT statement",
        ));
    }
    let Statement::Query(mut q) = parsed.remove(0) else {
        return Err(EngineError::unsupported(
            "CREATE TABLE AS requires a query source",
        ));
    };
    if source.kind == DataSourceKind::Sqlserver && q.with.is_some() {
        return Err(EngineError::unsupported(
            "SQL Server guarded CREATE TABLE AS does not support WITH CTEs",
        ));
    }
    let res = visit_expressions_mut(&mut q, |expr| {
        if let Expr::Value(v) = expr
            && matches!(v.value, Value::Placeholder(_))
        {
            return std::ops::ControlFlow::Break(EngineError::unsupported(
                "CREATE TABLE AS does not accept query parameters",
            ));
        }
        std::ops::ControlFlow::Continue(())
    });
    if let std::ops::ControlFlow::Break(e) = res {
        return Err(e);
    }
    let rel_res = visit_relations_mut(&mut q, |name| {
        match crate::mutation::rewrite_relation(name, source.kind, &source.catalog_alias) {
            Ok(()) => std::ops::ControlFlow::Continue(()),
            Err(e) => std::ops::ControlFlow::Break(e),
        }
    });
    if let std::ops::ControlFlow::Break(e) = rel_res {
        return Err(e);
    }
    Ok(q.to_string())
}

fn contract_version(intent: &DdlIntent, kind: DataSourceKind) -> i32 {
    if intent.action == "create_table_as"
        || (intent.action == "create_table"
            && (!intent.constraints.is_empty() || has_v6_column_modifiers(&intent.columns)))
        || (intent.action == "add_column" && has_v6_column_modifiers(&intent.columns))
    {
        return 6;
    }
    if intent.action == "create_table" || intent.action == "add_column" {
        for c in &intent.columns {
            match c.type_name.as_str() {
                "smallint" | "integer" | "bigint" | "varchar" => {}
                "text" if kind.is_mysql_family() => return 5,
                "text" => {}
                _ => return 5,
            }
        }
    }
    match intent.action.as_str() {
        "drop_column" | "alter_column_type" | "create_schema" | "drop_schema" => 5,
        "rename_table" | "add_column" | "rename_column" | "rename_index" => 3,
        "set_default" | "drop_default" | "set_not_null" | "drop_not_null" | "add_constraint"
        | "drop_constraint" | "truncate" | "reindex" => 4,
        _ => 2,
    }
}

pub(crate) fn build_guarded_plan(
    mut intent: DdlIntent,
    source: &DataSourceDefinition,
    database: String,
    schema: String,
    owner: String,
) -> Result<Option<GuardedDdlPlan>> {
    if source.kind == DataSourceKind::Starrocks {
        return Err(EngineError::unsupported(
            "StarRocks data sources are read-only; DDL is not enabled",
        ));
    }
    if intent.action == "create_table_as" {
        if source.kind.is_postgres_family() || source.kind.is_mysql_family() {
            return Ok(None);
        }
        let raw_q = intent
            .query_sql
            .as_deref()
            .ok_or_else(|| EngineError::unsupported("CREATE TABLE AS requires a source query"))?;
        intent.query_sql = Some(rewrite_query_for_dialect(raw_q, source)?);
    }
    if intent.only {
        return Err(EngineError::unsupported(
            "ONLY DDL modifier is disabled on guarded native DDL",
        ));
    }
    let max_ident = if source.kind.is_postgres_family() || source.kind.is_mysql_family() {
        63
    } else {
        128
    };
    for name in [
        database.as_str(),
        schema.as_str(),
        intent.table_name.as_str(),
        intent.index_name.as_str(),
        intent.new_name.as_str(),
        intent.column_name.as_str(),
        intent.constraint_name.as_str(),
        owner.as_str(),
    ] {
        if name.len() > max_ident {
            return Err(EngineError::unsupported(
                "DDL identifier exceeds native limit",
            ));
        }
    }
    let version = contract_version(&intent, source.kind);
    let mysql = matches!(source.kind, DataSourceKind::Mysql | DataSourceKind::Mariadb);
    match source.kind {
        DataSourceKind::Mysql | DataSourceKind::Mariadb => {
            if intent.action == "reindex"
                || (intent.action == "truncate" && intent.identity_mode != "restart")
            {
                return Err(EngineError::unsupported(
                    "DDL action has no equivalent native MySQL contract",
                ));
            }
            if let Some(c) = &intent.constraint
                && c.kind == "primary_key"
                && c.name != "PRIMARY"
            {
                return Err(EngineError::unsupported(
                    "MySQL primary key constraint name must be PRIMARY",
                ));
            }
        }
        DataSourceKind::Oracle => {
            if matches!(intent.action.as_str(), "create_schema" | "drop_schema") {
                return Err(EngineError::unsupported(
                    "Oracle schema DDL requires user/tablespace management outside guarded DDL",
                ));
            }
        }
        DataSourceKind::Db2 | DataSourceKind::Hana => {
            if intent.action == "reindex" {
                return Err(EngineError::unsupported(
                    "REINDEX is not enabled for this database dialect",
                ));
            }
        }
        _ => {}
    }
    for c in intent.constraint.iter().chain(intent.constraints.iter()) {
        if c.name.len() > max_ident
            || c.reference_table.len() > max_ident
            || c.check_column.len() > max_ident
            || c.columns.iter().any(|k| k.len() > max_ident)
            || c.reference_columns.iter().any(|k| k.len() > max_ident)
        {
            return Err(EngineError::unsupported(
                "DDL constraint identifier exceeds native limit",
            ));
        }
        if c.kind == "foreign_key" && c.reference_schema != schema {
            return Err(EngineError::unsupported(
                "DDL foreign key must reference a table in the same schema",
            ));
        }
    }
    for c in &intent.columns {
        if c.name.len() > max_ident {
            return Err(EngineError::unsupported("column name exceeds native limit"));
        }
        if mysql
            && (c.length > 16383
                || c.type_name == "timestamptz"
                || (c.type_name == "numeric" && c.scale > 30))
        {
            return Err(EngineError::unsupported(
                "MySQL native DDL column type is outside contract",
            ));
        }
        match source.kind {
            DataSourceKind::Postgres | DataSourceKind::Greenplum | DataSourceKind::Kingbasees => {
                if c.type_name == "numeric" && c.precision > 38 {
                    return Err(EngineError::unsupported(
                        "PostgreSQL DDL numeric precision exceeds 38",
                    ));
                }
            }
            DataSourceKind::Oracle => {
                if c.type_name == "time"
                    || c.length > 4000
                    || (c.type_name == "numeric" && c.precision > 38)
                {
                    return Err(EngineError::unsupported(
                        "Oracle native DDL column type is outside contract",
                    ));
                }
            }
            DataSourceKind::Sqlserver => {
                if c.length > 4000 || (c.type_name == "numeric" && c.precision > 38) {
                    return Err(EngineError::unsupported(
                        "SQL Server native DDL column type is outside contract",
                    ));
                }
            }
            DataSourceKind::Dameng => {
                if c.length > 8188 || (c.type_name == "numeric" && c.precision > 38) {
                    return Err(EngineError::unsupported(
                        "Dameng native DDL column type is outside contract",
                    ));
                }
            }
            DataSourceKind::Db2 => {
                if c.type_name == "timestamptz"
                    || c.length > 32672
                    || (c.type_name == "numeric" && c.precision > 31)
                {
                    return Err(EngineError::unsupported(
                        "DB2 native DDL column type is outside contract",
                    ));
                }
            }
            DataSourceKind::Hana => {
                if c.type_name == "timestamptz"
                    || c.length > 5000
                    || (c.type_name == "numeric" && c.precision > 34)
                {
                    return Err(EngineError::unsupported(
                        "SAP HANA native DDL column type is outside contract",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(Some(GuardedDdlPlan {
        source: source.id.clone(),
        kind: source.kind,
        catalog: source.catalog_alias.clone(),
        database,
        schema,
        action: intent.action,
        table_name: intent.table_name,
        index_name: intent.index_name,
        owner,
        new_name: intent.new_name,
        column_name: intent.column_name,
        default_kind: intent.default_kind,
        default_value: intent.default_value,
        constraint_name: intent.constraint_name,
        constraint: intent.constraint,
        constraints: intent.constraints,
        identity_mode: intent.identity_mode,
        columns: intent.columns,
        keys: intent.keys,
        conditional: intent.conditional,
        query_sql: intent.query_sql,
        contract_version: version,
    }))
}
