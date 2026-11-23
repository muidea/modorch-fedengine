use crate::{Analyzer, ObjectRef, analysis::classify};
use modorch_fedengine_core::*;
use sqlparser::ast::*;
use std::{collections::BTreeSet, ops::ControlFlow};
impl Analyzer {
    pub(super) fn parts(name: &ObjectName) -> Result<Vec<String>> {
        name.0
            .iter()
            .map(|p| match p {
                ObjectNamePart::Identifier(i) => Ok(if i.quote_style.is_some() {
                    i.value.clone()
                } else {
                    i.value.to_lowercase()
                }),
                _ => Err(EngineError::unsupported(
                    "computed object names are disabled",
                )),
            })
            .collect()
    }
    pub fn bind(&self, name: &ObjectName, schema_object: bool) -> Result<ObjectRef> {
        let p = Self::parts(name)?;
        let enabled: Vec<_> = self.sources.iter().filter(|s| s.enabled).collect();
        if let Some(ns) = &self.namespace {
            let local = match p.as_slice() {
                [catalog, schema, object] if !schema_object && catalog == &ns.catalog => {
                    Some((schema.clone(), object.clone()))
                }
                [catalog, schema, _] if !schema_object && catalog != &ns.catalog => {
                    if !ns.allow_cross_database {
                        return Err(EngineError::unsupported(
                            "cross-database references are not implemented; use schemas in the current database",
                        ));
                    }
                    let _ = schema;
                    None
                }
                [schema, object] if !schema_object => Some((schema.clone(), object.clone())),
                [object] if !schema_object => {
                    let schema = ns
                        .search_path
                        .iter()
                        .find(|s| ns.relations.contains(&((*s).clone(), object.clone())))
                        .or_else(|| ns.search_path.first())
                        .cloned()
                        .ok_or_else(|| {
                            EngineError::new(
                                "42P01",
                                "relation does not exist in the effective search_path",
                            )
                        })?;
                    Some((schema, object.clone()))
                }
                [schema] if schema_object => Some((schema.clone(), schema.clone())),
                [catalog, schema] if schema_object && catalog == &ns.catalog => {
                    Some((schema.clone(), schema.clone()))
                }
                [_, _] if schema_object && !ns.allow_cross_database => {
                    return Err(EngineError::unsupported(
                        "cross-database references are not implemented",
                    ));
                }
                _ => None,
            };
            if let Some((schema, object)) = local {
                let route = if !schema_object {
                    ns.routes
                        .as_ref()
                        .map(|routes| routes.get(&(schema.clone(), object.clone())))
                } else {
                    None
                };
                let (id, remote_schema) = route
                    .unwrap_or_else(|| ns.bindings.get(&schema))
                    .ok_or_else(|| {
                        EngineError::new(
                            "42P01",
                            "relation is outside the available database schemas",
                        )
                    })?;
                let source = enabled
                    .iter()
                    .find(|s| &s.id == id)
                    .ok_or_else(|| EngineError::new("55000", "source is unavailable"))?;
                if !source.allowed_schemas.contains(remote_schema)
                    || !allowed_namespace(remote_schema)
                {
                    return Err(EngineError::denied(
                        "namespace is outside the data source allowlist",
                    ));
                }
                return Ok(ObjectRef {
                    source: id.clone(),
                    catalog: source.catalog_alias.clone(),
                    schema: remote_schema.clone(),
                    name: object,
                });
            }
        }

        let default = || {
            if let Some(namespace) = &self.namespace {
                return enabled
                    .iter()
                    .find(|s| s.catalog_alias == namespace.catalog)
                    .copied()
                    .ok_or_else(|| EngineError::new("3D000", "current catalog is unavailable"));
            }
            if enabled.len() == 1 {
                Ok(enabled[0])
            } else {
                Err(EngineError::new(
                    "3D000",
                    "object requires an unambiguous registered catalog",
                ))
            }
        };
        let (source, schema, object) = if schema_object {
            match p.as_slice() {
                [catalog, schema] => (
                    enabled
                        .iter()
                        .find(|s| s.catalog_alias == *catalog)
                        .copied()
                        .ok_or_else(|| EngineError::new("3D000", "unknown catalog"))?,
                    schema.clone(),
                    schema.clone(),
                ),
                [schema] => (default()?, schema.clone(), schema.clone()),
                _ => return Err(EngineError::unsupported("schema must be catalog.schema")),
            }
        } else {
            match p.as_slice() {
                [catalog, schema, object] => (
                    enabled
                        .iter()
                        .find(|s| s.catalog_alias == *catalog)
                        .copied()
                        .ok_or_else(|| EngineError::new("3D000", "unknown catalog"))?,
                    schema.clone(),
                    object.clone(),
                ),
                [first, object] => {
                    if self.namespace.is_some() {
                        (default()?, first.clone(), object.clone())
                    } else if let Some(s) = enabled.iter().find(|s| s.catalog_alias == *first) {
                        (*s, s.default_schema.clone(), object.clone())
                    } else {
                        (default()?, first.clone(), object.clone())
                    }
                }
                [object] => {
                    let s = default()?;
                    let schema = if let Some(ns) = &self.namespace {
                        ns.search_path
                            .iter()
                            .find(|schema| {
                                ns.relations.contains(&((*schema).clone(), object.clone()))
                            })
                            .or_else(|| ns.search_path.first())
                            .cloned()
                            .ok_or_else(|| {
                                EngineError::new("3F000", "no schema in the effective search_path")
                            })?
                    } else {
                        s.default_schema.clone()
                    };
                    (s, schema, object.clone())
                }
                _ => return Err(EngineError::unsupported("invalid object qualification")),
            }
        };
        if !source.allowed_schemas.contains(&schema)
            || ["pg_catalog", "information_schema", "sys", "mysql"]
                .contains(&schema.to_ascii_lowercase().as_str())
        {
            return Err(EngineError::denied(
                "namespace is outside the data source allowlist",
            ));
        }
        Ok(ObjectRef {
            source: source.id.clone(),
            catalog: source.catalog_alias.clone(),
            schema,
            name: object,
        })
    }
}
pub(super) fn object_name(parts: &[&str]) -> ObjectName {
    ObjectName(
        parts
            .iter()
            .map(|p| ObjectNamePart::Identifier(Ident::with_quote('"', *p)))
            .collect(),
    )
}
pub(super) struct Binder<'a> {
    analyzer: &'a Analyzer,
    pub(super) objects: BTreeSet<ObjectRef>,
    ctes: Vec<BTreeSet<String>>,
    aliases: Vec<Vec<(Vec<String>, String)>>,
    alias_counter: usize,
    builtin_relation: Option<ObjectName>,
    depth: usize,
}
impl<'a> Binder<'a> {
    pub(super) fn new(analyzer: &'a Analyzer) -> Self {
        Self {
            analyzer,
            objects: BTreeSet::new(),
            ctes: Vec::new(),
            aliases: Vec::new(),
            alias_counter: 0,
            builtin_relation: None,
            depth: 0,
        }
    }

    fn qualifier(&self, parts: &[String]) -> Result<Option<String>> {
        for scope in self.aliases.iter().rev() {
            let matches = scope
                .iter()
                .filter(|(key, _)| key == parts)
                .map(|(_, alias)| alias)
                .collect::<BTreeSet<_>>();
            if matches.len() > 1 {
                return Err(EngineError::new("42702", "table reference is ambiguous"));
            }
            if let Some(alias) = matches.first() {
                return Ok(Some((*alias).clone()));
            }
        }
        Ok(None)
    }
    fn aliases(&mut self, select: &mut Select) -> Result<()> {
        fn tables<'b>(table: &'b mut TableFactor, out: &mut Vec<&'b mut TableFactor>) {
            if let TableFactor::NestedJoin {
                table_with_joins, ..
            } = table
            {
                tables(&mut table_with_joins.relation, out);
                for join in &mut table_with_joins.joins {
                    tables(&mut join.relation, out);
                }
            } else {
                out.push(table);
            }
        }
        let mut factors = vec![];
        for from in &mut select.from {
            tables(&mut from.relation, &mut factors);
            for join in &mut from.joins {
                tables(&mut join.relation, &mut factors);
            }
        }
        let mut taken = BTreeSet::new();
        for factor in &factors {
            match factor {
                TableFactor::Table {
                    alias: Some(alias), ..
                }
                | TableFactor::Derived {
                    alias: Some(alias), ..
                } => {
                    taken.insert(if alias.name.quote_style.is_some() {
                        alias.name.value.clone()
                    } else {
                        alias.name.value.to_lowercase()
                    });
                }
                _ => {}
            }
        }
        let mut scope = vec![];
        for factor in factors {
            let explicit = match &*factor {
                TableFactor::Table {
                    alias: Some(alias), ..
                }
                | TableFactor::Derived {
                    alias: Some(alias), ..
                } => Some(alias),
                _ => None,
            };
            if let Some(alias) = explicit {
                let name = if alias.name.quote_style.is_some() {
                    alias.name.value.clone()
                } else {
                    alias.name.value.to_lowercase()
                };
                scope.push((vec![name.clone()], name));
                continue;
            }
            let TableFactor::Table {
                name,
                alias,
                args: None,
                ..
            } = factor
            else {
                continue;
            };
            let parts = Analyzer::parts(name)?;
            if parts.len() == 1 && self.ctes.iter().rev().any(|s| s.contains(&parts[0])) {
                scope.push((parts.clone(), parts[0].clone()));
                continue;
            }
            let object = self.analyzer.bind(name, false)?;
            let generated = loop {
                self.alias_counter += 1;
                let candidate = format!("__fed_binding_{}", self.alias_counter);
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
            };
            scope.push((parts.clone(), generated.clone()));
            scope.push((vec![parts.last().unwrap().clone()], generated.clone()));
            if parts.len() >= 2 {
                scope.push((parts[parts.len() - 2..].to_vec(), generated.clone()));
                if parts.len() == 2
                    && let Some(ns) = &self.analyzer.namespace
                {
                    scope.push((
                        vec![ns.catalog.clone(), parts[0].clone(), parts[1].clone()],
                        generated.clone(),
                    ));
                }
            } else if let Some(ns) = &self.analyzer.namespace
                && let Some(schema) = ns.search_path.iter().find(|schema| {
                    ns.bindings.get(*schema).is_some_and(|(source, remote)| {
                        source == &object.source && remote == &object.schema
                    })
                })
            {
                scope.push((vec![schema.clone(), object.name.clone()], generated.clone()));
                scope.push((
                    vec![ns.catalog.clone(), schema.clone(), object.name.clone()],
                    generated.clone(),
                ));
            }
            *alias = Some(TableAlias {
                name: Ident::with_quote('"', generated),
                columns: vec![],
                explicit: true,
                at: None,
            });
        }
        if let Some(current) = self.aliases.last_mut() {
            *current = scope;
        }
        for item in &mut select.projection {
            if let SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(name),
                _,
            ) = item
            {
                let parts = Analyzer::parts(name)?;
                if let Some(alias) = self.qualifier(&parts)? {
                    *name = object_name(&[&alias]);
                } else if parts.len() > 1 {
                    return Err(EngineError::new(
                        "42P01",
                        "missing FROM-clause entry for qualified wildcard",
                    ));
                }
            }
        }
        Ok(())
    }
    fn relation(&mut self, n: &mut ObjectName) -> Result<()> {
        let p = Analyzer::parts(n)?;
        if p.len() == 1 && self.ctes.iter().rev().any(|s| s.contains(&p[0])) {
            return Ok(());
        }
        // Built-in bounded generators are table functions, handled separately by pre_visit_table_factor.
        if self.builtin_relation.as_ref() == Some(n) {
            self.builtin_relation.take();
            return Ok(());
        }
        let o = self.analyzer.bind(n, false)?;
        *n = object_name(&[&o.catalog, &o.schema, &o.name]);
        self.objects.insert(o);
        Ok(())
    }
}
impl VisitorMut for Binder<'_> {
    type Break = EngineError;
    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<EngineError> {
        if !q.locks.is_empty() {
            return ControlFlow::Break(EngineError::unsupported("locking reads are disabled"));
        }
        let mut scope = BTreeSet::new();
        if let Some(w) = &q.with {
            for c in &w.cte_tables {
                scope.insert(c.alias.name.value.to_lowercase());
            }
        }
        self.ctes.push(scope);
        self.aliases.push(vec![]);
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<EngineError> {
        self.ctes.pop();
        self.aliases.pop();
        ControlFlow::Continue(())
    }
    fn pre_visit_select(&mut self, s: &mut Select) -> ControlFlow<EngineError> {
        if s.into.is_some() {
            return ControlFlow::Break(EngineError::unsupported("SELECT INTO is disabled"));
        }
        if self.analyzer.namespace.is_some()
            && let Err(e) = self.aliases(s)
        {
            return ControlFlow::Break(e);
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_statement(&mut self, s: &mut Statement) -> ControlFlow<EngineError> {
        if self.depth > 0 && !matches!(s, Statement::Query(_)) {
            return ControlFlow::Break(EngineError::unsupported(
                "nested write statements are disabled",
            ));
        }
        self.depth += 1;
        match classify(s) {
            Ok(_) => ControlFlow::Continue(()),
            Err(e) => ControlFlow::Break(e),
        }
    }
    fn post_visit_statement(&mut self, _: &mut Statement) -> ControlFlow<EngineError> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
    fn pre_visit_relation(&mut self, n: &mut ObjectName) -> ControlFlow<EngineError> {
        match self.relation(n) {
            Ok(()) => ControlFlow::Continue(()),
            Err(e) => ControlFlow::Break(e),
        }
    }
    fn pre_visit_table_factor(&mut self, t: &mut TableFactor) -> ControlFlow<EngineError> {
        match t {
            TableFactor::Table {
                name,
                args: Some(_),
                ..
            } => {
                self.builtin_relation = Some(name.clone());
                let name = name.to_string().to_lowercase();
                if !["range", "generate_series"].contains(&name.as_str()) {
                    return ControlFlow::Break(EngineError::denied(
                        "table function is not allowed",
                    ));
                }
            }
            TableFactor::Table { .. }
            | TableFactor::Derived { .. }
            | TableFactor::NestedJoin { .. } => {}
            _ => {
                return ControlFlow::Break(EngineError::unsupported(
                    "table factor cannot be proven safe",
                ));
            }
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<EngineError> {
        if let Expr::Cast { data_type, .. } = e {
            if let Err(err) = validate_cast_type(data_type) {
                return ControlFlow::Break(err);
            }
            unqualify_pg_catalog_json(data_type);
        }
        if self.analyzer.namespace.is_some()
            && let Expr::CompoundIdentifier(parts) = e
        {
            let prefix = ObjectName(
                parts[..parts.len() - 1]
                    .iter()
                    .cloned()
                    .map(ObjectNamePart::Identifier)
                    .collect(),
            );
            let prefix = match Analyzer::parts(&prefix) {
                Ok(p) => p,
                Err(e) => return ControlFlow::Break(e),
            };
            match self.qualifier(&prefix) {
                Ok(Some(alias)) => {
                    let column = parts.last().unwrap().clone();
                    *parts = vec![Ident::with_quote('"', alias), column];
                }
                Ok(None) => {}
                Err(e) => return ControlFlow::Break(e),
            }
        }
        if let Expr::Function(f) = e {
            let name = f.name.to_string().to_lowercase();
            const ALLOWED: &[&str] = &[
                "decode",
                "count",
                "sum",
                "avg",
                "min",
                "max",
                "abs",
                "round",
                "floor",
                "ceil",
                "ceiling",
                "lower",
                "upper",
                "length",
                "char_length",
                "substring",
                "substr",
                "trim",
                "ltrim",
                "rtrim",
                "concat",
                "concat_ws",
                "coalesce",
                "nullif",
                "greatest",
                "least",
                "date_trunc",
                "date_part",
                "extract",
                "now",
                "current_timestamp",
                "current_date",
                "row_number",
                "rank",
                "dense_rank",
                "lag",
                "lead",
                "first_value",
                "last_value",
                "string_agg",
                "bool_and",
                "bool_or",
                "stddev",
                "variance",
                "sqrt",
                "power",
                "mod",
                "replace",
                "regexp_matches",
                "regexp_replace",
                "version",
                "current_database",
                "current_schema",
            ];
            if !ALLOWED.contains(&name.as_str()) {
                return ControlFlow::Break(EngineError::denied(
                    "function is not in the safe function whitelist",
                ));
            }
        }
        ControlFlow::Continue(())
    }
}
pub(super) fn bind_constraints(
    b: &mut Binder<'_>,
    columns: &mut [ColumnDef],
    constraints: &mut [TableConstraint],
) -> Result<()> {
    for c in columns {
        validate_scalar_type(&c.data_type)?;
        for option in &mut c.options {
            match &mut option.option {
                ColumnOption::ForeignKey(f) => b.relation(&mut f.foreign_table)?,
                ColumnOption::Null
                | ColumnOption::NotNull
                | ColumnOption::Default(_)
                | ColumnOption::PrimaryKey(_)
                | ColumnOption::Unique(_)
                | ColumnOption::Check(_) => {}
                _ => return Err(EngineError::unsupported("column option is not enabled")),
            }
        }
    }
    for c in constraints {
        match c {
            TableConstraint::ForeignKey(f) => b.relation(&mut f.foreign_table)?,
            TableConstraint::PrimaryKey(_)
            | TableConstraint::Unique(_)
            | TableConstraint::Check(_) => {}
            _ => return Err(EngineError::unsupported("table constraint is not enabled")),
        }
    }
    Ok(())
}
pub(super) fn validate_scalar_type(ty: &DataType) -> Result<()> {
    let name = ty.to_string().to_uppercase();
    let base = name.split('(').next().unwrap().trim();
    if ![
        "BOOLEAN",
        "BOOL",
        "SMALLINT",
        "INT2",
        "INTEGER",
        "INT",
        "INT4",
        "BIGINT",
        "INT8",
        "REAL",
        "FLOAT",
        "DOUBLE",
        "DOUBLE PRECISION",
        "DECIMAL",
        "NUMERIC",
        "VARCHAR",
        "CHAR",
        "CHARACTER",
        "CHARACTER VARYING",
        "TEXT",
        "BLOB",
        "BYTEA",
        "DATE",
        "TIME",
        "TIMESTAMP",
        "TIMESTAMPTZ",
        "UUID",
    ]
    .contains(&base)
    {
        return Err(EngineError::unsupported(
            "only built-in scalar column types are enabled",
        ));
    }
    Ok(())
}
fn validate_decimal_info(info: &ExactNumberInfo) -> Result<()> {
    match info {
        ExactNumberInfo::None => Ok(()),
        ExactNumberInfo::Precision(p) => {
            if *p == 0 || *p > 38 {
                Err(EngineError::unsupported(
                    "DECIMAL precision is outside DuckDB DECIMAL(38) range",
                ))
            } else {
                Ok(())
            }
        }
        ExactNumberInfo::PrecisionAndScale(p, s) => {
            if *p == 0 || *p > 38 {
                Err(EngineError::unsupported(
                    "DECIMAL precision is outside DuckDB DECIMAL(38) range",
                ))
            } else if *s < 0 || *s > *p as i64 {
                Err(EngineError::new(
                    "22023",
                    "DECIMAL scale must be between 0 and precision",
                ))
            } else {
                Ok(())
            }
        }
    }
}
fn unqualify_pg_catalog_json(ty: &mut DataType) {
    match ty {
        DataType::Custom(name, modifiers)
            if modifiers.is_empty()
                && Analyzer::parts(name).is_ok_and(|parts| {
                    parts.len() == 2 && parts[0] == "pg_catalog" && parts[1] == "json"
                }) =>
        {
            *ty = DataType::JSON;
        }
        DataType::Array(ArrayElemTypeDef::AngleBracket(inner))
        | DataType::Array(ArrayElemTypeDef::SquareBracket(inner, _)) => {
            unqualify_pg_catalog_json(inner);
        }
        _ => {}
    }
}
pub(super) fn validate_cast_type(ty: &DataType) -> Result<()> {
    match ty {
        DataType::JSONB => {
            return Err(EngineError::unsupported(
                "JSONB encoding is not implemented",
            ));
        }
        DataType::Numeric(info) | DataType::Decimal(info) => return validate_decimal_info(info),
        DataType::Array(ArrayElemTypeDef::AngleBracket(inner))
        | DataType::Array(ArrayElemTypeDef::SquareBracket(inner, _)) => {
            if matches!(inner.as_ref(), DataType::Array(_)) {
                return Err(EngineError::unsupported(
                    "multi-dimensional arrays are not implemented",
                ));
            }
            return validate_cast_type(inner);
        }
        DataType::Custom(name, modifiers) => {
            let parts = Analyzer::parts(name)?;
            let raw = name.to_string().to_ascii_lowercase();
            let base = raw.split('(').next().unwrap().trim();
            if parts.len() == 2 && parts[0] == "pg_catalog" {
                let pg_name = parts[1].as_str();
                if matches!(
                    pg_name,
                    "integer"
                        | "bigint"
                        | "boolean"
                        | "float"
                        | "double"
                        | "double precision"
                        | "decimal"
                ) {
                    return Err(EngineError::new(
                        "42704",
                        "type does not exist in pg_catalog",
                    ));
                }
                if pg_name == "jsonb" {
                    return Err(EngineError::unsupported(
                        "JSONB encoding is not implemented",
                    ));
                }
                if matches!(
                    pg_name,
                    "bool"
                        | "int2"
                        | "int4"
                        | "int8"
                        | "float4"
                        | "float8"
                        | "numeric"
                        | "varchar"
                        | "bpchar"
                        | "text"
                        | "bytea"
                        | "date"
                        | "time"
                        | "timestamp"
                        | "timestamptz"
                        | "interval"
                        | "uuid"
                        | "json"
                        | "point"
                ) {
                    return Ok(());
                }
                return Err(EngineError::new(
                    "42704",
                    "type does not exist in pg_catalog",
                ));
            }
            if base == "jsonb" {
                return Err(EngineError::unsupported(
                    "JSONB encoding is not implemented",
                ));
            }
            if matches!(
                base,
                "money"
                    | "inet"
                    | "cidr"
                    | "macaddr"
                    | "macaddr8"
                    | "xml"
                    | "bit"
                    | "varbit"
                    | "timetz"
                    | "tsvector"
                    | "tsquery"
                    | "line"
                    | "lseg"
                    | "box"
                    | "path"
                    | "polygon"
                    | "circle"
            ) {
                return Err(EngineError::unsupported(
                    "cast target type is not supported",
                ));
            }
            if matches!(
                base,
                "int1"
                    | "tinyint"
                    | "utinyint"
                    | "usmallint"
                    | "uinteger"
                    | "ubigint"
                    | "hugeint"
                    | "uhugeint"
                    | "int2"
                    | "int4"
                    | "int8"
                    | "float4"
                    | "float8"
                    | "bpchar"
                    | "timestamptz"
                    | "point"
            ) && modifiers.is_empty()
            {
                return Ok(());
            }
            if parts.len() > 1 {
                return Err(EngineError::new("42704", "cast target type does not exist"));
            }
            return Err(EngineError::new(
                "42704",
                "cast target type is not a supported built-in type",
            ));
        }
        _ => {}
    }
    let name = ty.to_string().to_uppercase();
    let base = name.split('(').next().unwrap().trim();
    if [
        "BOOLEAN",
        "BOOL",
        "TINYINT",
        "INT1",
        "SMALLINT",
        "INT2",
        "INTEGER",
        "INT",
        "INT4",
        "BIGINT",
        "INT8",
        "UTINYINT",
        "USMALLINT",
        "UINTEGER",
        "UBIGINT",
        "HUGEINT",
        "UHUGEINT",
        "REAL",
        "FLOAT",
        "FLOAT4",
        "DOUBLE",
        "DOUBLE PRECISION",
        "FLOAT8",
        "DECIMAL",
        "NUMERIC",
        "VARCHAR",
        "CHAR",
        "CHARACTER",
        "CHARACTER VARYING",
        "BPCHAR",
        "TEXT",
        "BLOB",
        "BYTEA",
        "DATE",
        "TIME",
        "TIMESTAMP",
        "TIMESTAMP WITHOUT TIME ZONE",
        "TIMESTAMPTZ",
        "TIMESTAMP WITH TIME ZONE",
        "INTERVAL",
        "UUID",
        "JSON",
        "POINT",
    ]
    .contains(&base)
    {
        return Ok(());
    }
    Err(EngineError::unsupported(
        "cast target type is not supported",
    ))
}
