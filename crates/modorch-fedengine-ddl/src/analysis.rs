use crate::{
    binding::{Binder, bind_constraints, object_name, validate_scalar_type},
    model::*,
};
use modorch_fedengine_core::*;
use sqlparser::{
    ast::*,
    dialect::{DuckDbDialect, PostgreSqlDialect},
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};
use std::{collections::BTreeSet, ops::ControlFlow};
/// Shared immutable relation indexes from the current catalog snapshots.
#[derive(Debug, Clone, Default)]
pub struct RelationNames(Vec<std::sync::Arc<BTreeSet<(String, String)>>>);
impl RelationNames {
    pub fn extend_shared(&mut self, names: std::sync::Arc<BTreeSet<(String, String)>>) {
        self.0.push(names);
    }
    pub fn contains(&self, name: &(String, String)) -> bool {
        self.0.iter().any(|names| names.contains(name))
    }
}
impl<const N: usize> From<[(String, String); N]> for RelationNames {
    fn from(names: [(String, String); N]) -> Self {
        Self(vec![std::sync::Arc::new(names.into_iter().collect())])
    }
}

#[derive(Debug, Clone)]
pub struct BindingNamespace {
    pub catalog: String,
    pub allow_cross_database: bool,
    pub bindings: std::collections::BTreeMap<String, (DataSourceId, String)>,
    pub routes: Option<std::collections::BTreeMap<(String, String), (DataSourceId, String)>>,
    pub search_path: Vec<String>,
    pub relations: RelationNames,
}
#[derive(Debug, Clone)]
pub struct Analyzer {
    pub namespace: Option<BindingNamespace>,
    pub sources: Vec<DataSourceDefinition>,
    pub indexes: Vec<IndexOwner>,
}
fn split_pg_fallback_statements(sql: &str) -> Result<Option<Vec<String>>> {
    if modorch_fedengine_pg_compat::parse_special_timezone_command(sql).is_some() {
        return Ok(Some(vec![
            sql.trim().trim_end_matches(';').trim().to_string(),
        ]));
    }
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location() else {
        return Ok(None);
    };
    let semi_locations: Vec<(u64, u64)> = tokens
        .iter()
        .filter(|t| matches!(t.token, Token::SemiColon))
        .map(|t| (t.span.start.line, t.span.start.column))
        .collect();
    if semi_locations.is_empty() {
        return Ok(None);
    }

    let mut split_offsets = Vec::with_capacity(semi_locations.len());
    let mut loc_idx = 0usize;
    let mut line = 1u64;
    let mut col = 1u64;
    for (byte_idx, ch) in sql.char_indices() {
        if loc_idx < semi_locations.len() && (line, col) == semi_locations[loc_idx] {
            split_offsets.push(byte_idx);
            loc_idx += 1;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    if loc_idx != semi_locations.len() {
        return Ok(None);
    }

    let mut raw_chunks = Vec::new();
    let mut start = 0usize;
    for semi_idx in split_offsets {
        let trimmed = sql[start..semi_idx].trim();
        if !trimmed.is_empty() {
            raw_chunks.push(trimmed.to_string());
        }
        start = semi_idx + 1;
    }
    let tail = sql[start..].trim();
    if !tail.is_empty() {
        raw_chunks.push(tail.to_string());
    }
    if raw_chunks.is_empty() {
        return Ok(None);
    }

    let multi = raw_chunks.len() > 1;
    let mut out = Vec::with_capacity(raw_chunks.len());
    let mut has_special = false;
    for chunk in raw_chunks {
        if modorch_fedengine_pg_compat::parse_special_timezone_command(&chunk).is_some() {
            has_special = true;
            out.push(chunk);
        } else if let Ok(stmts) = Parser::parse_sql(&PostgreSqlDialect {}, &chunk)
            && stmts.len() == 1
        {
            match &stmts[0] {
                Statement::Query(_) | Statement::Set(_) | Statement::Reset(_) => {
                    out.push(stmts[0].to_string());
                }
                _ if multi => {
                    return Err(EngineError::unsupported(
                        "multi-statement messages containing writes, DDL or transaction commands are disabled",
                    ));
                }
                _ => return Ok(None),
            }
        } else {
            return Ok(None);
        }
    }
    if !has_special {
        return Ok(None);
    }
    if out.len() > 64 {
        return Err(EngineError::new("54000", "too many statements"));
    }
    Ok(Some(out))
}

impl Analyzer {
    pub fn parse(sql: &str) -> Result<Vec<Statement>> {
        if sql.len() > 1024 * 1024 {
            return Err(EngineError::new("54000", "SQL exceeds 1 MiB limit"));
        }
        Parser::parse_sql(&DuckDbDialect {}, sql).or_else(|_| {
            if let Ok(stmts) = Parser::parse_sql(&PostgreSqlDialect {}, sql)
                && matches!(stmts.as_slice(), [Statement::Truncate(_)])
            {
                return Ok(stmts);
            }
            Err(EngineError::new(
                "42601",
                "SQL syntax is outside the supported grammar",
            ))
        })
    }
    pub fn split(sql: &str) -> Result<Vec<String>> {
        if sql.len() > 1024 * 1024 {
            return Err(EngineError::new("54000", "SQL exceeds 1 MiB limit"));
        }
        let statements = match Self::parse(sql) {
            Ok(s) => s,
            Err(err) => {
                if let Ok(pg_stmts) = Parser::parse_sql(&PostgreSqlDialect {}, sql)
                    && !pg_stmts.is_empty()
                    && pg_stmts
                        .iter()
                        .any(|s| matches!(s, Statement::Set(_) | Statement::Reset(_)))
                    && pg_stmts.iter().all(|s| {
                        matches!(
                            s,
                            Statement::Query(_) | Statement::Set(_) | Statement::Reset(_)
                        )
                    })
                {
                    pg_stmts
                } else if let Some(split) = split_pg_fallback_statements(sql)? {
                    return Ok(split);
                } else if parse_reindex_index(sql)?.is_some() {
                    return Ok(vec![sql.trim().trim_end_matches(';').trim().to_string()]);
                } else {
                    return Err(err);
                }
            }
        };
        if statements.len() > 64 {
            return Err(EngineError::new("54000", "too many statements"));
        }
        if statements.len() > 1
            && statements.iter().any(|s| {
                !matches!(
                    s,
                    Statement::Query(_) | Statement::Set(_) | Statement::Reset(_)
                )
            })
        {
            return Err(EngineError::unsupported(
                "multi-statement messages containing writes, DDL or transaction commands are disabled",
            ));
        }
        Ok(statements.iter().map(ToString::to_string).collect())
    }
    pub fn analyze(&self, sql: &str) -> Result<Analysis> {
        let mut parsed = match Self::parse(sql) {
            Ok(p) => p,
            Err(err) => {
                if let Some(name) = parse_reindex_index(sql)? {
                    return self.analyze_reindex(&name);
                }
                return Err(err);
            }
        };
        if parsed.len() != 1 {
            return Err(EngineError::unsupported(
                "exactly one statement is required",
            ));
        }
        let mut statement = parsed.remove(0);
        let (kind, action, command) = classify(&statement)?;
        let mut binder = Binder::new(self);
        let mut primary = None;
        let mut index = None;
        let mut returning = None;
        match &mut statement {
            Statement::CreateSchema {
                schema_name,
                with,
                options,
                default_collate_spec,
                clone,
                ..
            } => {
                if with.is_some()
                    || options.is_some()
                    || default_collate_spec.is_some()
                    || clone.is_some()
                {
                    return Err(EngineError::unsupported("schema options are disabled"));
                }
                let SchemaName::Simple(name) = schema_name else {
                    return Err(EngineError::unsupported(
                        "schema authorization syntax is disabled",
                    ));
                };
                let object = self.bind(name, true)?;
                *name = object_name(&[&object.catalog, &object.schema]);
                binder.objects.insert(object.clone());
                primary = Some(object);
            }
            Statement::Drop {
                object_type,
                if_exists,
                names,
                cascade,
                purge,
                temporary,
                table,
                ..
            } => {
                if *cascade || *purge || *temporary || names.len() != 1 {
                    return Err(EngineError::unsupported(
                        "CASCADE, temporary and multiple-object DROP are disabled",
                    ));
                }
                let object = self.bind(&names[0], *object_type == ObjectType::Schema)?;
                if *object_type == ObjectType::Index {
                    let candidates: Vec<_> =
                        self.indexes.iter().filter(|i| i.index == object).collect();
                    if candidates.len() > 1 || (candidates.is_empty() && !*if_exists) {
                        return Err(EngineError::new(
                            "42704",
                            "index table ownership is unknown or ambiguous",
                        ));
                    }
                    if candidates.is_empty() {
                        let owner_table = if let Some(t) = table {
                            let bound = self.bind(t, false)?;
                            binder.objects.insert(bound.clone());
                            bound
                        } else {
                            ObjectRef {
                                source: object.source.clone(),
                                catalog: object.catalog.clone(),
                                schema: object.schema.clone(),
                                name: String::new(),
                            }
                        };
                        index = Some(IndexIntent {
                            table: owner_table.clone(),
                            name: object.name.clone(),
                            columns: vec![],
                            unique: false,
                            create: false,
                            if_exists: true,
                        });
                        primary = Some(if table.is_some() {
                            owner_table
                        } else {
                            object.clone()
                        });
                    } else {
                        let owner = candidates[0];
                        if owner.protected {
                            return Err(EngineError::denied(
                                "constraint/protected indexes cannot be dropped",
                            ));
                        }
                        if let Some(t) = table
                            && self.bind(t, false)? != owner.table
                        {
                            return Err(EngineError::denied("index and supplied table disagree"));
                        }
                        binder.objects.insert(owner.table.clone());
                        index = Some(IndexIntent {
                            table: owner.table.clone(),
                            name: object.name.clone(),
                            columns: vec![],
                            unique: false,
                            create: false,
                            if_exists: *if_exists,
                        });
                        primary = Some(owner.table.clone());
                    }
                } else {
                    primary = Some(object.clone());
                }
                names[0] = if *object_type == ObjectType::Schema {
                    object_name(&[&object.catalog, &object.schema])
                } else {
                    object_name(&[&object.catalog, &object.schema, &object.name])
                };
                binder.objects.insert(object);
            }
            Statement::CreateIndex(i) => {
                if i.concurrently
                    || !i.include.is_empty()
                    || i.nulls_distinct.is_some()
                    || !i.with.is_empty()
                    || i.predicate.is_some()
                    || !i.index_options.is_empty()
                    || !i.alter_options.is_empty()
                    || i.using
                        .as_ref()
                        .is_some_and(|v| v.to_string().to_lowercase() != "btree")
                {
                    return Err(EngineError::unsupported(
                        "only ordinary B-tree column indexes are allowed",
                    ));
                }
                let table = self.bind(&i.table_name, false)?;
                let name = i
                    .name
                    .as_ref()
                    .ok_or_else(|| EngineError::unsupported("index name is required"))?;
                let p = Self::parts(name)?;
                let index_name = match p.as_slice() {
                    [n] => n.clone(),
                    _ => {
                        let idx = self.bind(name, false)?;
                        binder.objects.insert(idx.clone());
                        if idx.schema != table.schema {
                            return Err(EngineError::unsupported(
                                "index must share table namespace",
                            ));
                        }
                        idx.name
                    }
                };
                let mut columns = Vec::new();
                for c in &i.columns {
                    let Expr::Identifier(id) = &c.column.expr else {
                        return Err(EngineError::unsupported("expression indexes are disabled"));
                    };
                    if c.operator_class.is_some()
                        || c.column.options.asc.is_some()
                        || c.column.options.nulls_first.is_some()
                    {
                        return Err(EngineError::unsupported(
                            "index sort/operator options are disabled",
                        ));
                    }
                    columns.push(id.value.clone());
                }
                if columns.is_empty() {
                    return Err(EngineError::unsupported("index requires columns"));
                }
                index = Some(IndexIntent {
                    table: table.clone(),
                    name: index_name,
                    columns,
                    unique: i.unique,
                    create: true,
                    if_exists: i.if_not_exists,
                });
                primary = Some(table);
            }
            Statement::CreateTable(t) => {
                // Reconstruct the permitted form; comparing ASTs catches every non-whitelisted option.
                let base = if t.query.is_some() {
                    "CREATE TABLE x AS SELECT 1"
                } else {
                    "CREATE TABLE x (id INTEGER)"
                };
                let Statement::CreateTable(mut permitted) = Self::parse(base)?.remove(0) else {
                    unreachable!()
                };
                permitted.name = t.name.clone();
                permitted.columns = t.columns.clone();
                permitted.constraints = t.constraints.clone();
                permitted.query = t.query.clone();
                permitted.if_not_exists = t.if_not_exists;
                if *t != permitted {
                    return Err(EngineError::unsupported(
                        "CREATE TABLE options are outside the whitelist",
                    ));
                }
                primary = Some(self.bind(&t.name, false)?);
                bind_constraints(&mut binder, &mut t.columns, &mut t.constraints)?;
            }
            Statement::AlterTable(t) => {
                primary = Some(self.bind(&t.name, false)?);
                if t.operations.len() != 1 || t.table_type.is_some() || t.location.is_some() {
                    return Err(EngineError::unsupported(
                        "one ordinary ALTER TABLE operation is required",
                    ));
                }
                match &mut t.operations[0] {
                    AlterTableOperation::AddColumn {
                        column_def,
                        column_position,
                        ..
                    } => {
                        if column_position.is_some() {
                            return Err(EngineError::unsupported("column position is disabled"));
                        }
                        bind_constraints(&mut binder, std::slice::from_mut(column_def), &mut [])?;
                    }
                    AlterTableOperation::DropColumn {
                        drop_behavior: None | Some(DropBehavior::Restrict),
                        ..
                    }
                    | AlterTableOperation::RenameColumn { .. }
                    | AlterTableOperation::RenameTable { .. } => {}
                    AlterTableOperation::AlterColumn { op, .. } => match op {
                        AlterColumnOperation::SetNotNull
                        | AlterColumnOperation::DropNotNull
                        | AlterColumnOperation::SetDefault { .. }
                        | AlterColumnOperation::DropDefault => {}
                        AlterColumnOperation::SetDataType {
                            data_type,
                            using: None,
                            ..
                        } => validate_scalar_type(data_type)?,
                        _ => {
                            return Err(EngineError::unsupported(
                                "ALTER COLUMN operation is not enabled",
                            ));
                        }
                    },
                    AlterTableOperation::AddConstraint {
                        constraint,
                        not_valid: false,
                    } => match constraint {
                        TableConstraint::PrimaryKey(_)
                        | TableConstraint::Unique(_)
                        | TableConstraint::Check(_) => {}
                        TableConstraint::ForeignKey(fk) => {
                            let target = self.bind(&fk.foreign_table, false)?;
                            if let Some(p) = &primary
                                && p.source != target.source
                            {
                                return Err(EngineError::unsupported(
                                    "cross-source foreign keys are disabled",
                                ));
                            }
                            fk.foreign_table =
                                object_name(&[&target.catalog, &target.schema, &target.name]);
                            binder.objects.insert(target);
                        }
                        _ => {
                            return Err(EngineError::unsupported(
                                "ALTER TABLE ADD CONSTRAINT variant is not enabled",
                            ));
                        }
                    },
                    AlterTableOperation::DropConstraint {
                        drop_behavior: None | Some(DropBehavior::Restrict),
                        ..
                    } => {}
                    // Other renames and schema migrations fail closed.
                    _ => return Err(EngineError::unsupported("ALTER operation is not enabled")),
                }
            }
            Statement::AlterIndex {
                name,
                operation: AlterIndexOperation::RenameIndex { .. },
            } => {
                let object = self.bind(name, false)?;
                let candidates: Vec<_> =
                    self.indexes.iter().filter(|i| i.index == object).collect();
                if candidates.len() != 1 {
                    return Err(EngineError::new(
                        "42704",
                        "index table ownership is unknown or ambiguous",
                    ));
                }
                let owner = candidates[0];
                if owner.protected {
                    return Err(EngineError::denied(
                        "constraint/protected indexes cannot be altered",
                    ));
                }
                binder.objects.insert(owner.table.clone());
                primary = Some(owner.table.clone());
                *name = object_name(&[&object.catalog, &object.schema, &object.name]);
                binder.objects.insert(object);
            }
            Statement::Truncate(tr) => {
                if tr.table_names.len() != 1
                    || tr.partitions.is_some()
                    || tr.on_cluster.is_some()
                    || matches!(tr.cascade, Some(CascadeOption::Cascade))
                {
                    return Err(EngineError::unsupported("TRUNCATE variant is not enabled"));
                }
                let target = self.bind(&tr.table_names[0].name, false)?;
                primary = Some(target);
            }
            Statement::Insert(i) => {
                if i.overwrite
                    || i.replace_into
                    || i.or.is_some()
                    || i.ignore
                    || i.partitioned.is_some()
                    || i.on.is_some()
                    || i.output.is_some()
                    || i.settings.is_some()
                    || i.format_clause.is_some()
                    || i.multi_table_insert_type.is_some()
                    || !i.multi_table_into_clauses.is_empty()
                {
                    return Err(EngineError::unsupported("INSERT variant is not enabled"));
                }
                returning = extract_returning(&i.returning)?;
                if let TableObject::TableName(name) = &i.table {
                    primary = Some(self.bind(name, false)?);
                }
            }
            Statement::Update(u) => {
                if !u.table.joins.is_empty() || u.output.is_some() || u.or.is_some() {
                    return Err(EngineError::unsupported("UPDATE variant is not enabled"));
                }
                returning = extract_returning(&u.returning)?;
                if let TableFactor::Table { name, .. } = &u.table.relation {
                    primary = Some(self.bind(name, false)?);
                }
            }
            Statement::Delete(d) => {
                if !d.tables.is_empty() || d.output.is_some() {
                    return Err(EngineError::unsupported("DELETE variant is not enabled"));
                }
                returning = extract_returning(&d.returning)?;
                let from = match &d.from {
                    FromTable::WithFromKeyword(v) | FromTable::WithoutKeyword(v) => v,
                };
                if from.len() == 1
                    && let TableFactor::Table { name, .. } = &from[0].relation
                {
                    primary = Some(self.bind(name, false)?);
                }
            }
            _ => {}
        }
        if let ControlFlow::Break(e) = VisitMut::visit(&mut statement, &mut binder) {
            return Err(e);
        }
        let sources: BTreeSet<_> = binder.objects.iter().map(|o| o.source.clone()).collect();
        if kind != Kind::Query && sources.len() != 1 {
            return Err(EngineError::denied(
                "write/DDL references must resolve to exactly one registered DataSourceId",
            ));
        }
        let capability = match &statement {
            Statement::AlterTable(t) => match &t.operations[0] {
                AlterTableOperation::AddColumn { .. } => "ALTER_TABLE_ADD_COLUMN",
                AlterTableOperation::DropColumn { .. } => "ALTER_TABLE_DROP_COLUMN",
                AlterTableOperation::RenameColumn { .. } => "ALTER_TABLE_RENAME_COLUMN",
                AlterTableOperation::RenameTable { .. } => "ALTER_TABLE_RENAME_TABLE",
                AlterTableOperation::AddConstraint { .. } => "ALTER_TABLE_ADD_CONSTRAINT",
                AlterTableOperation::DropConstraint { .. } => "ALTER_TABLE_DROP_CONSTRAINT",
                AlterTableOperation::AlterColumn { op, .. } => match op {
                    AlterColumnOperation::SetDataType { .. } => "ALTER_TABLE_COLUMN_TYPE",
                    AlterColumnOperation::SetDefault { .. } | AlterColumnOperation::DropDefault => {
                        "ALTER_TABLE_COLUMN_DEFAULT"
                    }
                    _ => "ALTER_TABLE_COLUMN_NULLABILITY",
                },
                _ => unreachable!(),
            },
            Statement::AlterIndex { .. } => "RENAME_INDEX",
            Statement::CreateIndex(i) if i.unique => "CREATE_INDEX_UNIQUE_BTREE",
            Statement::CreateIndex(_) => "CREATE_INDEX_BTREE",
            _ => action.as_str(),
        }
        .to_string();
        let ddl_intent = if kind == Kind::Ddl {
            crate::ddl_intent::extract_ddl_intent(&statement, primary.as_ref(), index.as_ref())?
        } else {
            None
        };
        Ok(Analysis {
            capability,
            sql: statement.to_string(),
            kind,
            action,
            command,
            objects: binder.objects,
            sources,
            primary,
            index,
            returning,
            ddl_intent,
        })
    }

    fn analyze_reindex(&self, name: &ObjectName) -> Result<Analysis> {
        let object = self.bind(name, false)?;
        let candidates: Vec<_> = self.indexes.iter().filter(|i| i.index == object).collect();
        if candidates.len() != 1 {
            return Err(EngineError::new(
                "42704",
                "index table ownership is unknown or ambiguous",
            ));
        }
        let owner = candidates[0];
        if owner.protected {
            return Err(EngineError::denied(
                "constraint/protected indexes cannot be reindexed",
            ));
        }
        let mut objects = BTreeSet::new();
        objects.insert(owner.table.clone());
        objects.insert(object.clone());
        let sources: BTreeSet<_> = objects.iter().map(|o| o.source.clone()).collect();
        let rewritten_name = object_name(&[&object.catalog, &object.schema, &object.name]);
        Ok(Analysis {
            capability: "REINDEX".into(),
            sql: format!("REINDEX INDEX {rewritten_name}"),
            kind: Kind::Ddl,
            action: "REINDEX".into(),
            command: "REINDEX".into(),
            objects,
            sources,
            primary: Some(owner.table.clone()),
            index: None,
            returning: None,
            ddl_intent: Some(DdlIntent {
                action: "reindex".into(),
                table_name: object.name.clone(),
                index_name: String::new(),
                owner: owner.table.name.clone(),
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
            }),
        })
    }
}

pub(crate) fn extract_returning(
    returning: &Option<Vec<SelectItem>>,
) -> Result<Option<modorch_fedengine_contract::ReturningPlan>> {
    let Some(items) = returning else {
        return Ok(None);
    };
    if items.len() != 1 {
        return Err(EngineError::unsupported(
            "RETURNING only supports a single identifier column",
        ));
    }
    let SelectItem::UnnamedExpr(Expr::Identifier(col)) = &items[0] else {
        return Err(EngineError::unsupported(
            "RETURNING only supports a single identifier column",
        ));
    };
    Ok(Some(modorch_fedengine_contract::ReturningPlan {
        column: col.value.clone(),
    }))
}

pub(crate) fn parse_reindex_index(sql: &str) -> Result<Option<ObjectName>> {
    let tokens: Vec<Token> = Tokenizer::new(&DuckDbDialect {}, sql)
        .tokenize()
        .map_err(|_| EngineError::new("42601", "SQL syntax is outside the supported grammar"))?
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let Some(Token::Word(first)) = tokens.first() else {
        return Ok(None);
    };
    if first.quote_style.is_some() || !first.value.eq_ignore_ascii_case("REINDEX") {
        return Ok(None);
    }
    let Some(Token::Word(kind)) = tokens.get(1) else {
        return Err(EngineError::unsupported(
            "only ordinary REINDEX INDEX <name> is enabled",
        ));
    };
    if kind.quote_style.is_some() || !kind.value.eq_ignore_ascii_case("INDEX") {
        return Err(EngineError::unsupported(
            "only ordinary REINDEX INDEX <name> is enabled",
        ));
    }
    let mut idx = 2usize;
    let mut parts = Vec::new();
    loop {
        let Some(Token::Word(w)) = tokens.get(idx) else {
            return Err(EngineError::unsupported(
                "REINDEX target must be a 1..3-part index identifier",
            ));
        };
        if parts.is_empty()
            && w.quote_style.is_none()
            && w.value.eq_ignore_ascii_case("CONCURRENTLY")
        {
            return Err(EngineError::unsupported("REINDEX CONCURRENTLY is disabled"));
        }
        let ident = match w.quote_style {
            Some(q) => Ident::with_quote(q, &w.value),
            None => Ident::new(&w.value),
        };
        parts.push(ObjectNamePart::Identifier(ident));
        idx += 1;
        if matches!(tokens.get(idx), Some(Token::Period)) {
            idx += 1;
            continue;
        }
        break;
    }
    if parts.is_empty() || parts.len() > 3 {
        return Err(EngineError::unsupported(
            "REINDEX target must be a 1..3-part index identifier",
        ));
    }
    if matches!(tokens.get(idx), Some(Token::SemiColon)) {
        idx += 1;
    }
    if idx != tokens.len() {
        return Err(EngineError::unsupported(
            "REINDEX options or trailing tokens are disabled",
        ));
    }
    Ok(Some(ObjectName(parts)))
}
pub(super) fn classify(s: &Statement) -> Result<(Kind, String, String)> {
    let (k, a, c) = match s {
        Statement::Query(_) => (Kind::Query, "SELECT", "SELECT"),
        Statement::Insert(_) => (Kind::Insert, "INSERT", "INSERT"),
        Statement::Update(_) => (Kind::Update, "UPDATE", "UPDATE"),
        Statement::Delete(_) => (Kind::Delete, "DELETE", "DELETE"),
        Statement::CreateTable(t) => (
            Kind::Ddl,
            if t.query.is_some() {
                "CREATE_TABLE_AS"
            } else {
                "CREATE_TABLE"
            },
            "CREATE TABLE",
        ),
        Statement::CreateSchema { .. } => (Kind::Ddl, "CREATE_SCHEMA", "CREATE SCHEMA"),
        Statement::CreateIndex(_) => (Kind::Ddl, "CREATE_INDEX", "CREATE INDEX"),
        Statement::AlterIndex { .. } => (Kind::Ddl, "ALTER_INDEX", "ALTER INDEX"),
        Statement::AlterTable(_) => (Kind::Ddl, "ALTER_TABLE", "ALTER TABLE"),
        Statement::Truncate(_) => (Kind::Ddl, "TRUNCATE", "TRUNCATE TABLE"),
        Statement::Drop { object_type, .. } => match object_type {
            ObjectType::Schema => (Kind::Ddl, "DROP_SCHEMA", "DROP SCHEMA"),
            ObjectType::Table => (Kind::Ddl, "DROP_TABLE", "DROP TABLE"),
            ObjectType::Index => (Kind::Ddl, "DROP_INDEX", "DROP INDEX"),
            _ => return Err(EngineError::unsupported("object type is not enabled")),
        },
        _ => {
            return Err(EngineError::unsupported(
                "statement class is not enabled; explicit transactions and engine administration are disabled",
            ));
        }
    };
    Ok((k, a.into(), c.into()))
}
