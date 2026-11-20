use crate::Analyzer;
use modorch_fedengine_core::*;
use sqlparser::{ast::*, dialect::PostgreSqlDialect, parser::Parser};
use std::ops::ControlFlow;

fn parse_parameter_sql(sql: &str) -> Result<Vec<Statement>> {
    Analyzer::parse(sql).or_else(|_| {
        if sql.len() > 1024 * 1024 {
            return Err(EngineError::new("54000", "SQL exceeds 1 MiB limit"));
        }
        Parser::parse_sql(&PostgreSqlDialect {}, sql)
            .map_err(|_| EngineError::new("42601", "SQL syntax is outside the supported grammar"))
    })
}

pub fn is_bare_numeric_data_type(ty: &DataType) -> bool {
    match ty {
        DataType::Numeric(ExactNumberInfo::None) | DataType::Decimal(ExactNumberInfo::None) => true,
        DataType::Custom(name, modifiers) if modifiers.is_empty() => {
            let s = name.to_string().to_ascii_lowercase();
            matches!(
                s.as_str(),
                "numeric" | "decimal" | "pg_catalog.numeric" | "pg_catalog.decimal"
            )
        }
        _ => false,
    }
}

/// Lower the parameters actually referenced by an execution plan to dense slots.
/// The returned zero-based indices address the original client parameter vector.
/// Client parameter discovery and validation must happen before this lowering.
pub fn compact_parameters(sql: &str) -> Result<(String, Vec<usize>)> {
    let mut statements = parse_parameter_sql(sql)?;
    if statements.len() != 1 {
        return Err(EngineError::unsupported("one SQL statement is required"));
    }
    // Validate the placeholder grammar and bound even when a caller has no types.
    parameter_hints(sql)?;
    let mut slots = std::collections::BTreeSet::new();
    let _ = visit_expressions(&statements, |e| {
        if let Expr::Value(v) = e
            && let Value::Placeholder(p) = &v.value
        {
            slots.insert(p[1..].parse::<usize>().expect("validated parameter") - 1);
        }
        ControlFlow::<()>::Continue(())
    });
    let slots = slots.into_iter().collect::<Vec<_>>();
    let _ = visit_expressions_mut(&mut statements, |e| {
        if let Expr::Value(v) = e
            && let Value::Placeholder(p) = &mut v.value
        {
            let original = p[1..].parse::<usize>().expect("validated parameter") - 1;
            *p = format!("${}", slots.binary_search(&original).unwrap() + 1);
        }
        ControlFlow::<()>::Continue(())
    });
    Ok((statements.remove(0).to_string(), slots))
}
/// Infer only explicit casts. Context-sensitive inference is refused rather than guessing an OID.
pub fn parameter_hints(sql: &str) -> Result<Vec<Option<String>>> {
    struct Hints {
        types: Vec<Option<String>>,
    }
    impl Visitor for Hints {
        type Break = EngineError;
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<Self::Break> {
            if let Expr::Value(v) = e
                && let Value::Placeholder(p) = &v.value
            {
                let Some(n) = p
                    .strip_prefix('$')
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| *n > 0 && *n <= 1024)
                else {
                    return ControlFlow::Break(EngineError::unsupported(
                        "only numeric $1..$1024 parameters are supported",
                    ));
                };
                self.types.resize(self.types.len().max(n), None);
            }
            if let Expr::Cast {
                expr, data_type, ..
            } = e
                && let Expr::Value(v) = expr.as_ref()
                && let Value::Placeholder(p) = &v.value
                && let Some(n) = p
                    .strip_prefix('$')
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| *n > 0 && *n <= 1024)
            {
                self.types.resize(self.types.len().max(n), None);
                self.types[n - 1].get_or_insert_with(|| data_type.to_string());
            }
            ControlFlow::Continue(())
        }
    }
    let mut hints = Hints { types: Vec::new() };
    for s in parse_parameter_sql(sql)? {
        if let ControlFlow::Break(e) = Visit::visit(&s, &mut hints) {
            return Err(e);
        }
    }
    Ok(hints.types)
}

fn parse_cast_data_type(t: &str) -> Result<DataType> {
    let mut p = Analyzer::parse(&format!("SELECT CAST(1 AS {t})"))?;
    let Statement::Query(q) = p.remove(0) else {
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

/// Add parameter casts with the SQL AST; strings/comments are never rewritten.
/// Explicit casts on individual placeholder occurrences are preserved unless they are bare NUMERIC/DECIMAL.
pub fn cast_parameters(sql: &str, types: &[String]) -> Result<String> {
    struct Casts {
        types: Vec<Option<DataType>>,
        skip_depth: usize,
    }
    impl VisitorMut for Casts {
        type Break = EngineError;
        fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<Self::Break> {
            if self.skip_depth > 0 {
                return ControlFlow::Continue(());
            }
            if let Expr::Cast {
                expr, data_type, ..
            } = e
                && let Expr::Value(v) = expr.as_ref()
                && let Value::Placeholder(p) = &v.value
                && let Some(n) = p.strip_prefix('$').and_then(|v| v.parse::<usize>().ok())
            {
                if is_bare_numeric_data_type(data_type)
                    && let Some(Some(t)) = n.checked_sub(1).and_then(|i| self.types.get(i))
                {
                    *data_type = t.clone();
                }
                self.skip_depth = 1;
            }
            ControlFlow::Continue(())
        }
        fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<Self::Break> {
            if self.skip_depth > 0 {
                if matches!(e, Expr::Cast { .. }) {
                    self.skip_depth = 0;
                }
                return ControlFlow::Continue(());
            }
            if let Expr::Value(v) = e
                && let Value::Placeholder(p) = &v.value
                && let Some(n) = p.strip_prefix('$').and_then(|v| v.parse::<usize>().ok())
                && let Some(Some(t)) = n.checked_sub(1).and_then(|i| self.types.get(i))
            {
                let template = format!("SELECT CAST({p} AS {t})");
                if let Ok(mut statements) = parse_parameter_sql(&template)
                    && let Statement::Query(q) = statements.remove(0)
                    && let SetExpr::Select(s) = *q.body
                    && let SelectItem::UnnamedExpr(cast) = s.projection.into_iter().next().unwrap()
                {
                    *e = cast;
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut parsed = parse_parameter_sql(sql)?;
    let mut casts = Casts {
        types: Vec::new(),
        skip_depth: 0,
    };
    for t in types {
        if t == "SKIP" {
            casts.types.push(None);
            continue;
        }
        casts.types.push(Some(parse_cast_data_type(t)?));
    }
    for s in &mut parsed {
        if let ControlFlow::Break(e) = VisitMut::visit(s, &mut casts) {
            return Err(e);
        }
    }
    Ok(parsed[0].to_string())
}
