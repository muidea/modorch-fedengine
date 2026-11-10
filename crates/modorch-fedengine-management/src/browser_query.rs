//! Bounded, read-only consumers of the normal query coordinator.
use modorch::{prelude::*, *};
use modorch_fedengine_contract::*;
use modorch_fedengine_core::*;
use serde::Serialize;
use sqlparser::ast::{Query, Select, Statement, Visit, Visitor};
use std::{
    ops::ControlFlow,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(crate) const ROW_LIMIT: usize = 1000;
pub(crate) const BYTE_LIMIT: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Option<String>>>,
    pub notices: Vec<String>,
    pub truncated: bool,
    pub elapsed_ms: u64,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ResultColumn {
    name: String,
    type_name: String,
    type_oid: u32,
    type_modifier: i32,
}
struct ReadOnly;
impl Visitor for ReadOnly {
    type Break = EngineError;
    fn pre_visit_statement(&mut self, s: &Statement) -> ControlFlow<Self::Break> {
        if !matches!(s, Statement::Query(_)) {
            return ControlFlow::Break(EngineError::unsupported(
                "SQL debugger accepts one read-only query",
            ));
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<Self::Break> {
        if !q.locks.is_empty() {
            return ControlFlow::Break(EngineError::unsupported("locking queries are disabled"));
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_select(&mut self, s: &Select) -> ControlFlow<Self::Break> {
        if s.into.is_some() {
            return ControlFlow::Break(EngineError::unsupported("SELECT INTO is disabled"));
        }
        ControlFlow::Continue(())
    }
}
pub(crate) fn read_only(sql: &str) -> Result<()> {
    let statements = modorch_fedengine_ddl::Analyzer::parse(sql)?;
    if statements.len() != 1 {
        return Err(EngineError::unsupported(
            "exactly one read-only query is required",
        ));
    }
    if let ControlFlow::Break(error) = statements.visit(&mut ReadOnly) {
        return Err(error);
    }
    Ok(())
}
// Cancellation on every exit, including dropped HTTP futures and admission timeouts.
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) async fn execute(
    hub: &ExternalEventHub,
    database: &VirtualDatabase,
    sql: String,
    timeout: Duration,
    cancel: CancellationToken,
    row_limit: usize,
) -> Result<QueryResult> {
    read_only(&sql)?;
    let _guard = CancelOnDrop(cancel.clone());
    let started = Instant::now();
    let deadline = started + timeout;
    let session = SessionContext {
        database: database.config.name.clone(),
        database_oid: Some(database.oid),
        ..Default::default()
    };
    let mut request = QueryRequest {
        session,
        principal: database.config.owner.clone(),
        session_id: 0,
        sql,
        mode: QueryMode::Prepare {
            parameter_types: vec![],
        },
        deadline,
        cancel: cancel.clone(),
    };
    let prepared = send(hub, request.clone()).await?;
    let descriptor = match prepared.as_ref() {
        Ok(QueryReply::Prepared(d))
            if d.kind == StatementKind::Query && d.parameters.is_empty() =>
        {
            d.clone()
        }
        Ok(_) => {
            return Err(EngineError::unsupported(
                "only parameter-free read-only queries are supported",
            ));
        }
        Err(e) => return Err(e.clone()),
    };
    let expected = DatabaseVersion {
        oid: database.oid,
        revision: database.revision,
    };
    if descriptor.database_version != Some(expected) {
        return Err(EngineError::new(
            "40001",
            "database definition changed; reload the workspace",
        ));
    }
    request.mode = QueryMode::Execute {
        parameters: vec![],
        parameter_types: vec![],
        expected: descriptor.generations,
        expected_database: Some(expected),
    };
    let reply = send(hub, request).await?;
    let accepted = match reply.as_ref() {
        Ok(QueryReply::Accepted(a)) => a,
        Err(e) => return Err(e.clone()),
        _ => return Err(EngineError::new("XX000", "unexpected query reply")),
    };
    let mut stream = accepted.take_stream()?;
    let mut result = QueryResult {
        columns: accepted
            .columns
            .iter()
            .map(|c| ResultColumn {
                name: c.name.clone(),
                type_name: format!("{:?}", c.data_type),
                type_oid: c.data_type.oid(),
                type_modifier: c.type_modifier,
            })
            .collect(),
        notices: accepted.notices.iter().map(|(_, n)| n.clone()).collect(),
        ..Default::default()
    };
    let mut bytes = serde_json::to_vec(&result)
        .map_err(|_| EngineError::new("XX000", "result encoding failed"))?
        .len();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(EngineError::new("57014", "query canceled")),
            _ = tokio::time::sleep_until(deadline.into()) => return Err(EngineError::new("57014", "query timeout")),
            chunk = stream.receiver.recv() => chunk,
        };
        match chunk {
            Some(ResultChunk::Rows(rows)) => {
                for row in rows {
                    if result.rows.len() >= row_limit {
                        result.truncated = true;
                        break;
                    }
                    let size: usize = row.iter().map(cell_size).sum();
                    // Reserve JSON escaping and bytea expansion before allocating text values.
                    if size.saturating_mul(6).saturating_add(bytes) > BYTE_LIMIT {
                        result.truncated = true;
                        break;
                    }
                    let row = row
                        .iter()
                        .zip(&accepted.columns)
                        .map(|(cell, col)| {
                            modorch_fedengine_pg_compat::text_value(cell, col.data_type)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    bytes += serde_json::to_vec(&row)
                        .map_err(|_| EngineError::new("XX000", "result encoding failed"))?
                        .len();
                    result.rows.push(row);
                }
            }
            Some(ResultChunk::Complete(c)) => {
                result.notices.extend(c.notices.into_iter().map(|(_, n)| n));
                break;
            }
            Some(ResultChunk::Error(e)) => return Err(e),
            None => {
                return Err(EngineError::new(
                    "58000",
                    "query stream closed without completion",
                ));
            }
        }
        if result.truncated {
            break;
        }
    }
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}
fn cell_size(cell: &Cell) -> usize {
    match cell {
        Cell::Text(s) | Cell::Numeric(s) | Cell::NamedOid { name: s, .. } => s.len(),
        Cell::Bytes(b) => b.len().saturating_mul(2),
        Cell::Array { values, .. } => values
            .iter()
            .map(cell_size)
            .sum::<usize>()
            .saturating_add(values.len() * 4),
        _ => 64,
    }
}
async fn send(
    hub: &ExternalEventHub,
    r: QueryRequest,
) -> Result<std::sync::Arc<Result<QueryReply>>> {
    let timeout = r.deadline.saturating_duration_since(Instant::now());
    let cancel = r.cancel.clone();
    let hub_payload = r;
    let hub_options = modorch::MessageOptions {
        payload_bytes: Some(modorch_fedengine_core::HubPayloadBytes::hub_payload_bytes(
            &hub_payload,
        )),
        ..Default::default()
    };
    let ticket = hub
        .request::<_, Result<QueryReply>>(
            QUERY_TOPIC,
            Destination::Component(ComponentRef::module("query-coordinator")),
            hub_payload,
            hub_options,
        )
        .map_err(|_| EngineError::new("53300", "query admission unavailable"))?;
    tokio::select! {
        _ = cancel.cancelled() => Err(EngineError::new("57014", "query canceled")),
        reply = ticket.wait(timeout) => reply.map_err(|_| EngineError::new("57014", "query admission timeout")),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_ast_recursively() {
        for sql in [
            "SELECT 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT 'DELETE'",
            "VALUES (1)",
        ] {
            read_only(sql).unwrap();
        }
        for sql in [
            "DELETE FROM t",
            "SELECT 1; SELECT 2",
            "SET x=1",
            "SELECT * INTO t FROM s",
            "SELECT * FROM t FOR UPDATE",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
        ] {
            assert!(read_only(sql).is_err(), "{sql}");
        }
    }
}
