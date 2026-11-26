//! One session setting registry for PostgreSQL probes and state-changing commands.
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use sqlparser::{
    ast::*,
    dialect::PostgreSqlDialect,
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};

pub const SERVER_VERSION: &str = "16.0 (FedEngine 0.1; PostgreSQL protocol subset)";
pub(crate) fn is_reindex_statement(sql: &str) -> bool {
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else {
        return false;
    };
    let mut non_ws = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    if matches!(non_ws.last(), Some(Token::SemiColon)) {
        non_ws.pop();
    }
    if non_ws.iter().any(|t| matches!(t, Token::SemiColon)) {
        return false;
    }
    matches!(
        non_ws.first(),
        Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("REINDEX")
    )
}
pub fn parse_pg(sql: &str) -> Result<Vec<Statement>> {
    if sql.len() > 1024 * 1024 {
        return Err(EngineError::new("54000", "SQL exceeds 1 MiB limit"));
    }
    Parser::parse_sql(&PostgreSqlDialect {}, sql).map_err(|_| {
        EngineError::new(
            "42601",
            "SQL syntax is outside the supported PostgreSQL grammar",
        )
    })
}
fn default_path(session: &SessionContext, scope: &MetadataScope) -> Vec<SessionIdentifier> {
    if scope.virtual_database.is_some() {
        return vec![
            SessionIdentifier {
                name: "$user".into(),
                quoted: true,
            },
            SessionIdentifier {
                name: "public".into(),
                quoted: false,
            },
        ];
    }
    scope
        .catalogs
        .iter()
        .find(|c| c.catalog == session.database)
        .filter(|c| !c.default_schema.is_empty())
        .map(|c| {
            vec![SessionIdentifier {
                name: c.default_schema.clone(),
                quoted: true,
            }]
        })
        .unwrap_or_default()
}
pub fn effective_path(session: &SessionContext, scope: &MetadataScope) -> Vec<String> {
    let entries = session
        .search_path
        .clone()
        .unwrap_or_else(|| default_path(session, scope));
    let mut schemas = scope
        .catalogs
        .iter()
        .filter(|c| c.catalog == session.database)
        .flat_map(|c| {
            c.schemas
                .iter()
                .filter(|schema| {
                    scope
                        .virtual_database
                        .as_ref()
                        .is_some_and(|db| db.config.owner == scope.principal)
                        || c.grants.iter().any(|g| {
                            !g.actions.is_empty() && (g.schema == "*" || &g.schema == *schema)
                        })
                })
                .cloned()
        })
        .collect::<Vec<_>>();
    schemas.extend(["pg_catalog".into(), "information_schema".into()]);
    if scope.virtual_database.is_some() {
        schemas.push("public".into());
    }
    let mut result = vec![];
    for entry in entries {
        let name = if entry.name == "$user" {
            scope.principal.clone()
        } else if entry.quoted {
            entry.name
        } else {
            entry.name.to_lowercase()
        };
        if schemas.contains(&name) && !result.contains(&name) {
            result.push(name);
        }
    }
    result
}
fn canonical_setting_name(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "time.zone" => "timezone".to_string(),
        "transaction.isolation.level" => "transaction_isolation".to_string(),
        other => other.to_string(),
    }
}

/// Returns the active session timezone from `session.settings`, defaulting to UTC.
pub fn active_timezone(session: &SessionContext) -> Result<crate::timezone::SessionTimeZone> {
    match session.settings.get("timezone") {
        Some(raw) => crate::timezone::parse_session_timezone(raw),
        None => Ok(crate::timezone::SessionTimeZone::utc()),
    }
}

pub fn setting(session: &SessionContext, scope: &MetadataScope, name: &str) -> Result<String> {
    let name = canonical_setting_name(name);
    if name == "search_path" {
        let names = session
            .search_path
            .clone()
            .unwrap_or_else(|| default_path(session, scope));
        return Ok(names
            .into_iter()
            .map(|n| {
                if n.quoted {
                    quote_identifier(&n.name)
                } else {
                    n.name
                }
            })
            .collect::<Vec<_>>()
            .join(", "));
    }
    if let Some(v) = session.settings.get(&name) {
        return Ok(v.clone());
    }
    Ok(match name.as_str() {
        "server_version" => SERVER_VERSION,
        "server_version_num" => "160000",
        "server_encoding" | "client_encoding" => "UTF8",
        "standard_conforming_strings" | "integer_datetimes" => "on",
        "timezone" => "UTC",
        "datestyle" => "ISO, YMD",
        "application_name" => "",
        "extra_float_digits" => "3",
        "client_min_messages" => "notice",
        "transaction_isolation" | "default_transaction_isolation" => "read committed",
        "transaction_read_only" | "default_transaction_read_only" => "off",
        _ => {
            return Err(EngineError::new(
                "42704",
                "unrecognized or unsupported configuration parameter",
            ));
        }
    }
    .into())
}
fn expr_string(expr: &Expr) -> Result<String> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) | Value::Number(s, _) => {
                Ok(s.clone())
            }
            Value::Boolean(b) => Ok(if *b { "on" } else { "off" }.into()),
            _ => Err(EngineError::unsupported("setting requires a literal value")),
        },
        Expr::Identifier(i) => Ok(i.value.clone()),
        Expr::UnaryOp { op, expr } if *op == UnaryOperator::Minus => {
            Ok(format!("-{}", expr_string(expr)?))
        }
        _ => Err(EngineError::unsupported("setting requires a literal value")),
    }
}
fn set(session: &mut SessionContext, name: &str, values: &[Expr]) -> Result<()> {
    let name = canonical_setting_name(name);
    if name == "search_path" {
        let entries = values
            .iter()
            .map(|v| match v {
                Expr::Identifier(i) => Ok(SessionIdentifier {
                    name: i.value.clone(),
                    quoted: i.quote_style.is_some(),
                }),
                _ => Ok(SessionIdentifier {
                    name: expr_string(v)?,
                    quoted: false,
                }),
            })
            .collect::<Result<Vec<_>>>()?;
        if entries.len() > 64 {
            return Err(EngineError::new("54000", "search_path exceeds 64 entries"));
        }
        // Quoted setting strings may contain an SQL identifier list; parse it as expressions.
        let entries = if values.len() == 1 && matches!(&values[0], Expr::Value(_)) {
            let text = expr_string(&values[0])?;
            if text.is_empty() {
                vec![]
            } else {
                let parsed = parse_pg(&format!("SELECT {text}"))?;
                let Statement::Query(q) = &parsed[0] else {
                    return Err(EngineError::new("42601", "invalid search_path"));
                };
                let SetExpr::Select(select) = q.body.as_ref() else {
                    return Err(EngineError::new("42601", "invalid search_path"));
                };
                select
                    .projection
                    .iter()
                    .map(|p| {
                        if let SelectItem::UnnamedExpr(Expr::Identifier(i)) = p {
                            Ok(SessionIdentifier {
                                name: i.value.clone(),
                                quoted: i.quote_style.is_some(),
                            })
                        } else {
                            Err(EngineError::new("42601", "invalid search_path identifier"))
                        }
                    })
                    .collect::<Result<_>>()?
            }
        } else {
            entries
        };
        session.search_path = Some(entries);
        session.namespace_version = session
            .namespace_version
            .checked_add(1)
            .ok_or_else(|| EngineError::new("54000", "session namespace version exhausted"))?;
        return Ok(());
    }
    if values.len() != 1 {
        return Err(EngineError::unsupported("setting requires one value"));
    }
    if name == "timezone" {
        let tz = crate::timezone::parse_timezone_expr(&values[0])?;
        session.settings.insert(name, tz.display);
        return Ok(());
    }
    let value = expr_string(&values[0])?;
    let normalized = value.to_ascii_lowercase();
    let valid = match name.as_str() {
        "application_name" => value.len() <= 256,
        "client_encoding" => matches!(normalized.as_str(), "utf8" | "utf-8"),
        "datestyle" => matches!(normalized.as_str(), "iso" | "iso, ymd" | "iso, mdy"),
        "standard_conforming_strings" => matches!(normalized.as_str(), "on" | "true"),
        "extra_float_digits" => value == "3",
        "client_min_messages" => normalized == "notice",
        _ => {
            return Err(EngineError::new(
                "42704",
                "unrecognized or unsupported configuration parameter",
            ));
        }
    };
    if !valid {
        return Err(EngineError::unsupported(
            "setting value does not match implemented behavior",
        ));
    }
    let value = match name.as_str() {
        "client_encoding" => "UTF8".into(),
        "standard_conforming_strings" => "on".into(),
        _ => value,
    };
    session.settings.insert(name, value);
    Ok(())
}
/// Special `TIME ZONE` command variants that `sqlparser`'s PostgreSQL grammar cannot parse directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialTimezoneCommand {
    /// `SET [SESSION] TIME ZONE DEFAULT` or `SET [SESSION] TIME ZONE LOCAL`.
    SetDefault,
    /// `SET LOCAL TIME ZONE DEFAULT` or `SET LOCAL TIME ZONE LOCAL` (unsupported `0A000`).
    SetLocalUnsupported,
    /// `RESET TIME ZONE`.
    Reset,
}

/// Recognizes unquoted single-statement `RESET TIME ZONE` and `SET [SESSION|LOCAL] TIME ZONE (LOCAL|DEFAULT)` forms.
pub fn parse_special_timezone_command(sql: &str) -> Option<SpecialTimezoneCommand> {
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else {
        return None;
    };
    let mut non_ws = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    if matches!(non_ws.last(), Some(Token::SemiColon)) {
        non_ws.pop();
    }
    if non_ws.iter().any(|t| matches!(t, Token::SemiColon)) {
        return None;
    }
    let words = non_ws
        .iter()
        .map(|t| match t {
            Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    match refs.as_slice() {
        ["RESET", "TIME", "ZONE"] => Some(SpecialTimezoneCommand::Reset),
        ["SET", "TIME", "ZONE", "DEFAULT" | "LOCAL"]
        | ["SET", "SESSION", "TIME", "ZONE", "DEFAULT" | "LOCAL"] => {
            Some(SpecialTimezoneCommand::SetDefault)
        }
        ["SET", "LOCAL", "TIME", "ZONE", "DEFAULT" | "LOCAL"] => {
            Some(SpecialTimezoneCommand::SetLocalUnsupported)
        }
        _ => None,
    }
}

/// Parse validates against a copy; only Execute commits the returned session state.
pub fn session_command(
    sql: &str,
    session: &SessionContext,
) -> Result<Option<(SessionContext, String)>> {
    Ok(session_command_with_timezone(sql, session)?.map(|(next, cmd, _)| (next, cmd)))
}

/// Like [`session_command`], additionally returning the new [`crate::timezone::SessionTimeZone`]
/// whenever the command updates or resets the active session timezone.
pub fn session_command_with_timezone(
    sql: &str,
    session: &SessionContext,
) -> Result<
    Option<(
        SessionContext,
        String,
        Option<crate::timezone::SessionTimeZone>,
    )>,
> {
    if sql.len() > 1024 * 1024 {
        return Err(EngineError::new("54000", "SQL exceeds 1 MiB limit"));
    }
    if let Some(cmd) = parse_special_timezone_command(sql) {
        let mut next = session.clone();
        return match cmd {
            SpecialTimezoneCommand::Reset => {
                reset(&mut next, "timezone")?;
                Ok(Some((
                    next,
                    "RESET".into(),
                    Some(crate::timezone::SessionTimeZone::utc()),
                )))
            }
            SpecialTimezoneCommand::SetDefault => {
                reset(&mut next, "timezone")?;
                Ok(Some((
                    next,
                    "SET".into(),
                    Some(crate::timezone::SessionTimeZone::utc()),
                )))
            }
            SpecialTimezoneCommand::SetLocalUnsupported => Err(EngineError::unsupported(
                "SET LOCAL TIME ZONE is not supported",
            )),
        };
    }
    let mut statements = match parse_pg(sql) {
        Ok(s) => s,
        Err(_) if is_reindex_statement(sql) => return Ok(None),
        Err(err) => return Err(err),
    };
    if statements.len() != 1 {
        return Err(EngineError::unsupported("one statement is required"));
    }
    let mut next = session.clone();
    match statements.remove(0) {
        Statement::Set(Set::SingleAssignment {
            scope,
            hivevar,
            variable,
            values,
        }) => {
            if hivevar || scope == Some(ContextModifier::Local) {
                return Err(EngineError::unsupported(
                    "SET LOCAL and dialect-specific settings are unsupported",
                ));
            }
            let raw_name = variable.to_string();
            let canonical = canonical_setting_name(&raw_name);
            if values.len() == 1
                && matches!(&values[0],Expr::Identifier(i) if i.value.eq_ignore_ascii_case("default"))
            {
                reset(&mut next, &canonical)?;
            } else {
                set(&mut next, &canonical, &values)?;
            }
            let tz = (canonical == "timezone")
                .then(|| active_timezone(&next))
                .transpose()?;
            Ok(Some((next, "SET".into(), tz)))
        }
        Statement::Set(Set::SetTimeZone { local, value }) => {
            if local {
                return Err(EngineError::unsupported(
                    "SET LOCAL TIME ZONE is not supported",
                ));
            }
            set(&mut next, "timezone", &[value])?;
            let tz = active_timezone(&next)?;
            Ok(Some((next, "SET".into(), Some(tz))))
        }
        Statement::Reset(r) => {
            let tz = match r.reset {
                Reset::ALL => {
                    next.settings.clear();
                    next.search_path = None;
                    next.namespace_version += 1;
                    Some(crate::timezone::SessionTimeZone::utc())
                }
                Reset::ConfigurationParameter(n) => {
                    let canonical = canonical_setting_name(&n.to_string());
                    reset(&mut next, &canonical)?;
                    (canonical == "timezone").then(crate::timezone::SessionTimeZone::utc)
                }
            };
            Ok(Some((next, "RESET".into(), tz)))
        }
        Statement::Set(_) => Err(EngineError::unsupported(
            "session command is outside the supported registry",
        )),
        _ => Ok(None),
    }
}

/// Recognizes scalar `SELECT set_config('timezone', value, is_local)` statements,
/// returning the updated session context, parsed timezone, and output column label.
pub fn set_config_timezone_query(
    sql: &str,
    session: &SessionContext,
) -> Result<Option<(SessionContext, crate::timezone::SessionTimeZone, String)>> {
    let mut statements = match parse_pg(sql) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    if statements.len() != 1 {
        return Ok(None);
    }
    let Statement::Query(q) = statements.remove(0) else {
        return Ok(None);
    };
    if q.with.is_some() || q.order_by.is_some() || q.limit_clause.is_some() || !q.locks.is_empty() {
        return Ok(None);
    }
    let SetExpr::Select(select) = q.body.as_ref() else {
        return Ok(None);
    };
    if select.distinct.is_some()
        || !select.from.is_empty()
        || select.selection.is_some()
        || !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs, modifiers) if exprs.is_empty() && modifiers.is_empty()
        )
        || select.having.is_some()
        || select.projection.len() != 1
    {
        return Ok(None);
    }
    let (expr, col_name) = match &select.projection[0] {
        SelectItem::UnnamedExpr(e) => (e, "set_config".to_string()),
        SelectItem::ExprWithAlias { expr, alias } => (expr, alias.value.clone()),
        _ => return Ok(None),
    };
    let Expr::Function(f) = expr else {
        return Ok(None);
    };
    let fn_name = f.name.to_string().to_ascii_lowercase();
    if !matches!(
        fn_name.as_str(),
        "set_config"
            | "pg_catalog.set_config"
            | "system.main.set_config"
            | "system.pg_catalog.set_config"
    ) {
        return Ok(None);
    }
    let FunctionArguments::List(arg_list) = &f.args else {
        return Ok(None);
    };
    if !arg_list.clauses.is_empty() || arg_list.args.len() != 3 {
        return Ok(None);
    }
    let mut args = Vec::with_capacity(3);
    for a in &arg_list.args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a else {
            return Ok(None);
        };
        args.push(e);
    }
    let Some((setting_name, false)) = crate::timezone::extract_interval_or_string_literal(args[0])
    else {
        return Ok(None);
    };
    if !setting_name.trim().eq_ignore_ascii_case("timezone") {
        return Ok(None);
    }
    match args[2] {
        Expr::Value(v) if v.value == Value::Boolean(true) => {
            return Err(EngineError::unsupported(
                "SET LOCAL TIME ZONE is not supported",
            ));
        }
        Expr::Value(v) if v.value == Value::Boolean(false) => {}
        _ => {
            return Err(EngineError::config(
                "set_config requires a boolean literal for is_local",
            ));
        }
    }
    let Some((raw_value, false)) = crate::timezone::extract_interval_or_string_literal(args[1])
    else {
        return Err(EngineError::config(
            "invalid value for parameter \"TimeZone\"",
        ));
    };
    let tz = crate::timezone::parse_session_timezone(&raw_value)?;
    let mut next = session.clone();
    next.settings.insert("timezone".into(), tz.display.clone());
    Ok(Some((next, tz, col_name)))
}
fn reset(session: &mut SessionContext, name: &str) -> Result<()> {
    let name = canonical_setting_name(name);
    if name == "search_path" {
        session.search_path = None;
        session.namespace_version += 1;
    } else {
        let scope = MetadataScope {
            virtual_database: None,
            databases: vec![],
            database: session.database.clone(),
            principal: String::new(),
            catalogs: vec![],
        };
        setting(&SessionContext::default(), &scope, &name)?;
        session.settings.remove(&name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindex_passes_session_command_unless_multi_statement() {
        assert_eq!(
            session_command(
                "REINDEX INDEX pg_crm.public.idx_a",
                &SessionContext::default()
            )
            .unwrap(),
            None
        );
        assert!(
            session_command("REINDEX INDEX idx_a; SELECT 1", &SessionContext::default()).is_err()
        );
    }

    #[test]
    fn timezone_session_commands_and_set_config_work() {
        let base = SessionContext::default();
        for (sql, want_disp, want_duck, want_cmd) in [
            (
                "SET TIME ZONE 'Asia/Shanghai'",
                "Asia/Shanghai",
                "Asia/Shanghai",
                "SET",
            ),
            ("SET TIME ZONE '+08:00'", "+08:00", "Etc/GMT-8", "SET"),
            ("SET TIME ZONE 8", "+08:00", "Etc/GMT-8", "SET"),
            ("SET TIME ZONE -5", "-05:00", "Etc/GMT+5", "SET"),
            (
                "SET TIME ZONE INTERVAL '+08:00'",
                "+08:00",
                "Etc/GMT-8",
                "SET",
            ),
            (
                "SET TIME ZONE INTERVAL '-5 hours'",
                "-05:00",
                "Etc/GMT+5",
                "SET",
            ),
            (
                "SET TIME ZONE INTERVAL '+08:00' HOUR TO MINUTE",
                "+08:00",
                "Etc/GMT-8",
                "SET",
            ),
            ("SET timezone = 'UTC+8'", "UTC+8", "Etc/GMT+8", "SET"),
            (
                "SET SESSION TIME ZONE '+09:00'",
                "+09:00",
                "Etc/GMT-9",
                "SET",
            ),
            ("SET TIME ZONE DEFAULT", "UTC", "UTC", "SET"),
            ("SET TIME ZONE LOCAL", "UTC", "UTC", "SET"),
            ("RESET TIME ZONE", "UTC", "UTC", "RESET"),
            ("RESET ALL", "UTC", "UTC", "RESET"),
        ] {
            let (next, cmd, maybe_tz) = session_command_with_timezone(sql, &base).unwrap().unwrap();
            assert_eq!(cmd, want_cmd, "{sql}");
            let tz = maybe_tz.unwrap();
            assert_eq!(tz.display, want_disp, "{sql}");
            assert_eq!(tz.duckdb_zone, want_duck, "{sql}");
            assert_eq!(active_timezone(&next).unwrap().display, want_disp, "{sql}");
        }

        for (bad_sql, want_state) in [
            ("SET LOCAL TIME ZONE 'UTC'", "0A000"),
            ("SET LOCAL timezone = 'UTC'", "0A000"),
            ("SET TIME ZONE 5.5", "0A000"),
            ("SET TIME ZONE INTERVAL '5 hours 30 minutes'", "0A000"),
            ("SET TIME ZONE INTERVAL '1 day'", "22023"),
            ("SET TIME ZONE 'Invalid/Zone'", "22023"),
        ] {
            let err = session_command_with_timezone(bad_sql, &base).unwrap_err();
            assert_eq!(err.sqlstate, want_state, "{bad_sql}");
        }

        let (next, tz, col) = set_config_timezone_query(
            "SELECT pg_catalog.set_config('TimeZone', 'Asia/Shanghai', false) AS tz",
            &base,
        )
        .unwrap()
        .unwrap();
        assert_eq!(col, "tz");
        assert_eq!(tz.display, "Asia/Shanghai");
        assert_eq!(active_timezone(&next).unwrap().display, "Asia/Shanghai");

        assert_eq!(
            set_config_timezone_query(
                "SELECT set_config('timezone', 'Asia/Shanghai', true)",
                &base
            )
            .unwrap_err()
            .sqlstate,
            "0A000"
        );
        assert_eq!(
            set_config_timezone_query("SELECT set_config('timezone', 'Mars/Bad', false)", &base)
                .unwrap_err()
                .sqlstate,
            "22023"
        );
        assert!(
            set_config_timezone_query("SELECT set_config('application_name', 'x', false)", &base)
                .unwrap()
                .is_none()
        );
    }
}
