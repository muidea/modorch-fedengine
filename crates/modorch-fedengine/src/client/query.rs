//! Version-pinned SQL preparation and bounded result streaming.
use super::*;

/// Cloneable SQL capability; every statement goes through the existing typed
/// Query/Policy/DuckDB path. No mutable session, raw hub or native port is exposed.
#[derive(Clone)]
pub struct FedEngineQueryClient(pub(super) Arc<BoundClient>);
/// Opaque preparation bound to the same client and admitted database identity.
/// Source/database CAS versions are rechecked at execution; callers cannot edit them.
pub struct FedEnginePreparedQuery {
    owner: Arc<BoundClient>,
    descriptor: PreparedDescriptor,
    session: SessionContext,
}
impl FedEnginePreparedQuery {
    /// Resolved parameter type identities.
    pub fn parameters(&self) -> &[FedEngineDataType] {
        &self.descriptor.parameters
    }
    /// Bounded result description without executing a write.
    pub fn columns(&self) -> &[FedEngineColumn] {
        &self.descriptor.columns
    }
}
impl FedEngineQueryClient {
    /// Describe one SQL statement and pin the currently authorized database OID.
    /// Parameters use the same SQLSTATE/type/DDL checks as PGWire. Session commands
    /// and multiple statements are not supported by this stateless client.
    pub async fn prepare(
        &self,
        database: &str,
        sql: &str,
        parameter_types: &[Option<FedEngineDataType>],
        cancel: &FedEngineCancellation,
    ) -> Result<FedEnginePreparedQuery> {
        let _permit = self.0.admit()?;
        let mut guard = CancelOnDrop(Some(cancel.0.clone()));
        if sql.is_empty()
            || sql.len() > self.0.budget.max_sql_bytes
            || database.is_empty()
            || database.len() > 128
            || parameter_types.len() > self.0.budget.max_parameters
        {
            return Err(EngineError::config(
                "client SQL/database/parameter input budget exceeded",
            ));
        }
        let deadline = Instant::now() + self.0.budget.timeout;
        let headers = self
            .0
            .control::<_, CatalogReply>(
                CATALOG_TOPIC,
                "catalog-manager",
                CatalogRequest::Headers,
                0,
                deadline,
                Some(&cancel.0),
            )
            .await?;
        let headers = match headers.as_ref() {
            Ok(CatalogReply::Headers(h)) => h.clone(),
            Err(e) => return Err(e.clone()),
            _ => return Err(EngineError::new("XX000", "invalid catalog reply")),
        };
        // Initial CONNECT is evaluated with no OID. Only after admission do we pin it.
        let mut session = SessionContext {
            database: database.into(),
            ..Default::default()
        };
        let effective = headers
            .sources
            .iter()
            .filter_map(|h| h.applied.as_ref().map(|r| r.effective_definition()))
            .collect::<Vec<_>>();
        let scope = modorch_fedengine_policy::session_metadata_scope(
            &self.0.config,
            &self.0.identity.name,
            &session,
            headers,
            &effective,
        )?;
        session.database_oid = scope.virtual_database.as_ref().map(|d| d.oid).or_else(|| {
            scope
                .catalogs
                .iter()
                .find(|c| c.catalog == database)
                .map(|c| c.database_oid)
        });
        let request = QueryRequest {
            session: session.clone(),
            principal: self.0.identity.name.clone(),
            session_id: 0,
            sql: sql.into(),
            mode: QueryMode::Prepare {
                parameter_types: parameter_types.to_vec(),
            },
            deadline,
            cancel: cancel.0.clone(),
        };
        let reply = self
            .send(
                request,
                sql.len() + parameter_types.len() * std::mem::size_of::<PgType>(),
            )
            .await?;
        match reply.as_ref() {
            Ok(QueryReply::Prepared(d)) => {
                let bytes = d.columns.iter().fold(0usize, |n, c| {
                    n.saturating_add(c.name.len() + std::mem::size_of::<ColumnMeta>())
                });
                if bytes > self.0.budget.max_control_bytes
                    || d.parameters.len() > self.0.budget.max_parameters
                {
                    return Err(EngineError::new(
                        "54000",
                        "client description/parameter budget exceeded",
                    ));
                }
                guard.0.take();
                Ok(FedEnginePreparedQuery {
                    owner: self.0.clone(),
                    descriptor: d.clone(),
                    session,
                })
            }
            Err(e) => Err(e.clone()),
            _ => Err(EngineError::new("XX000", "invalid preparation reply")),
        }
    }
    async fn send(&self, request: QueryRequest, bytes: usize) -> Result<Arc<Result<QueryReply>>> {
        let deadline = request.deadline;
        let cancel = request.cancel.clone();
        self.0
            .control(
                QUERY_TOPIC,
                "query-coordinator",
                request,
                bytes,
                deadline,
                Some(&cancel),
            )
            .await
    }
    /// Execute an opaque preparation once per call. DML/DDL share the existing
    /// Policy, single-source and verified-version gates. Never retries a write.
    /// Dropping this future requests cancellation, as does dropping its stream.
    pub async fn execute(
        &self,
        prepared: &FedEnginePreparedQuery,
        parameters: Vec<FedEngineValue>,
        cancel: &FedEngineCancellation,
    ) -> Result<FedEngineResultStream> {
        let permit = self.0.admit()?;
        let mut guard = Some(CancelOnDrop(Some(cancel.0.clone())));
        if !Arc::ptr_eq(&self.0, &prepared.owner) {
            return Err(EngineError::denied(
                "prepared statement belongs to another client",
            ));
        }
        if parameters.len() != prepared.descriptor.parameters.len() {
            return Err(EngineError::new("08P01", "parameter count mismatch"));
        }
        let bytes = parameters
            .iter()
            .fold(prepared.descriptor.sql.len(), |n, v| {
                n.saturating_add(value_bytes(v))
            });
        if bytes > self.0.budget.max_control_bytes {
            return Err(EngineError::new(
                "54000",
                "client parameter budget exceeded",
            ));
        }
        let deadline = Instant::now() + self.0.budget.timeout;
        let request = QueryRequest {
            session: prepared.session.clone(),
            principal: self.0.identity.name.clone(),
            session_id: 0,
            sql: prepared.descriptor.sql.clone(),
            mode: QueryMode::Execute {
                parameters,
                parameter_types: prepared.descriptor.parameters.clone(),
                expected: prepared.descriptor.generations.clone(),
                expected_database: prepared.descriptor.database_version,
            },
            deadline,
            cancel: cancel.0.clone(),
        };
        let reply = self.send(request, bytes).await?;
        match reply.as_ref() {
            Ok(QueryReply::Accepted(a)) => {
                let stream = a.take_stream()?;
                let metadata_bytes = a
                    .columns
                    .iter()
                    .fold(0usize, |n, c| {
                        n.saturating_add(c.name.len())
                            .saturating_add(std::mem::size_of::<ColumnMeta>())
                    })
                    .saturating_add(a.notices.iter().fold(0usize, |n, (code, message)| {
                        n.saturating_add(code.len())
                            .saturating_add(message.len())
                            .saturating_add(std::mem::size_of::<(String, String)>())
                    }));
                if metadata_bytes > self.0.budget.max_control_bytes {
                    return Err(EngineError::new(
                        "54000",
                        "client execution description budget exceeded",
                    )
                    .unknown());
                }
                let result = FedEngineResultStream {
                    stream: Some(stream),
                    columns: a.columns.clone(),
                    notices: a.notices.clone(),
                    guard: guard.take(),
                    _permit: Some(permit),
                    bound: self.0.clone(),
                    deadline,
                    rows: 0,
                    bytes: 0,
                    terminal: false,
                    may_write: a.kind.writes(),
                };
                Ok(result)
            }
            Err(e) => Err(e.clone()),
            _ => Err(EngineError::new("XX000", "invalid execution reply")),
        }
    }
}
fn value_bytes(value: &Cell) -> usize {
    value_bytes_at(value, 0)
}
fn value_bytes_at(value: &Cell, depth: usize) -> usize {
    if depth >= 64 {
        return usize::MAX;
    }
    let extra = match value {
        Cell::Text(s) | Cell::Numeric(s) => s.len(),
        Cell::NamedOid { name, .. } => name.len(),
        Cell::Bytes(b) => b.len(),
        Cell::Array { values, .. } => values.iter().fold(0usize, |n, v| {
            n.saturating_add(value_bytes_at(v, depth + 1))
        }),
        _ => 0,
    };
    std::mem::size_of::<Cell>().saturating_add(extra)
}

/// One bounded row batch or terminal completion. Errors are returned by next().
pub enum FedEngineResultChunk {
    /// Owned neutral rows; cumulative budgets are checked before delivery.
    Rows(Vec<Vec<FedEngineValue>>),
    /// Terminal receipt; commit confirmation is preserved exactly.
    Complete(FedEngineCompletion),
}
/// Single-consumer result stream, backed by the configured bounded native channel.
/// Its permit and cancellation scope survive between polls. A missing terminal
/// receipt is an error; cancellation/drop never proves actual native work exited.
pub struct FedEngineResultStream {
    stream: Option<ExecutionStream>,
    columns: Vec<ColumnMeta>,
    notices: Vec<(String, String)>,
    guard: Option<CancelOnDrop>,
    _permit: Option<OwnedSemaphorePermit>,
    bound: Arc<BoundClient>,
    deadline: Instant,
    rows: usize,
    bytes: usize,
    terminal: bool,
    may_write: bool,
}
impl FedEngineResultStream {
    /// Result description from the execution admission.
    pub fn columns(&self) -> &[FedEngineColumn] {
        &self.columns
    }
    /// Initial diagnostics; terminal completion may contain further DDL notices.
    pub fn notices(&self) -> &[(String, String)] {
        &self.notices
    }
    /// Request cancellation without claiming rollback or resource release.
    pub fn cancel(&self) {
        if let Some(stream) = &self.stream {
            stream.cancel.cancel();
        }
    }
    fn finish(&mut self) {
        self.terminal = true;
        self.stream.take();
        self.guard.take();
        self._permit.take();
    }
    fn error(&mut self, error: EngineError) -> EngineError {
        let e = if self.may_write {
            error.unknown()
        } else {
            error
        };
        self.finish();
        e
    }
    /// Receive at most one batch. Deadline, cancellation or output budget failure
    /// terminates this receiver. Callers must still let the host drain real work.
    pub async fn next(&mut self) -> Result<Option<FedEngineResultChunk>> {
        if self.terminal {
            return Ok(None);
        }
        if Instant::now() >= self.deadline {
            return Err(self.error(EngineError::new(
                "57014",
                "client execution deadline exceeded",
            )));
        }
        if let Err(e) = self.bound.check() {
            return Err(self.error(e));
        }
        let stream = self.stream.as_mut().expect("active stream");
        let chunk = tokio::select! { biased; _=stream.cancel.cancelled()=>return Err(self.error(EngineError::new("57014","client execution canceled; native exit is not confirmed"))), r=tokio::time::timeout_at(tokio::time::Instant::from_std(self.deadline),stream.receiver.recv())=>r };
        match chunk {
            Ok(Some(ResultChunk::Rows(rows))) => {
                self.rows = self.rows.saturating_add(rows.len());
                self.bytes = rows.iter().fold(self.bytes, |n, row| {
                    row.iter().fold(
                        n.saturating_add(std::mem::size_of::<Vec<Cell>>()),
                        |n, v| n.saturating_add(value_bytes(v)),
                    )
                });
                if self.rows > self.bound.budget.max_result_rows
                    || self.bytes > self.bound.budget.max_result_bytes
                {
                    return Err(
                        self.error(EngineError::new("54000", "client result budget exceeded"))
                    );
                }
                Ok(Some(FedEngineResultChunk::Rows(rows)))
            }
            Ok(Some(ResultChunk::Complete(c))) => {
                self.finish();
                Ok(Some(FedEngineResultChunk::Complete(c)))
            }
            Ok(Some(ResultChunk::Error(e))) => {
                self.finish();
                Err(e)
            }
            Ok(None) => Err(self.error(EngineError::new(
                "58000",
                "execution stream ended without a terminal receipt",
            ))),
            Err(_) => Err(self.error(EngineError::new(
                "57014",
                "client execution deadline exceeded",
            ))),
        }
    }
}
