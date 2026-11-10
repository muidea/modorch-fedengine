//! Public clients use only the library Facade and a host-owned Application.
mod common;
use common::TestDirectory;
use modorch::{
    ApplicationBuilder, ApplicationHandle, ComponentSelection, ExecutionResources, HubLimits,
    ShutdownOptions, ShutdownReceipt,
};
use modorch_fedengine::*;
use std::time::Duration;

fn identity(name: &str) -> FedEnginePrincipal {
    FedEnginePrincipal::authenticated(name, "test-host/authenticated-session").unwrap()
}
async fn host() -> (
    TestDirectory,
    FedEngineRegisteredComponents,
    ApplicationHandle,
) {
    host_with_config(FedEngineConfig::load("config/application.toml").unwrap()).await
}
async fn host_with_config(
    mut c: FedEngineConfig,
) -> (
    TestDirectory,
    FedEngineRegisteredComponents,
    ApplicationHandle,
) {
    let directory = TestDirectory::new();
    c.duckdb.database_path = directory
        .path()
        .join("state.duckdb")
        .to_string_lossy()
        .into();
    c.duckdb.temp_directory = directory.path().join("tmp").to_string_lossy().into();
    c.duckdb.max_workers = 2;
    c.duckdb.threads = 1;
    c.duckdb.batch_rows = 2;
    c.duckdb.result_channel_batches = 1;
    c.management.listen = "127.0.0.1:0".into();
    c.users.push(
        serde_json::from_value(
            serde_json::json!({"name":"reader","password_secret_ref":"UNREAD_CLIENT_PASSWORD"}),
        )
        .unwrap(),
    );
    c.virtual_databases.push(
        serde_json::from_value(
            serde_json::json!({"name":"clients","owner":"fedengine","connect":[],"schemas":[]}),
        )
        .unwrap(),
    );
    let mut b = ApplicationBuilder::new("client-test-host");
    let r = FedEngineBundle::new(c)
        .profile(FedEngineProfile::ManagedServer { pgwire: false })
        .register(&mut b)
        .unwrap();
    let mut s = ComponentSelection::new();
    for id in r.initiator_ids() {
        s.enable_initiator(id);
    }
    for id in r.module_ids() {
        s.enable_module(id);
    }
    let d = r.resource_requirements();
    let app = b
        .selection(s)
        .message_limits(HubLimits {
            max_message_bytes: Some(1024 * 1024),
            max_in_flight_bytes: Some(16 * 1024 * 1024),
            max_in_flight_bytes_per_target: Some(8 * 1024 * 1024),
            max_waiting_bytes: Some(8 * 1024 * 1024),
            ..Default::default()
        })
        .execution_resources(ExecutionResources {
            blocking_io_threads: d.blocking_io_threads,
            blocking_io_capacity: d.blocking_io_capacity,
            max_tasks_per_owner: d.max_tasks_per_owner,
            ..Default::default()
        })
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    r.observe(&app).unwrap();
    (directory, r, app)
}
async fn stop(app: &ApplicationHandle) {
    assert!(matches!(
        app.shutdown_checked(ShutdownOptions {
            timeout: Duration::from_secs(10)
        })
        .await
        .unwrap(),
        ShutdownReceipt::GracefullyStopped
    ));
}
async fn rows(stream: &mut FedEngineResultStream) -> Vec<Vec<FedEngineValue>> {
    let mut rows = vec![];
    while let Some(chunk) = stream.next().await.unwrap() {
        match chunk {
            FedEngineResultChunk::Rows(batch) => rows.extend(batch),
            FedEngineResultChunk::Complete(c) => assert!(matches!(
                c.outcome,
                FedEngineExecutionOutcome::Completed | FedEngineExecutionOutcome::CommitConfirmed
            )),
        }
    }
    rows
}
fn query(
    r: &FedEngineRegisteredComponents,
    a: &ApplicationHandle,
    b: FedEngineClientBudget,
) -> FedEngineQueryClient {
    r.query_client(a, identity("fedengine"), b).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_clients_prepare_parameterize_stream_and_do_not_grant_admin() {
    let (_dir, r, a) = host().await;
    let q = query(&r, &a, Default::default());
    let token = FedEngineCancellation::new();
    let p = q
        .prepare(
            "clients",
            "SELECT $1::INTEGER AS value, $2::TEXT AS label",
            &[Some(FedEngineDataType::Int4), Some(FedEngineDataType::Text)],
            &token,
        )
        .await
        .unwrap();
    assert!(!token.is_cancelled());
    assert_eq!(
        p.parameters(),
        &[FedEngineDataType::Int4, FedEngineDataType::Text]
    );
    assert_eq!(p.columns()[0].name, "value");
    let mut result = q
        .execute(
            &p,
            vec![
                FedEngineValue::I32(42),
                FedEngineValue::Text("hello ' typed".into()),
            ],
            &FedEngineCancellation::new(),
        )
        .await
        .unwrap();
    let data = rows(&mut result).await;
    assert!(matches!(&data[0][0], FedEngineValue::I32(42)));
    assert!(matches!(&data[0][1],FedEngineValue::Text(s) if s=="hello ' typed"));
    let denied = r
        .management_client(
            &a,
            identity("fedengine"),
            Default::default(),
            Default::default(),
        )
        .unwrap();
    assert_eq!(
        denied.database("clients").await.err().unwrap().sqlstate,
        "42501"
    );
    let reader = r
        .query_client(&a, identity("reader"), Default::default())
        .unwrap();
    assert_eq!(
        reader
            .prepare("clients", "SELECT 1", &[], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "42501"
    );
    assert_eq!(
        q.prepare("_fedengine", "SELECT 1", &[], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "3D000"
    );
    assert_eq!(
        r.query_client(&a, identity("unknown"), Default::default())
            .err()
            .unwrap()
            .sqlstate,
        "42501"
    );
    stop(&a).await;
    assert_eq!(
        q.prepare("clients", "SELECT 1", &[], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "55000"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_input_output_and_clone_admission_budgets_are_enforced() {
    let (_dir, r, a) = host().await;
    let budget = FedEngineClientBudget {
        max_in_flight: 1,
        max_sql_bytes: 128,
        max_parameters: 1,
        max_result_rows: 1,
        max_result_bytes: 512,
        ..Default::default()
    };
    let q = query(&r, &a, budget);
    assert_eq!(
        q.prepare(
            "clients",
            &"x".repeat(129),
            &[],
            &FedEngineCancellation::new()
        )
        .await
        .err()
        .unwrap()
        .sqlstate,
        "22023"
    );
    assert_eq!(
        q.prepare(
            "clients",
            "SELECT $1::INTEGER, $2::INTEGER",
            &[],
            &FedEngineCancellation::new()
        )
        .await
        .err()
        .unwrap()
        .sqlstate,
        "54000"
    );
    let p = q
        .prepare(
            "clients",
            "SELECT i FROM range(5) t(i)",
            &[],
            &FedEngineCancellation::new(),
        )
        .await
        .unwrap();
    let mut stream = q
        .execute(&p, vec![], &FedEngineCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        q.clone()
            .prepare("clients", "SELECT 1", &[], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "53300"
    );
    assert_eq!(stream.next().await.err().unwrap().sqlstate, "54000");
    assert!(stream.next().await.unwrap().is_none());
    let p = q
        .prepare("clients", "SELECT 1", &[], &FedEngineCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        rows(
            &mut q
                .execute(&p, vec![], &FedEngineCancellation::new())
                .await
                .unwrap()
        )
        .await
        .len(),
        1
    );
    let other = query(&r, &a, Default::default());
    assert_eq!(
        other
            .execute(&p, vec![], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "42501"
    );
    let p = q
        .prepare(
            "clients",
            "SELECT $1::TEXT",
            &[Some(FedEngineDataType::Text)],
            &FedEngineCancellation::new(),
        )
        .await
        .unwrap();
    let mut stream = q
        .execute(
            &p,
            vec![FedEngineValue::Text("x".repeat(1024))],
            &FedEngineCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(stream.next().await.err().unwrap().sqlstate, "54000");
    let mut nested = FedEngineValue::I32(1);
    for _ in 0..65 {
        nested = FedEngineValue::Array {
            lower_bound: 1,
            values: vec![nested],
        };
    }
    assert_eq!(
        q.execute(&p, vec![nested], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "54000"
    );
    stop(&a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_cancel_drop_and_stream_deadline_release_only_after_real_drain() {
    let (_dir, r, a) = host().await;
    let q = query(
        &r,
        &a,
        FedEngineClientBudget {
            timeout: Duration::from_secs(1),
            ..Default::default()
        },
    );
    let canceled = FedEngineCancellation::new();
    canceled.cancel();
    assert_eq!(
        q.prepare("clients", "SELECT 1", &[], &canceled)
            .await
            .err()
            .unwrap()
            .sqlstate,
        "57014"
    );
    let p = q
        .prepare(
            "clients",
            "SELECT i FROM range(1000000000) t(i)",
            &[],
            &FedEngineCancellation::new(),
        )
        .await
        .unwrap();
    let cancel = FedEngineCancellation::new();
    let mut stream = q.execute(&p, vec![], &cancel).await.unwrap();
    cancel.cancel();
    assert_eq!(stream.next().await.err().unwrap().sqlstate, "57014");
    let mut stream = q
        .execute(&p, vec![], &FedEngineCancellation::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(stream.next().await.err().unwrap().sqlstate, "57014");
    let stream = q
        .execute(&p, vec![], &FedEngineCancellation::new())
        .await
        .unwrap();
    drop(stream);
    stop(&a).await;
    assert_eq!(r.handle().metrics().active_workers, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn management_clients_scope_cas_replay_and_pin_prepared_database_identity() {
    let (_dir, r, a) = host().await;
    let grant = FedEngineManagementGrant::default()
        .database("clients")
        .database("newdb")
        .allow_writes();
    let m = r
        .management_client(&a, identity("fedengine"), Default::default(), grant)
        .unwrap();
    let read = r
        .management_client(
            &a,
            identity("reader"),
            Default::default(),
            FedEngineManagementGrant::default().database("newdb"),
        )
        .unwrap();
    let q = query(&r, &a, Default::default());
    let p = q
        .prepare("clients", "SELECT 1", &[], &FedEngineCancellation::new())
        .await
        .unwrap();
    let old = m.database("clients").await.unwrap();
    let deletion = FedEngineDatabaseMutation::Delete {
        name: "clients".into(),
        target: FedEngineDatabaseTarget {
            oid: old.oid,
            expected_revision: old.revision,
        },
    };
    let receipt = m
        .mutate_database(deletion.clone(), "delete-old")
        .await
        .unwrap();
    assert_eq!(
        m.mutate_database(deletion, "delete-old").await.unwrap(),
        receipt
    );
    assert!(receipt.deleted);
    let definition: FedEngineDatabaseDefinition = serde_json::from_value(
        serde_json::json!({"name":"clients","owner":"fedengine","schemas":[]}),
    )
    .unwrap();
    let created = m
        .mutate_database(FedEngineDatabaseMutation::Create { definition }, "recreate")
        .await
        .unwrap();
    assert_ne!(created.oid, old.oid);
    assert_eq!(
        q.execute(&p, vec![], &FedEngineCancellation::new())
            .await
            .err()
            .unwrap()
            .sqlstate,
        "3D000"
    );
    let definition: FedEngineDatabaseDefinition = serde_json::from_value(
        serde_json::json!({"name":"newdb","owner":"fedengine","schemas":[]}),
    )
    .unwrap();
    assert_eq!(
        read.mutate_database(
            FedEngineDatabaseMutation::Create {
                definition: definition.clone()
            },
            "reader-create"
        )
        .await
        .err()
        .unwrap()
        .sqlstate,
        "42501"
    );
    let created = m
        .mutate_database(
            FedEngineDatabaseMutation::Create { definition },
            "create-new",
        )
        .await
        .unwrap();
    let db = read.database("newdb").await.unwrap();
    assert_eq!(db.oid, created.oid);
    let bad = FedEngineDatabaseMutation::Delete {
        name: "newdb".into(),
        target: FedEngineDatabaseTarget {
            oid: created.oid,
            expected_revision: 0,
        },
    };
    assert_eq!(
        m.mutate_database(bad, "stale-delete")
            .await
            .err()
            .unwrap()
            .sqlstate,
        "40001"
    );
    let mut changed = db.config;
    changed.schemas = vec![FedEngineSchemaMapping {
        name: "outside".into(),
        source: FedEngineSourceId("outside".into()),
        remote_schema: "public".into(),
    }];
    let mutation = FedEngineDatabaseMutation::Replace {
        name: "newdb".into(),
        target: FedEngineDatabaseTarget {
            oid: db.oid,
            expected_revision: db.revision,
        },
        definition: changed,
    };
    assert_eq!(
        m.mutate_database(mutation, "outside-map")
            .await
            .err()
            .unwrap()
            .sqlstate,
        "42501"
    );
    assert_eq!(
        m.source(FedEngineSourceId("outside".into()))
            .await
            .err()
            .unwrap()
            .sqlstate,
        "42501"
    );
    stop(&a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_binding_rejects_foreign_and_unselected_applications() {
    let (_dir, r, a) = host().await;
    let other = ApplicationBuilder::new("other")
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert_eq!(
        r.query_client(&other, identity("fedengine"), Default::default())
            .err()
            .unwrap()
            .sqlstate,
        "55000"
    );
    let directory = TestDirectory::new();
    let mut c = FedEngineConfig::load("config/application.toml").unwrap();
    c.duckdb.database_path = directory
        .path()
        .join("unused.duckdb")
        .to_string_lossy()
        .into();
    let mut builder = ApplicationBuilder::new("unselected");
    let unselected = FedEngineBundle::new(c).register(&mut builder).unwrap();
    let empty = builder.build().unwrap().start().await.unwrap();
    assert_eq!(
        unselected
            .query_client(&empty, identity("fedengine"), Default::default())
            .err()
            .unwrap()
            .sqlstate,
        "55000"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    stop(&empty).await;
    stop(&other).await;
    stop(&a).await;
}

async fn execute_sql(
    q: &FedEngineQueryClient,
    database: &str,
    sql: &str,
    parameters: Vec<FedEngineValue>,
    types: &[Option<FedEngineDataType>],
) -> (Vec<Vec<FedEngineValue>>, FedEngineCompletion) {
    let p = q
        .prepare(database, sql, types, &FedEngineCancellation::new())
        .await
        .unwrap_or_else(|e| panic!("prepare {database}: {sql}: {e:?}"));
    let mut stream = q
        .execute(&p, parameters, &FedEngineCancellation::new())
        .await
        .unwrap();
    let mut data = vec![];
    let mut completion = None;
    while let Some(chunk) = stream.next().await.unwrap() {
        match chunk {
            FedEngineResultChunk::Rows(rows) => data.extend(rows),
            FedEngineResultChunk::Complete(c) => completion = Some(c),
        }
    }
    (data, completion.expect("terminal receipt required"))
}
async fn refreshed(m: &FedEngineManagementClient, id: &str) {
    let view = m.source(FedEngineSourceId(id.into())).await.unwrap();
    let key = format!("client-refresh-{}-{}", id, view.source.desired_revision);
    let op = m
        .submit(
            FedEngineManagementAction::Refresh {
                id: FedEngineSourceId(id.into()),
                target: FedEngineSourceTarget {
                    incarnation: view.source.incarnation,
                    expected_revision: view.source.desired_revision,
                },
            },
            &key,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let op = m.operation(&op.id).await.unwrap();
            if op.status.finished() {
                assert_eq!(
                    op.status,
                    FedEngineOperationStatus::Succeeded,
                    "{} {:?}",
                    op.phase,
                    op.error
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires disposable PostgreSQL/MySQL and prepared extensions; run scripts/test-federation.sh"]
async fn public_clients_remote_policy_crud_ddl_management_and_unknown_write_drain() {
    let mut config = FedEngineConfig::load("examples/config/federation.toml").unwrap();
    config.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into(),
    );
    config.pgwire.allow_cross_database = true;
    config
        .datasources
        .iter_mut()
        .find(|s| s.id.0 == "crm")
        .unwrap()
        .allowed_schemas = vec!["public".into()];
    config.capabilities=serde_json::from_value(serde_json::json!([
        {"source":"crm","duckdb_version":"v2.0.0","extension_version":"c0f5a3c0c4","remote_version":"17.7","evidence":"tests/clients.rs disposable tuple","operations":["CREATE_TABLE","DROP_TABLE"],"index_adapter":false},
        {"source":"erp","duckdb_version":"v2.0.0","extension_version":"f7fc0d4271","remote_version":"5.7.44","evidence":"tests/clients.rs disposable tuple","operations":["CREATE_TABLE","DROP_TABLE"],"index_adapter":false}
    ])).unwrap();
    let (_dir, r, a) = host_with_config(config).await;
    let budget = FedEngineClientBudget {
        timeout: Duration::from_secs(10),
        ..Default::default()
    };
    let q = query(&r, &a, budget);
    let m = r
        .management_client(
            &a,
            identity("fedengine"),
            Default::default(),
            FedEngineManagementGrant::default()
                .source("crm")
                .source("erp")
                .allow_writes(),
        )
        .unwrap();
    for id in ["crm", "erp"] {
        let view = m.source(FedEngineSourceId(id.into())).await.unwrap();
        assert_eq!(
            view.source.status,
            FedEngineSourceStatus::Active,
            "source {id}: {:?}",
            view.source.last_error
        );
    }
    for (db, schema, id) in [("pg_crm", "public", "crm"), ("mysql_erp", "erp", "erp")] {
        let (_, complete) = execute_sql(
            &q,
            db,
            &format!("CREATE TABLE {db}.{schema}.fe12_client_items(id INTEGER, value INTEGER)"),
            vec![],
            &[],
        )
        .await;
        assert_eq!(complete.outcome, FedEngineExecutionOutcome::CommitConfirmed);
        refreshed(&m, id).await;
        let (_, complete) = execute_sql(
            &q,
            db,
            &format!("INSERT INTO {db}.{schema}.fe12_client_items VALUES($1::INTEGER,$2::INTEGER)"),
            vec![FedEngineValue::I32(1), FedEngineValue::I32(10)],
            &[Some(FedEngineDataType::Int4), Some(FedEngineDataType::Int4)],
        )
        .await;
        assert_eq!(complete.affected_rows, Some(1));
        assert_eq!(complete.outcome, FedEngineExecutionOutcome::CommitConfirmed);
        let (_, complete) = execute_sql(
            &q,
            db,
            &format!("UPDATE {db}.{schema}.fe12_client_items SET value=20 WHERE id=1"),
            vec![],
            &[],
        )
        .await;
        assert_eq!(complete.affected_rows, Some(1));
        let (data, _) = execute_sql(
            &q,
            db,
            &format!("SELECT value FROM {db}.{schema}.fe12_client_items WHERE id=$1::INTEGER"),
            vec![FedEngineValue::I32(1)],
            &[Some(FedEngineDataType::Int4)],
        )
        .await;
        assert!(matches!(data[0][0], FedEngineValue::I32(20)));
        let reader = r
            .query_client(&a, identity("reader"), Default::default())
            .unwrap();
        assert_eq!(
            reader
                .prepare(
                    db,
                    &format!("SELECT * FROM {db}.{schema}.fe12_client_items"),
                    &[],
                    &FedEngineCancellation::new()
                )
                .await
                .err()
                .unwrap()
                .sqlstate,
            "42501"
        );
    }
    // The fixture enables cross-database federation explicitly; it is not a default grant.
    let (joined,_)=execute_sql(&q,"pg_crm","SELECT p.id FROM pg_crm.public.fe12_client_items p JOIN mysql_erp.erp.fe12_client_items m ON p.id=m.id",vec![],&[]).await;
    assert_eq!(joined.len(), 1);
    assert_eq!(q.prepare("pg_crm","INSERT INTO pg_crm.public.fe12_client_items SELECT id,value FROM mysql_erp.erp.fe12_client_items",&[],&FedEngineCancellation::new()).await.err().unwrap().sqlstate,"42501");
    assert_eq!(
        q.prepare(
            "pg_crm",
            "ALTER TABLE pg_crm.public.fe12_client_items ADD COLUMN forbidden INTEGER",
            &[],
            &FedEngineCancellation::new()
        )
        .await
        .err()
        .unwrap()
        .sqlstate,
        "0A000"
    );
    let other = r
        .management_client(
            &a,
            identity("fedengine"),
            Default::default(),
            FedEngineManagementGrant::default().source("crm"),
        )
        .unwrap();
    let source = m.source(FedEngineSourceId("erp".into())).await.unwrap();
    let op = m
        .submit(
            FedEngineManagementAction::Probe {
                id: FedEngineSourceId("erp".into()),
                target: FedEngineSourceTarget {
                    incarnation: source.source.incarnation,
                    expected_revision: source.source.desired_revision,
                },
            },
            "probe-erp",
        )
        .await
        .unwrap();
    assert_eq!(
        other.operation(&op.id).await.err().unwrap().sqlstate,
        "42501"
    );
    for (db, schema) in [("pg_crm", "public"), ("mysql_erp", "erp")] {
        let (_, complete) = execute_sql(
            &q,
            db,
            &format!("DELETE FROM {db}.{schema}.fe12_client_items WHERE id=1"),
            vec![],
            &[],
        )
        .await;
        assert_eq!(complete.affected_rows, Some(1));
    }
    let (_, completion) = execute_sql(
        &q,
        "mysql_erp",
        "DROP TABLE mysql_erp.erp.fe12_client_items",
        vec![],
        &[],
    )
    .await;
    assert_eq!(
        completion.outcome,
        FedEngineExecutionOutcome::CommitConfirmed
    );
    // A non-transactional sequence proves the library never automatically replays a write.
    let (native, connection) = tokio_postgres::connect(
        &std::env::var("FEDENGINE_POSTGRES_DSN").unwrap(),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    let driver = tokio::spawn(connection);
    native.batch_execute("CREATE SEQUENCE public.fe12_attempts; CREATE FUNCTION public.fe12_block() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('public.fe12_attempts'); PERFORM set_config('lock_timeout','45s',true); PERFORM pg_advisory_xact_lock(12012012); RETURN NEW; END $$; CREATE TRIGGER fe12_gate BEFORE INSERT ON public.fe12_client_items FOR EACH ROW EXECUTE FUNCTION public.fe12_block()").await.unwrap();
    native
        .query_one("SELECT pg_advisory_lock(12012012)", &[])
        .await
        .unwrap();
    let p = q
        .prepare(
            "pg_crm",
            "INSERT INTO public.fe12_client_items VALUES(2,2)",
            &[],
            &FedEngineCancellation::new(),
        )
        .await
        .unwrap();
    let cancel = FedEngineCancellation::new();
    let mut stream = q.execute(&p, vec![], &cancel).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15),async{
        loop {let waiting:bool=native.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event='advisory' AND query LIKE '%fe12_client_items%')",&[]).await.unwrap().get(0);if waiting{break}tokio::time::sleep(Duration::from_millis(10)).await;}
    }).await.unwrap();
    cancel.cancel();
    let error = stream.next().await.err().unwrap();
    assert!(error.outcome_unknown);
    assert_eq!(error.sqlstate, "57014");
    assert!(matches!(
        a.shutdown_checked(ShutdownOptions {
            timeout: Duration::from_millis(50)
        })
        .await,
        Err(modorch::FrameworkError::StopIncomplete { .. })
    ));
    assert!(r.handle().metrics().active_workers > 0);
    native
        .query_one("SELECT pg_advisory_unlock(12012012)", &[])
        .await
        .unwrap();
    stop(&a).await;
    assert_eq!(r.handle().metrics().active_workers, 0);
    let attempts: i64 = native
        .query_one("SELECT last_value FROM public.fe12_attempts", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(attempts, 1);
    native.batch_execute("DROP TABLE public.fe12_client_items; DROP FUNCTION public.fe12_block(); DROP SEQUENCE public.fe12_attempts").await.unwrap();
    drop(native);
    driver.await.unwrap().unwrap();
}
