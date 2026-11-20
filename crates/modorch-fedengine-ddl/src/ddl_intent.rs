use crate::model::{DdlIntent, IndexIntent, ObjectRef};
use modorch_fedengine_contract::{DdlColumnSpec, DdlConstraintSpec, GuardedDdlPlan};
use modorch_fedengine_core::{DataSourceDefinition, DataSourceKind, EngineError, Result};
use sqlparser::ast::*;

fn ident_value(i: &Ident) -> String {
    i.value.clone()
}

fn column_spec(col: &ColumnDef) -> Result<Option<DdlColumnSpec>> {
    // Inline column options (NOT NULL, PRIMARY KEY, REFERENCES, DEFAULT, etc.) are not represented
    // in the native guarded CREATE TABLE / ADD COLUMN ABI; let the standard DDL path handle them.
    if !col.options.is_empty() {
        return Ok(None);
    }
    let name = ident_value(&col.name);
    let spec = match &col.data_type {
        DataType::SmallInt(_) => DdlColumnSpec {
            name,
            type_name: "smallint".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => DdlColumnSpec {
            name,
            type_name: "integer".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::BigInt(_) | DataType::Int8(_) => DdlColumnSpec {
            name,
            type_name: "bigint".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::Varchar(len) | DataType::CharacterVarying(len) => match len {
            Some(CharacterLength::IntegerLength { length, .. }) => DdlColumnSpec {
                name,
                type_name: "varchar".into(),
                length: i32::try_from(*length)
                    .map_err(|_| EngineError::unsupported("VARCHAR length out of range"))?,
                precision: 0,
                scale: 0,
            },
            _ => DdlColumnSpec {
                name,
                type_name: "text".into(),
                length: 0,
                precision: 0,
                scale: 0,
            },
        },
        DataType::Text => DdlColumnSpec {
            name,
            type_name: "text".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
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
            DdlColumnSpec {
                name,
                type_name: "numeric".into(),
                length: 0,
                precision,
                scale,
            }
        }
        DataType::Date => DdlColumnSpec {
            name,
            type_name: "date".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::Time(p, _) => DdlColumnSpec {
            name,
            type_name: "time".into(),
            length: 0,
            precision: p.unwrap_or(0) as i32,
            scale: 0,
        },
        DataType::Timestamp(p, tz) => {
            let with_tz = matches!(tz, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz);
            DdlColumnSpec {
                name,
                type_name: if with_tz { "timestamptz" } else { "timestamp" }.into(),
                length: 0,
                precision: p.unwrap_or(0) as i32,
                scale: 0,
            }
        }
        DataType::Real | DataType::Float4 => DdlColumnSpec {
            name,
            type_name: "real".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::Double(_) | DataType::DoublePrecision | DataType::Float8 => DdlColumnSpec {
            name,
            type_name: "double".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::Boolean => DdlColumnSpec {
            name,
            type_name: "boolean".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::Uuid => DdlColumnSpec {
            name,
            type_name: "uuid".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
        DataType::Bytea => DdlColumnSpec {
            name,
            type_name: "bytea".into(),
            length: 0,
            precision: 0,
            scale: 0,
        },
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
        identity_mode: String::new(),
        columns: Vec::new(),
        keys: Vec::new(),
        conditional: false,
        only: false,
    }
}

fn default_literal(expr: &Expr) -> Result<(String, String)> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Null => Ok(("null".into(), String::new())),
            Value::Number(n, false) => Ok(("integer".into(), n.clone())),
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

fn constraint_spec(c: &TableConstraint, default_schema: &str) -> Result<DdlConstraintSpec> {
    match c {
        TableConstraint::PrimaryKey(pk) => {
            let name = pk
                .name
                .as_ref()
                .map(ident_value)
                .unwrap_or_else(|| "PRIMARY".into());
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
            let name =
                u.name.as_ref().map(ident_value).ok_or_else(|| {
                    EngineError::unsupported("UNIQUE constraint name is required")
                })?;
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
            let name = fk.name.as_ref().map(ident_value).ok_or_else(|| {
                EngineError::unsupported("FOREIGN KEY constraint name is required")
            })?;
            let parts = fk
                .foreign_table
                .0
                .iter()
                .map(|p| match p {
                    ObjectNamePart::Identifier(i) => Ok(ident_value(i)),
                    _ => Err(EngineError::unsupported("invalid referenced table")),
                })
                .collect::<Result<Vec<_>>>()?;
            let (ref_schema, ref_table) = match parts.as_slice() {
                [_cat, schema, table] => (schema.clone(), table.clone()),
                [schema, table] => (schema.clone(), table.clone()),
                [table] => (default_schema.to_string(), table.clone()),
                _ => return Err(EngineError::unsupported("invalid referenced table")),
            };
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
            let name = chk
                .name
                .as_ref()
                .map(ident_value)
                .ok_or_else(|| EngineError::unsupported("CHECK constraint name is required"))?;
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
        Statement::CreateTable(t) => {
            if t.query.is_some() || !t.constraints.is_empty() || t.columns.is_empty() {
                return Ok(None);
            }
            let mut columns = Vec::with_capacity(t.columns.len());
            for c in &t.columns {
                let Some(spec) = column_spec(c)? else {
                    return Ok(None);
                };
                columns.push(spec);
            }
            let mut intent = empty_intent("create_table", &target.name);
            intent.columns = columns;
            intent.conditional = t.if_not_exists;
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
                    let Some(spec) = column_spec(column_def)? else {
                        return Ok(None);
                    };
                    let mut i = empty_intent("add_column", &target.name);
                    i.columns = vec![spec];
                    i.conditional = *if_not_exists;
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
                    _ => return Ok(None),
                },
                AlterTableOperation::AddConstraint { constraint, .. } => {
                    let spec = constraint_spec(constraint, &target.schema)?;
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

fn contract_version(intent: &DdlIntent, kind: DataSourceKind) -> i32 {
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
        "rename_table" | "add_column" | "rename_column" | "rename_index" => 3,
        "set_default" | "drop_default" | "set_not_null" | "drop_not_null" | "add_constraint"
        | "drop_constraint" | "truncate" | "reindex" => 4,
        _ => 2,
    }
}

pub(crate) fn build_guarded_plan(
    intent: DdlIntent,
    source: &DataSourceDefinition,
    database: String,
    schema: String,
    owner: String,
) -> Result<Option<GuardedDdlPlan>> {
    // Native *_guarded_ddl contract covers PostgreSQL and MySQL scanners; other sources stay on
    // their configured adapter or fail closed in policy/execution.
    if !matches!(
        source.kind,
        DataSourceKind::Postgres | DataSourceKind::Mysql
    ) {
        return Ok(None);
    }
    if intent.only || intent.conditional {
        return Err(EngineError::unsupported(
            "ONLY and conditional DDL modifiers are disabled on guarded native DDL",
        ));
    }
    let mysql = source.kind == DataSourceKind::Mysql;
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
        if name.len() > 63 {
            return Err(EngineError::unsupported(
                "DDL identifier exceeds 63-byte native limit",
            ));
        }
    }
    let version = contract_version(&intent, source.kind);
    if mysql {
        if matches!(
            intent.action.as_str(),
            "set_not_null" | "drop_not_null" | "reindex"
        ) || (intent.action == "truncate" && intent.identity_mode != "restart")
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
    for c in &intent.columns {
        if c.name.len() > 63 {
            return Err(EngineError::unsupported("column name exceeds 63 bytes"));
        }
        if mysql
            && (c.length > 16383
                || matches!(
                    c.type_name.as_str(),
                    "boolean" | "time" | "timestamptz" | "uuid" | "bytea"
                )
                || (c.type_name == "numeric" && c.scale > 30))
        {
            return Err(EngineError::unsupported(
                "MySQL native DDL column type is outside contract",
            ));
        }
        if !mysql && c.type_name == "numeric" && c.precision > 38 {
            return Err(EngineError::unsupported(
                "PostgreSQL DDL numeric precision exceeds 38",
            ));
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
        identity_mode: intent.identity_mode,
        columns: intent.columns,
        keys: intent.keys,
        contract_version: version,
    }))
}
