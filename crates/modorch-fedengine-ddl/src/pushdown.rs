//! Controlled post-binding read pushdown and dialect normalization.
//! Runs only after Analyzer binding, Policy authorization and source read fences.
use crate::Analyzer;
use modorch_fedengine_core::*;
use sqlparser::ast::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
};

pub fn rewrite(
    sql: &str,
    sources: &BTreeSet<DataSourceId>,
    definitions: &[DataSourceDefinition],
) -> Result<String> {
    let mut statements = Analyzer::parse(sql)?;
    if statements.len() != 1 {
        return Ok(sql.to_string());
    }
    let Statement::Query(query) = &mut statements[0] else {
        return Ok(sql.to_string());
    };
    let has_param_or_decode = visit_expressions(query.as_ref(), |expr| {
        if let Expr::Value(v) = expr
            && matches!(v.value, Value::Placeholder(_))
        {
            return ControlFlow::Break(());
        }
        if let Expr::Function(f) = expr
            && f.name.to_string().eq_ignore_ascii_case("decode")
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
    .is_break();
    if !has_param_or_decode
        && sources.len() == 1
        && let Some(id) = sources.iter().next()
        && let Some(source) = definitions.iter().find(|d| &d.id == id)
        && (source.kind.is_mysql_family()
            || matches!(
                source.kind,
                DataSourceKind::Greenplum | DataSourceKind::Kingbasees
            ))
        && let Some(rewritten) = try_single_source_pushdown(query, source)?
    {
        return Ok(rewritten);
    }
    if sources.iter().any(|id| {
        definitions
            .iter()
            .find(|d| &d.id == id)
            .is_some_and(|d| d.kind == DataSourceKind::Greenplum)
    }) {
        rewrite_greenplum_cross_scans(query, definitions)?;
        return Ok(statements.remove(0).to_string());
    }
    Ok(sql.to_string())
}

fn ident_text(ident: &Ident) -> String {
    ident.value.clone()
}

fn object_parts(name: &ObjectName) -> Option<Vec<String>> {
    name.0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => Some(ident_text(i)),
            _ => None,
        })
        .collect()
}

fn parse_table_factor(sql: &str) -> Result<TableFactor> {
    let mut parsed = Analyzer::parse(sql)?;
    let Statement::Query(q) = parsed.remove(0) else {
        return Err(EngineError::new("XX000", "invalid pushdown table template"));
    };
    let SetExpr::Select(s) = *q.body else {
        return Err(EngineError::new("XX000", "invalid pushdown table template"));
    };
    Ok(s.from.into_iter().next().unwrap().relation)
}

fn rewrite_greenplum_cross_scans(
    query: &mut Query,
    definitions: &[DataSourceDefinition],
) -> Result<()> {
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            rewrite_greenplum_cross_scans(&mut cte.query, definitions)?;
        }
    }
    match query.body.as_mut() {
        SetExpr::Select(select) => rewrite_greenplum_select(select, definitions),
        SetExpr::Query(inner) => rewrite_greenplum_cross_scans(inner, definitions),
        SetExpr::SetOperation { left, right, .. } => {
            rewrite_greenplum_set_expr(left, definitions)?;
            rewrite_greenplum_set_expr(right, definitions)
        }
        _ => Ok(()),
    }
}

fn rewrite_greenplum_set_expr(
    set_expr: &mut SetExpr,
    definitions: &[DataSourceDefinition],
) -> Result<()> {
    match set_expr {
        SetExpr::Select(select) => rewrite_greenplum_select(select, definitions),
        SetExpr::Query(inner) => rewrite_greenplum_cross_scans(inner, definitions),
        SetExpr::SetOperation { left, right, .. } => {
            rewrite_greenplum_set_expr(left, definitions)?;
            rewrite_greenplum_set_expr(right, definitions)
        }
        _ => Ok(()),
    }
}

struct GreenplumBinding {
    catalog: String,
    schema: String,
    table: String,
    alias: String,
}

fn rewrite_greenplum_select(
    select: &mut Select,
    definitions: &[DataSourceDefinition],
) -> Result<()> {
    let mut bindings = Vec::new();
    for item in &mut select.from {
        rewrite_greenplum_table_factor(&mut item.relation, definitions, &mut bindings)?;
        for join in &mut item.joins {
            rewrite_greenplum_table_factor(&mut join.relation, definitions, &mut bindings)?;
        }
    }
    if bindings.is_empty() {
        return Ok(());
    }
    let _ = visit_expressions_mut(select, |expr| {
        if let Expr::CompoundIdentifier(parts) = expr
            && parts.len() == 4
        {
            for b in &bindings {
                if parts[0].value.eq_ignore_ascii_case(&b.catalog)
                    && parts[1].value.eq_ignore_ascii_case(&b.schema)
                    && parts[2].value.eq_ignore_ascii_case(&b.table)
                {
                    let col = parts.pop().unwrap();
                    *parts = vec![Ident::with_quote('"', &b.alias), col];
                    break;
                }
            }
        }
        ControlFlow::<()>::Continue(())
    });
    Ok(())
}

fn rewrite_greenplum_table_factor(
    factor: &mut TableFactor,
    definitions: &[DataSourceDefinition],
    bindings: &mut Vec<GreenplumBinding>,
) -> Result<()> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => {
            if let Some(parts) = object_parts(name)
                && parts.len() == 3
                && definitions.iter().any(|d| {
                    d.kind == DataSourceKind::Greenplum
                        && d.catalog_alias.eq_ignore_ascii_case(&parts[0])
                })
            {
                let effective_alias = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| parts[2].clone());
                let remote_sql = format!(
                    "SELECT * FROM {}.{}",
                    quote_identifier(&parts[1]),
                    quote_identifier(&parts[2])
                );
                let replacement = format!(
                    "SELECT * FROM postgres_query({}, {}) AS {}",
                    quote_literal(&parts[0]),
                    quote_literal(&remote_sql),
                    quote_identifier(&effective_alias)
                );
                bindings.push(GreenplumBinding {
                    catalog: parts[0].clone(),
                    schema: parts[1].clone(),
                    table: parts[2].clone(),
                    alias: effective_alias,
                });
                *factor = parse_table_factor(&replacement)?;
            }
            Ok(())
        }
        TableFactor::Derived { subquery, .. } => {
            rewrite_greenplum_cross_scans(subquery, definitions)
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            rewrite_greenplum_table_factor(&mut table_with_joins.relation, definitions, bindings)?;
            for join in &mut table_with_joins.joins {
                rewrite_greenplum_table_factor(&mut join.relation, definitions, bindings)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn try_single_source_pushdown(
    query: &mut Query,
    source: &DataSourceDefinition,
) -> Result<Option<String>> {
    let has_from = matches!(query.body.as_ref(), SetExpr::Select(s) if !s.from.is_empty());
    if !has_from {
        return Ok(None);
    }
    let mut ctes = BTreeSet::new();
    let mut table_count = 0usize;
    let mut eligible = true;
    let mut star_present = false;
    collect_single_source_info(
        query,
        &source.catalog_alias,
        &mut ctes,
        &mut table_count,
        &mut star_present,
        &mut eligible,
    );
    if !eligible || table_count == 0 || (table_count > 1 && star_present) {
        return Ok(None);
    }
    if source.kind.is_mysql_family() {
        let starrocks = source.kind == DataSourceKind::Starrocks;
        let has_cast = has_expr(query, |e| matches!(e, Expr::Cast { .. }));
        if !starrocks {
            if has_cast || !mysql_remote_compatible(query) {
                return Ok(None);
            }
        } else {
            rewrite_starrocks_query(query)?;
        }
    }
    let SetExpr::Select(select) = query.body.as_mut() else {
        return Ok(None);
    };
    let outer_projection = match unique_pushdown_projection(select) {
        Some(proj) => proj,
        None => return Ok(None),
    };
    strip_catalog_prefix(query, &source.catalog_alias, source.kind.is_mysql_family());
    let mut remote_sql = query.to_string();
    if source.kind.is_mysql_family() {
        remote_sql = mysql_backtick_identifiers(&remote_sql);
    }
    Ok(Some(format!(
        "SELECT {outer_projection} FROM {}_query({}, {})",
        source.kind.extension(),
        quote_literal(&source.catalog_alias),
        quote_literal(&remote_sql)
    )))
}

fn collect_single_source_info(
    query: &Query,
    catalog: &str,
    outer_ctes: &mut BTreeSet<String>,
    table_count: &mut usize,
    star_present: &mut bool,
    eligible: &mut bool,
) {
    let mut local_ctes = outer_ctes.clone();
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            local_ctes.insert(cte.alias.name.value.to_ascii_lowercase());
            collect_single_source_info(
                &cte.query,
                catalog,
                &mut local_ctes,
                table_count,
                star_present,
                eligible,
            );
        }
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        *eligible = false;
        return;
    };
    for item in &select.projection {
        if matches!(
            item,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
        ) {
            *star_present = true;
        }
    }
    let _ = visit_relations(select.as_ref(), |name| {
        if let Some(parts) = object_parts(name) {
            match parts.as_slice() {
                [single] if local_ctes.contains(&single.to_ascii_lowercase()) => {}
                [cat, _, _] if cat.eq_ignore_ascii_case(catalog) => {
                    *table_count += 1;
                }
                _ => {
                    *eligible = false;
                    return ControlFlow::Break(());
                }
            }
        } else {
            *eligible = false;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    let _ = visit_expressions(select.as_ref(), |expr| {
        if let Expr::Subquery(sub) = expr {
            collect_single_source_info(
                sub,
                catalog,
                &mut local_ctes,
                table_count,
                star_present,
                eligible,
            );
        }
        ControlFlow::<()>::Continue(())
    });
}

fn has_expr(query: &Query, pred: impl Fn(&Expr) -> bool + Copy) -> bool {
    visit_expressions(query, |e| {
        if pred(e) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}

fn mysql_remote_compatible(query: &Query) -> bool {
    if let Some(order) = &query.order_by
        && let OrderByKind::Expressions(exprs) = &order.kind
        && exprs.iter().any(|e| e.options.nulls_first.is_some())
    {
        return false;
    }
    let mut ok = true;
    let _ = visit_expressions(query, |expr| {
        match expr {
            Expr::GroupingSets(_) | Expr::Cube(_) | Expr::Rollup(_) => {
                ok = false;
                return ControlFlow::Break(());
            }
            Expr::Function(f) if f.name.to_string().eq_ignore_ascii_case("date_trunc") => {
                ok = false;
                return ControlFlow::Break(());
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    if !ok {
        return false;
    }
    if let SetExpr::Select(s) = query.body.as_ref() {
        for twj in &s.from {
            for join in &twj.joins {
                if matches!(
                    join.join_operator,
                    JoinOperator::FullOuter(_)
                        | JoinOperator::CrossJoin(_)
                        | JoinOperator::StraightJoin(_)
                ) {
                    return false;
                }
            }
        }
    }
    true
}

fn rewrite_starrocks_query(query: &mut Query) -> Result<()> {
    if let Some(order) = &mut query.order_by
        && let OrderByKind::Expressions(exprs) = &mut order.kind
    {
        for expr in exprs {
            expr.options.nulls_first = None;
        }
    }
    if let SetExpr::Select(select) = query.body.as_mut() {
        for twj in &mut select.from {
            rewrite_starrocks_joins(twj)?;
        }
    }
    let flow = visit_expressions_mut(query, |expr| {
        if let Expr::Cast { data_type, .. } = expr {
            let mapped = match starrocks_cast_type(data_type) {
                Ok(t) => t,
                Err(e) => return ControlFlow::Break(e),
            };
            *data_type = mapped;
        }
        ControlFlow::Continue(())
    });
    if let ControlFlow::Break(e) = flow {
        return Err(e);
    }
    Ok(())
}

fn starrocks_cast_type(ty: &DataType) -> Result<DataType> {
    let mapped = match ty {
        DataType::Numeric(ExactNumberInfo::PrecisionAndScale(p, s))
        | DataType::Decimal(ExactNumberInfo::PrecisionAndScale(p, s))
            if *p >= 1 && *p <= 38 && *s >= 0 && *s <= *p as i64 =>
        {
            format!("DECIMAL({p},{s})")
        }
        DataType::Numeric(ExactNumberInfo::Precision(p))
        | DataType::Decimal(ExactNumberInfo::Precision(p))
            if *p >= 1 && *p <= 38 =>
        {
            format!("DECIMAL({p},0)")
        }
        DataType::Varchar(Some(CharacterLength::IntegerLength { length, .. })) => {
            format!("VARCHAR({length})")
        }
        DataType::Char(Some(CharacterLength::IntegerLength { length, .. })) => {
            format!("CHAR({length})")
        }
        DataType::Text | DataType::String(_) | DataType::Varchar(None) => "VARCHAR".into(),
        DataType::Char(None) | DataType::Character(None) => "CHAR".into(),
        DataType::SmallInt(_) | DataType::Int2(_) => "SMALLINT".into(),
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => "INT".into(),
        DataType::BigInt(_) | DataType::Int8(_) => "BIGINT".into(),
        DataType::Boolean | DataType::Bool => "BOOLEAN".into(),
        DataType::Float4 | DataType::Real => "FLOAT".into(),
        DataType::Float8 | DataType::Double(_) | DataType::DoublePrecision => "DOUBLE".into(),
        DataType::Date => "DATE".into(),
        DataType::Time(_, TimezoneInfo::None) => "TIME".into(),
        DataType::Timestamp(_, TimezoneInfo::None) | DataType::Datetime(_) => "DATETIME".into(),
        _ => {
            return Err(EngineError::unsupported(
                "StarRocks single-source cast type is not supported",
            ));
        }
    };
    let mut parsed = Analyzer::parse(&format!("SELECT CAST(1 AS {mapped})"))?;
    let Statement::Query(q) = parsed.remove(0) else {
        unreachable!()
    };
    let SetExpr::Select(s) = *q.body else {
        unreachable!()
    };
    let SelectItem::UnnamedExpr(Expr::Cast { data_type, .. }) =
        s.projection.into_iter().next().unwrap()
    else {
        unreachable!()
    };
    Ok(data_type)
}

fn rewrite_starrocks_joins(twj: &mut TableWithJoins) -> Result<()> {
    let mut left_ref = factor_qualifier(&twj.relation);
    for join in &mut twj.joins {
        let right_ref = factor_qualifier(&join.relation);
        let using_cols = match &join.join_operator {
            JoinOperator::Join(JoinConstraint::Using(cols))
            | JoinOperator::Inner(JoinConstraint::Using(cols))
            | JoinOperator::Left(JoinConstraint::Using(cols))
            | JoinOperator::LeftOuter(JoinConstraint::Using(cols))
            | JoinOperator::Right(JoinConstraint::Using(cols))
            | JoinOperator::RightOuter(JoinConstraint::Using(cols))
            | JoinOperator::FullOuter(JoinConstraint::Using(cols)) => Some(cols.clone()),
            _ => None,
        };
        if let Some(cols) = using_cols
            && let (Some(l), Some(r)) = (&left_ref, &right_ref)
        {
            let mut predicate: Option<Expr> = None;
            for col in cols {
                let col_name = col.to_string();
                let eq = Expr::BinaryOp {
                    left: Box::new(Expr::CompoundIdentifier(vec![
                        Ident::new(l),
                        Ident::new(&col_name),
                    ])),
                    op: BinaryOperator::Eq,
                    right: Box::new(Expr::CompoundIdentifier(vec![
                        Ident::new(r),
                        Ident::new(&col_name),
                    ])),
                };
                predicate = Some(match predicate {
                    None => eq,
                    Some(prev) => Expr::BinaryOp {
                        left: Box::new(prev),
                        op: BinaryOperator::And,
                        right: Box::new(eq),
                    },
                });
            }
            if let Some(on_expr) = predicate {
                let constraint = JoinConstraint::On(on_expr);
                join.join_operator = match &join.join_operator {
                    JoinOperator::Join(_) => JoinOperator::Join(constraint),
                    JoinOperator::Inner(_) => JoinOperator::Inner(constraint),
                    JoinOperator::Left(_) => JoinOperator::Left(constraint),
                    JoinOperator::LeftOuter(_) => JoinOperator::LeftOuter(constraint),
                    JoinOperator::Right(_) => JoinOperator::Right(constraint),
                    JoinOperator::RightOuter(_) => JoinOperator::RightOuter(constraint),
                    JoinOperator::FullOuter(_) => JoinOperator::FullOuter(constraint),
                    other => other.clone(),
                };
            }
        }
        left_ref = right_ref.or(left_ref);
    }
    Ok(())
}

fn factor_qualifier(factor: &TableFactor) -> Option<String> {
    if let TableFactor::Table { name, alias, .. } = factor {
        if let Some(a) = alias {
            return Some(a.name.value.clone());
        }
        return object_parts(name)?.pop();
    }
    None
}

fn unique_pushdown_projection(select: &mut Select) -> Option<String> {
    if select.projection.iter().any(|p| {
        matches!(
            p,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
        )
    }) {
        return Some("*".into());
    }
    let mut labels = Vec::with_capacity(select.projection.len());
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut unknown = false;
    for item in &select.projection {
        let label = match item {
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            SelectItem::UnnamedExpr(Expr::Identifier(i)) => Some(i.value.clone()),
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                parts.last().map(|i| i.value.clone())
            }
            _ => None,
        };
        if let Some(l) = label {
            *counts.entry(l.to_ascii_lowercase()).or_default() += 1;
            labels.push(Some(l));
        } else {
            unknown = true;
            labels.push(None);
        }
    }
    let duplicates: BTreeSet<String> = counts
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(k, _)| k.clone())
        .collect();
    if duplicates.is_empty() {
        return Some("*".into());
    }
    if unknown {
        return None;
    }
    fn has_ambiguous_ref<V: Visit>(node: &V, duplicates: &BTreeSet<String>) -> bool {
        visit_expressions(node, |expr| {
            if let Expr::Identifier(i) = expr
                && duplicates.contains(&i.value.to_ascii_lowercase())
            {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .is_break()
    }
    let alias_referenced = select
        .selection
        .as_ref()
        .is_some_and(|e| has_ambiguous_ref(e, &duplicates))
        || has_ambiguous_ref(&select.group_by, &duplicates)
        || select
            .having
            .as_ref()
            .is_some_and(|e| has_ambiguous_ref(e, &duplicates));
    if alias_referenced {
        return None;
    }
    let mut used: BTreeSet<String> = counts.keys().cloned().collect();
    let mut out_parts = Vec::with_capacity(labels.len());
    for (i, (item, label)) in select.projection.iter_mut().zip(labels).enumerate() {
        let label = label.unwrap();
        let mut internal = label.clone();
        if duplicates.contains(&label.to_ascii_lowercase()) {
            internal = format!("__fed_output_{i}");
            while used.contains(&internal) {
                internal.push('_');
            }
            used.insert(internal.clone());
            let inner_expr = match item {
                SelectItem::ExprWithAlias { expr, .. } | SelectItem::UnnamedExpr(expr) => {
                    expr.clone()
                }
                _ => unreachable!(),
            };
            *item = SelectItem::ExprWithAlias {
                expr: inner_expr,
                alias: Ident::with_quote('"', &internal),
            };
        }
        out_parts.push(format!(
            "{} AS {}",
            quote_identifier(&internal),
            quote_identifier(&label)
        ));
    }
    Some(out_parts.join(", "))
}

fn strip_catalog_prefix(query: &mut Query, catalog: &str, quote_idents: bool) {
    let _ = visit_relations_mut(query, |name| {
        if name.0.len() == 3
            && let Some(ObjectNamePart::Identifier(first)) = name.0.first()
            && first.value.eq_ignore_ascii_case(catalog)
        {
            name.0.remove(0);
        }
        if quote_idents {
            for part in &mut name.0 {
                if let ObjectNamePart::Identifier(i) = part {
                    i.quote_style = Some('"');
                }
            }
        }
        ControlFlow::<()>::Continue(())
    });
    if quote_idents {
        struct Quoter;
        impl VisitorMut for Quoter {
            type Break = ();
            fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<()> {
                if let Some(alias) = match factor {
                    TableFactor::Table { alias, .. }
                    | TableFactor::Derived { alias, .. }
                    | TableFactor::Function { alias, .. } => alias.as_mut(),
                    _ => None,
                } {
                    alias.name.quote_style = Some('"');
                    for col in &mut alias.columns {
                        col.name.quote_style = Some('"');
                    }
                }
                ControlFlow::Continue(())
            }
            fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
                if let Some(with) = &mut q.with {
                    for cte in &mut with.cte_tables {
                        cte.alias.name.quote_style = Some('"');
                        for col in &mut cte.alias.columns {
                            col.name.quote_style = Some('"');
                        }
                    }
                }
                if let SetExpr::Select(select) = q.body.as_mut() {
                    for item in &mut select.projection {
                        if let SelectItem::ExprWithAlias { alias, .. } = item {
                            alias.quote_style = Some('"');
                        }
                    }
                }
                ControlFlow::Continue(())
            }
        }
        let _ = VisitMut::visit(query, &mut Quoter);
    }
    let _ = visit_expressions_mut(query, |expr| {
        match expr {
            Expr::CompoundIdentifier(parts) => {
                if parts.len() == 4 && parts[0].value.eq_ignore_ascii_case(catalog) {
                    parts.remove(0);
                }
                if quote_idents {
                    for part in parts {
                        part.quote_style = Some('"');
                    }
                }
            }
            Expr::Identifier(ident) if quote_idents => {
                ident.quote_style = Some('"');
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
}

/// Convert double-quoted SQL identifiers outside single-quoted string literals to MySQL backticks.
fn mysql_backtick_identifiers(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                out.push_str(&sql[start..i]);
            }
            b'"' => {
                i += 1;
                let mut ident = String::new();
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'"' {
                            ident.push('"');
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    let ch = sql[i..].chars().next().unwrap();
                    ident.push(ch);
                    i += ch.len_utf8();
                }
                out.push('`');
                out.push_str(&ident.replace('`', "``"));
                out.push('`');
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, kind: DataSourceKind, alias: &str) -> DataSourceDefinition {
        DataSourceDefinition {
            id: DataSourceId(id.into()),
            kind,
            catalog_alias: alias.into(),
            secret_ref: "unused".into(),
            organization: None,
            default_schema: "public".into(),
            read_only: true,
            ddl_enabled: false,
            allowed_schemas: vec!["public".into(), "sales".into()],
            enabled: true,
        }
    }

    #[test]
    fn mysql_and_starrocks_single_source_pushdown_handles_aliases_and_dedup() {
        let defs = vec![
            source("m1", DataSourceKind::Mysql, "my_src"),
            source("s1", DataSourceKind::Starrocks, "sr_src"),
        ];
        let mut s = BTreeSet::new();
        s.insert(DataSourceId("m1".into()));
        let rewritten = rewrite(
            "SELECT my_src.sales.a.id, my_src.sales.b.id FROM my_src.sales.a JOIN my_src.sales.b ON my_src.sales.a.id = my_src.sales.b.id",
            &s,
            &defs,
        )
        .unwrap();
        assert!(rewritten.contains("mysql_query('my_src',"), "{rewritten}");
        assert!(
            rewritten.contains("\"__fed_output_0\" AS \"id\""),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("\"__fed_output_1\" AS \"id\""),
            "{rewritten}"
        );
        assert!(rewritten.contains("`sales`.`a`"), "{rewritten}");

        let mut sr = BTreeSet::new();
        sr.insert(DataSourceId("s1".into()));
        let sr_sql = rewrite(
            "SELECT CAST(sr_src.sales.orders.id AS BIGINT) FROM sr_src.sales.orders ORDER BY 1 NULLS LAST",
            &sr,
            &defs,
        )
        .unwrap();
        assert!(sr_sql.contains("mysql_query('sr_src',"), "{sr_sql}");
        assert!(
            sr_sql.contains("CAST(`sales`.`orders`.`id` AS BIGINT)"),
            "{sr_sql}"
        );
        assert!(!sr_sql.contains("NULLS LAST"), "{sr_sql}");

        let sr_join = rewrite(
            "SELECT a.id FROM sr_src.sales.a JOIN sr_src.sales.b USING (id)",
            &sr,
            &defs,
        )
        .unwrap();
        assert!(sr_join.contains("ON `a`.`id` = `b`.`id`"), "{sr_join}");
    }

    #[test]
    fn greenplum_cross_source_rewrites_to_postgres_query_table_function() {
        let defs = vec![
            source("gp1", DataSourceKind::Greenplum, "gp_src"),
            source("pg1", DataSourceKind::Postgres, "pg_src"),
        ];
        let mut s = BTreeSet::new();
        s.insert(DataSourceId("gp1".into()));
        s.insert(DataSourceId("pg1".into()));
        let rewritten = rewrite(
            "SELECT gp_src.public.t1.id, pg_src.public.t2.id FROM gp_src.public.t1 JOIN pg_src.public.t2 ON gp_src.public.t1.id = pg_src.public.t2.id",
            &s,
            &defs,
        )
        .unwrap();
        assert!(
            rewritten.contains("postgres_query('gp_src', 'SELECT * FROM \"public\".\"t1\"')"),
            "{rewritten}"
        );
        assert!(rewritten.contains("\"t1\".id"), "{rewritten}");
        assert!(rewritten.contains("pg_src.public.t2"), "{rewritten}");
    }

    #[test]
    fn starrocks_rejects_unsupported_interval_cast_and_preserves_parameterized_local() {
        let defs = vec![source("s1", DataSourceKind::Starrocks, "sr_src")];
        let mut sr = BTreeSet::new();
        sr.insert(DataSourceId("s1".into()));
        let err = rewrite(
            "SELECT CAST('1 day' AS INTERVAL) FROM sr_src.sales.t",
            &sr,
            &defs,
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, "0A000");

        let param_sql = "SELECT sr_src.sales.t.id FROM sr_src.sales.t WHERE sr_src.sales.t.id = $1";
        assert_eq!(rewrite(param_sql, &sr, &defs).unwrap(), param_sql);
    }
}
