use super::{Backend, send};
use futures::{Sink, SinkExt};
use modorch::prelude::async_trait;
use modorch_fedengine_core::EngineError;
use modorch_fedengine_ddl::Analyzer;
use modorch_fedengine_pg_compat::{encode_row_in_timezone, fields, pg_error};
use pgwire::{
    api::{portal::Format, query::SimpleQueryHandler, results::Response, store::PortalStore, *},
    error::{PgWireError, PgWireResult},
    messages::{
        response::{EmptyQueryResponse, ReadyForQuery, TransactionStatus},
        simplequery::Query,
        *,
    },
};
use std::fmt::Debug;
#[async_trait]
impl SimpleQueryHandler for Backend {
    async fn on_query<C>(&self, client: &mut C, message: Query) -> PgWireResult<()>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let statements = Analyzer::split(&message.query).map_err(pg_error)?;
        client.set_state(PgWireConnectionState::QueryInProgress);
        if statements.is_empty() {
            client
                .send(PgWireBackendMessage::EmptyQueryResponse(
                    EmptyQueryResponse::new(),
                ))
                .await?;
        }
        for sql in statements {
            let mut cursor = self
                .cursor(client, sql, vec![], vec![], Default::default(), None)
                .await?;
            self.send_notices(client, &mut cursor).await?;
            if !cursor.columns.is_empty() {
                client
                    .send(PgWireBackendMessage::RowDescription(
                        pgwire::messages::data::RowDescription::new(
                            fields(&cursor.columns, &Format::UnifiedText)
                                .iter()
                                .map(Into::into)
                                .collect(),
                        ),
                    ))
                    .await?;
            }
            while let Some(row) = cursor.next().await? {
                send(
                    client,
                    PgWireBackendMessage::DataRow(
                        encode_row_in_timezone(
                            &row,
                            &cursor.columns,
                            &Format::UnifiedText,
                            &cursor.timezone,
                        )
                        .map_err(pg_error)?,
                    ),
                    self.config.pgwire.output_timeout_ms,
                )
                .await?;
            }
            self.send_notices(client, &mut cursor).await?;
            send(
                client,
                PgWireBackendMessage::CommandComplete(cursor.tag().into()),
                self.config.pgwire.output_timeout_ms,
            )
            .await?;
        }
        client.set_state(PgWireConnectionState::ReadyForQuery);
        client.set_transaction_status(TransactionStatus::Idle);
        client
            .send(PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                TransactionStatus::Idle,
            )))
            .await?;
        Ok(())
    }
    async fn do_query<C>(&self, _: &mut C, _: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Err(pg_error(EngineError::unsupported("use streaming on_query")))
    }
}
