//! Disposable PostgreSQL/MySQL integration. Never point the DSNs at business databases.
mod common;
#[path = "common/wire.rs"]
mod wire;
use common::TestDirectory;
use modorch::{Phase, ShutdownReceipt, StartupError};
use modorch_fedengine_core::*;
use modorch_fedengine_test_host::FedEngine;
use std::{collections::BTreeSet, time::Duration};
use tokio_postgres::NoTls;
#[tokio::test]
#[ignore = "requires isolated databases and extension downloads; run scripts/test-federation.sh"]
async fn remote_crud_federation_ddl_index_generation_and_zero_cross_source_effects() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("fedengine=info")
        .with_test_writer()
        .try_init();
    let temporary = TestDirectory::new();
    let mut config = Config::load("examples/config/federation.toml").unwrap();
    config.management.enabled = false;
    config.duckdb.database_path = ":memory:".into();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.users[0].password_secret_ref = "USER".into();
    config.duckdb.install_extensions = false;
    config.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    config.duckdb.max_workers = 4;
    config.duckdb.threads = 2;
    config.duckdb.temp_directory = temporary.path().to_string_lossy().into_owned();
    config.ddl.allow_drop_schema = true;
    config.datasources[1]
        .allowed_schemas
        .push("fedengine_test_ns".into());
    config.grants[1]
        .actions
        .extend(["CREATE_SCHEMA".into(), "DROP_SCHEMA".into()]);
    // Discover the exact tuple in a separate in-memory admin instance, then enable tested operations.
    let admin = duckdb::Connection::open_in_memory_with_flags(
        duckdb::Config::default()
            .with("allow_unsigned_extensions", "true")
            .unwrap(),
    )
    .unwrap();
    admin
        .execute_batch(&format!(
            "SET extension_directory={}; LOAD postgres; LOAD mysql",
            quote_literal(config.duckdb.extension_directory.as_ref().unwrap())
        ))
        .unwrap();
    for source in &config.datasources {
        let ext = source.kind.extension();
        let dsn = std::env::var(&source.secret_ref).expect("isolated test DSN required");
        admin
            .execute_batch(&format!(
                "ATTACH {} AS {} (TYPE {})",
                quote_literal(&dsn),
                quote_identifier(&source.catalog_alias),
                ext
            ))
            .unwrap();
        admin
            .execute_batch(&format!(
                "DROP TABLE IF EXISTS {}.{}.fedengine_test_items",
                quote_identifier(&source.catalog_alias),
                quote_identifier(&source.default_schema)
            ))
            .unwrap();
        let extension_version: String = admin
            .query_row(
                "SELECT extension_version FROM duckdb_extensions() WHERE extension_name=?",
                [format!("{ext}_scanner")],
                |r| r.get(0),
            )
            .unwrap();
        let version_sql = if source.kind == DataSourceKind::Postgres {
            "SELECT current_setting('server_version') AS version"
        } else {
            "SELECT VERSION() AS version"
        };
        let remote_version: String = admin
            .query_row(
                &format!(
                    "SELECT version FROM {ext}_query({}, {})",
                    quote_literal(&source.catalog_alias),
                    quote_literal(version_sql)
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        eprintln!(
            "verified tuple candidate: source={} duckdb=v2.0.0 extension={} remote={}",
            source.id.0, extension_version, remote_version
        );
        config.capabilities.push(CapabilityRecord {
            source: source.id.clone(),
            duckdb_version: "v2.0.0".into(),
            extension_version,
            remote_version,
            evidence: "tests/federation.rs disposable-database execution".into(),
            operations: BTreeSet::from_iter(
                [
                    "CREATE_SCHEMA",
                    "DROP_SCHEMA",
                    "CREATE_TABLE",
                    "CREATE_TABLE_AS",
                    "ALTER_TABLE",
                    "ALTER_TABLE_ADD_COLUMN",
                    "ALTER_TABLE_DROP_COLUMN",
                    "ALTER_TABLE_RENAME_TABLE",
                    "ALTER_TABLE_RENAME_COLUMN",
                    "ALTER_TABLE_COLUMN_TYPE",
                    "ALTER_TABLE_COLUMN_DEFAULT",
                    "ALTER_TABLE_COLUMN_NULLABILITY",
                    "ALTER_TABLE_ADD_CONSTRAINT",
                    "ALTER_TABLE_DROP_CONSTRAINT",
                    "CREATE_INDEX_BTREE",
                    "CREATE_INDEX_UNIQUE_BTREE",
                    "DROP_TABLE",
                    "CREATE_INDEX",
                    "DROP_INDEX",
                ]
                .map(String::from),
            ),
            index_adapter: true,
        });
    }
    // A capability record is an admission gate, not permission to probe DDL.
    let mut missing = config.clone();
    missing.capabilities.clear();
    let unverified = FedEngine::start(missing).await.unwrap();
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(unverified.address().unwrap().port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname("pg_crm")
        .connect(NoTls)
        .await
        .unwrap();
    let driver = tokio::spawn(connection);
    for source in &config.datasources {
        let error = client
            .simple_query(&format!(
                "CREATE TABLE {}.{}.fedengine_capability_denied(id INTEGER)",
                quote_identifier(&source.catalog_alias),
                quote_identifier(&source.default_schema)
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code().unwrap().code(), "0A000");
        let native_sql = format!(
            "SELECT table_name FROM information_schema.tables WHERE table_schema={} AND table_name='fedengine_capability_denied'",
            quote_literal(&source.default_schema)
        );
        let count: i64 = admin
            .query_row(
                &format!(
                    "SELECT count(*) FROM {}_query({}, {})",
                    source.kind.extension(),
                    quote_literal(&source.catalog_alias),
                    quote_literal(&native_sql)
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "unverified DDL must have no remote effect");
    }
    drop(client);
    driver.await.unwrap().unwrap();
    assert!(matches!(
        unverified.shutdown_until_drained().await.unwrap(),
        ShutdownReceipt::GracefullyStopped
    ));
    assert_eq!(unverified.handle.task_metrics().tasks, 0);
    drop(unverified);

    // Each remote's tuple is checked against the actual source before Ready.
    for index in 0..config.capabilities.len() {
        let mut mismatched = config.clone();
        mismatched.capabilities[index].remote_version = "unverified-fixture-version".into();
        let error = match FedEngine::start(mismatched).await {
            Ok(engine) => {
                engine.shutdown_until_drained().await.unwrap();
                panic!("a mismatched remote capability tuple was accepted");
            }
            Err(error) => error.downcast::<StartupError>().unwrap(),
        };
        assert_eq!(error.source.phase(), Some(Phase::Setup));
        assert!(error.source.to_string().contains("version does not match"));
        assert!(matches!(
            error.cleanup,
            Ok(ShutdownReceipt::GracefullyStopped)
        ));
        assert_eq!(error.handle.task_metrics().tasks, 0);
    }
    let mut virtual_config = config.clone();
    let engine = FedEngine::start(config).await.unwrap();
    let mut connection_config = tokio_postgres::Config::new();
    connection_config
        .host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .dbname("pg_crm")
        .password(std::env::var("USER").unwrap());
    let (client, connection) = connection_config.connect(NoTls).await.unwrap();
    let driver = tokio::spawn(async move { connection.await.unwrap() });
    // Regression: remote catalog attachment changes DuckDB's positional macro
    // overload binding. Explicit casts and binary parameters must still divide.
    assert_eq!(
        client
            .query_one("SELECT 5::integer/2::integer", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        2
    );
    assert_eq!(
        client
            .query_one("SELECT $1::bigint/$2::bigint", &[&5i64, &2i64])
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        client
            .query_one("SELECT 5::real/2::real", &[])
            .await
            .unwrap()
            .get::<_, f32>(0),
        2.5
    );
    let error = client
        .simple_query("SELECT 1::integer/0::integer")
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "22012");
    let databases: Vec<String> = client
        .query(
            "SELECT datname FROM pg_catalog.pg_database WHERE datallowconn ORDER BY datname",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(databases, ["mysql_erp", "pg_crm"]);
    let namespaces: Vec<String> = client
        .query(
            "SELECT nspname FROM pg_catalog.pg_namespace ORDER BY nspname",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(namespaces, ["information_schema", "pg_catalog", "public"]);
    for sql in [
        "SELECT * FROM fedengine_metadata.catalog_status",
        "SELECT * FROM _fedengine.meta_pg_database",
        "SELECT * FROM pg_crm._fedengine.meta_pg_database",
    ] {
        assert!(client.simple_query(sql).await.is_err(), "{sql}");
    }
    for (alias, schema) in [("pg_crm", "public"), ("mysql_erp", "erp")] {
        let table = format!("{alias}.{schema}.fedengine_test_items");
        client
            .simple_query(&format!(
                "CREATE TABLE {table}(id INTEGER PRIMARY KEY, name VARCHAR, amount INTEGER)"
            ))
            .await
            .unwrap();
        let insert = client
            .prepare_typed(
                &format!("INSERT INTO {table} VALUES($1,$2,$3)"),
                &[
                    tokio_postgres::types::Type::INT4,
                    tokio_postgres::types::Type::TEXT,
                    tokio_postgres::types::Type::INT4,
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            client
                .execute(&insert, &[&1i32, &"Alice", &10i32])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            client
                .execute(&insert, &[&2i32, &"Bob", &20i32])
                .await
                .unwrap(),
            1
        );
        // Reject the entire Simple Query message before its first write runs.
        let error = client
            .simple_query(&format!(
                "INSERT INTO {table} VALUES(99,'must-not-be-written',99); SELECT 1"
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code().unwrap().code(), "0A000");
        assert_eq!(
            client
                .query_one(&format!("SELECT count(*)::BIGINT FROM {table}"), &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            2
        );
        assert_eq!(
            client
                .execute(&format!("UPDATE {table} SET amount=30 WHERE id=1"), &[])
                .await
                .unwrap(),
            1
        );
        let row = client
            .query_one(&format!("SELECT name,amount FROM {table} WHERE id=1"), &[])
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "Alice");
        assert_eq!(row.get::<_, i32>(1), 30);
        let prepared = client
            .prepare(&format!("SELECT * FROM {table}"))
            .await
            .unwrap();
        client
            .simple_query(&format!("ALTER TABLE {table} ADD COLUMN note VARCHAR(255)"))
            .await
            .unwrap();
        assert_eq!(
            client
                .query(&prepared, &[])
                .await
                .unwrap_err()
                .code()
                .unwrap()
                .code(),
            "0A000"
        );
        client
            .simple_query(&format!("ALTER TABLE {table} DROP COLUMN note"))
            .await
            .unwrap();
        client
            .simple_query(&format!(
                "CREATE INDEX fedengine_test_amount_idx ON {table}(amount)"
            ))
            .await
            .unwrap();
        // Verify a real remote index, not a DuckDB-local catalog artifact.
        let metadata = if alias == "pg_crm" {
            "SELECT count(*) FROM postgres_query('pg_crm', 'SELECT indexname FROM pg_indexes WHERE indexname=''fedengine_test_amount_idx''')"
        } else {
            "SELECT count(*) FROM mysql_query('mysql_erp', 'SELECT index_name FROM information_schema.statistics WHERE table_schema=''erp'' AND table_name=''fedengine_test_items'' AND index_name=''fedengine_test_amount_idx''')"
        };
        assert_eq!(
            admin
                .query_row(metadata, [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        client
            .simple_query(&format!(
                "DROP INDEX {alias}.{schema}.fedengine_test_amount_idx"
            ))
            .await
            .unwrap();
        assert_eq!(
            admin
                .query_row(metadata, [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        client
            .simple_query(&format!(
                "CREATE UNIQUE INDEX fedengine_unique_amount_idx ON {table}(amount)"
            ))
            .await
            .unwrap();
        assert!(
            client
                .execute(
                    &format!("INSERT INTO {table}(id,name,amount) VALUES(3,'Eve',30)"),
                    &[]
                )
                .await
                .is_err()
        );
        assert_eq!(
            client
                .query_one(&format!("SELECT count(*)::BIGINT FROM {table}"), &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            2
        );
        if alias == "pg_crm" {
            client
                .simple_query("DROP INDEX pg_crm.public.fedengine_unique_amount_idx")
                .await
                .unwrap();
        } else {
            assert!(
                client
                    .simple_query("DROP INDEX mysql_erp.erp.fedengine_unique_amount_idx")
                    .await
                    .is_err()
            );
        }
        let no_key = format!("{alias}.{schema}.fedengine_no_key");
        client
            .simple_query(&format!("CREATE TABLE {no_key}(value INTEGER)"))
            .await
            .unwrap();
        assert_eq!(
            client
                .execute(
                    &format!("INSERT INTO {no_key} VALUES(NULL),(NULL),(1)"),
                    &[]
                )
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            client
                .execute(
                    &format!("UPDATE {no_key} SET value=7 WHERE value IS NULL"),
                    &[]
                )
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            client
                .execute(&format!("DELETE FROM {no_key} WHERE value=7"), &[])
                .await
                .unwrap(),
            2
        );
        client
            .simple_query(&format!("DROP TABLE {no_key}"))
            .await
            .unwrap();
        assert_eq!(
            client
                .execute(&format!("DELETE FROM {table} WHERE id=2"), &[])
                .await
                .unwrap(),
            1
        );
    }
    let rows=client.query("SELECT a.name,b.amount FROM pg_crm.public.fedengine_test_items a JOIN mysql_erp.erp.fedengine_test_items b ON a.id=b.id",&[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "Alice");
    // Exercise a real CancelRequest on an execution involving both remote sources.
    // This does not simulate a blocked extension network call.
    let cancel = client.cancel_token();
    {
        let query = client.simple_query(
            "SELECT sum(a.amount::BIGINT + r.i*s.j) \
             FROM pg_crm.public.fedengine_test_items a \
             JOIN mysql_erp.erp.fedengine_test_items b ON a.id=b.id \
             CROSS JOIN range(1000000000) r(i) CROSS JOIN range(1000000000) s(j)",
        );
        tokio::pin!(query);
        tokio::select! {
            result = &mut query => panic!("remote-source query ended before cancellation: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        cancel.cancel_query(NoTls).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), query)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code().unwrap().code(), "57014");
    }
    assert_eq!(
        client
            .query_one(
                "SELECT count(*)::BIGINT FROM pg_crm.public.fedengine_test_items a \
                 JOIN mysql_erp.erp.fedengine_test_items b ON a.id=b.id",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    // Suspend and resume a named Portal over the same cross-source JOIN, then
    // Close/Sync with one row still unread rather than consuming its result.
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut portal = wire::raw_start(engine.address().unwrap(), "pg_crm").await;
        wire::packet(
            &mut portal,
            b'P',
            b"join_stmt\0SELECT r.i::INTEGER \
              FROM pg_crm.public.fedengine_test_items a \
              JOIN mysql_erp.erp.fedengine_test_items b ON a.id=b.id \
              CROSS JOIN range(3) r(i) ORDER BY r.i\0\0\0",
        )
        .await;
        wire::packet(&mut portal, b'B', b"join_portal\0join_stmt\0\0\0\0\0\0\0").await;
        let mut execute = b"join_portal\0".to_vec();
        execute.extend_from_slice(&1i32.to_be_bytes());
        for expected in ["0", "1"] {
            wire::packet(&mut portal, b'E', &execute).await;
            wire::packet(&mut portal, b'H', &[]).await;
            let (rows, boundary) = wire::until_boundary(&mut portal).await;
            assert_eq!(rows, [expected]);
            assert_eq!(boundary, b's');
        }
        wire::packet(&mut portal, b'C', b"Pjoin_portal\0").await;
        wire::packet(&mut portal, b'S', &[]).await;
        assert_eq!(wire::message(&mut portal).await.0, b'3');
        assert_eq!(wire::message(&mut portal).await.0, b'Z');
    })
    .await
    .unwrap();
    for sql in [
        "INSERT INTO pg_crm.public.fedengine_test_items(id,name,amount) SELECT id,name,amount FROM mysql_erp.erp.fedengine_test_items",
        "CREATE TABLE pg_crm.public.cross_source_must_not_exist AS SELECT * FROM mysql_erp.erp.fedengine_test_items",
        "CREATE TABLE pg_crm.public.cross_source_must_not_exist(id INTEGER REFERENCES mysql_erp.erp.fedengine_test_items(id))",
    ] {
        assert!(client.simple_query(sql).await.is_err());
    }
    assert!(
        client
            .query(
                "SELECT * FROM pg_crm.public.cross_source_must_not_exist",
                &[]
            )
            .await
            .is_err()
    );
    client.simple_query("CREATE TABLE pg_crm.public.fedengine_copy AS SELECT * FROM pg_crm.public.fedengine_test_items").await.unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT count(*)::BIGINT FROM pg_crm.public.fedengine_copy",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    client
        .simple_query("DROP TABLE pg_crm.public.fedengine_copy")
        .await
        .unwrap();

    // Verify v6 constraints, conditional DDL, SET/DROP NOT NULL, param reuse/reorder,
    // expanded types (boolean/time/bytea/uuid), and UPDATE/DELETE RETURNING live on PG 17.7 & MySQL 5.7.
    let v6_time = chrono::NaiveTime::from_hms_opt(12, 34, 56).unwrap();
    let v6_bytes = b"\x01\x02\xab\xff".to_vec();
    let v6_uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let v6_param_types = [
        tokio_postgres::types::Type::TEXT,
        tokio_postgres::types::Type::INT4,
        tokio_postgres::types::Type::INT4,
        tokio_postgres::types::Type::BOOL,
        tokio_postgres::types::Type::TIME,
        tokio_postgres::types::Type::BYTEA,
        tokio_postgres::types::Type::UUID,
    ];

    client
        .simple_query("CREATE TABLE pg_crm.public.fedengine_v6_parent(id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    for _ in 0..2 {
        client
            .simple_query(
                "CREATE TABLE IF NOT EXISTS pg_crm.public.fedengine_v6_child(\
                 id INTEGER PRIMARY KEY, \
                 code VARCHAR(32) NOT NULL UNIQUE DEFAULT 'dflt', \
                 parent_id INTEGER, \
                 qty INTEGER NOT NULL DEFAULT 1, \
                 flag BOOLEAN, \
                 at_t TIME, \
                 raw_b BYTEA, \
                 uid UUID, \
                 FOREIGN KEY(parent_id) REFERENCES pg_crm.public.fedengine_v6_parent(id), \
                 CHECK(qty > 0))",
            )
            .await
            .unwrap();
        client
            .simple_query(
                "ALTER TABLE pg_crm.public.fedengine_v6_child ADD COLUMN IF NOT EXISTS note VARCHAR(64) NOT NULL DEFAULT 'n1'",
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        client
            .simple_query("ALTER TABLE pg_crm.public.fedengine_v6_child DROP COLUMN IF EXISTS note")
            .await
            .unwrap();
    }
    client
        .execute(
            "INSERT INTO pg_crm.public.fedengine_v6_parent VALUES(10)",
            &[],
        )
        .await
        .unwrap();
    assert!(
        client
            .execute(
                "INSERT INTO pg_crm.public.fedengine_v6_child(id, code, parent_id, qty) VALUES(1, 'bad_chk', 10, 0)",
                &[]
            )
            .await
            .is_err()
    );
    assert!(
        client
            .execute(
                "INSERT INTO pg_crm.public.fedengine_v6_child(id, code, parent_id, qty) VALUES(2, 'bad_fk', 999, 1)",
                &[]
            )
            .await
            .is_err()
    );
    let pg_ins = client
        .prepare_typed(
            "INSERT INTO pg_crm.public.fedengine_v6_child(id, code, parent_id, qty, flag, at_t, raw_b, uid) \
             VALUES($2, $1, $2, $3, $4, $5, $6, $7)",
            &v6_param_types,
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .execute(
                &pg_ins,
                &[
                    &"reused", &10i32, &5i32, &true, &v6_time, &v6_bytes, &v6_uuid
                ],
            )
            .await
            .unwrap(),
        1
    );
    let pg_upd_ret = client
        .prepare_typed(
            "UPDATE pg_crm.public.fedengine_v6_child SET qty=$1 WHERE id=$2 RETURNING id",
            &[
                tokio_postgres::types::Type::INT4,
                tokio_postgres::types::Type::INT4,
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .query_one(&pg_upd_ret, &[&9i32, &10i32])
            .await
            .unwrap()
            .get::<_, i32>(0),
        10
    );
    let pg_del_ret = client
        .prepare_typed(
            "DELETE FROM pg_crm.public.fedengine_v6_child WHERE id=$1 RETURNING id",
            &[tokio_postgres::types::Type::INT4],
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .query_one(&pg_del_ret, &[&10i32])
            .await
            .unwrap()
            .get::<_, i32>(0),
        10
    );
    for _ in 0..2 {
        client
            .simple_query(
                "CREATE INDEX IF NOT EXISTS fedengine_v6_idx ON pg_crm.public.fedengine_v6_child(qty)",
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        client
            .simple_query("DROP INDEX IF EXISTS pg_crm.public.fedengine_v6_idx")
            .await
            .unwrap();
    }
    for _ in 0..2 {
        client
            .simple_query("DROP TABLE IF EXISTS pg_crm.public.fedengine_v6_child")
            .await
            .unwrap();
        client
            .simple_query("DROP TABLE IF EXISTS pg_crm.public.fedengine_v6_parent")
            .await
            .unwrap();
    }

    assert!(
        client
            .simple_query(
                "CREATE TABLE mysql_erp.erp.fedengine_v6_chk(qty INTEGER, CHECK(qty > 0))"
            )
            .await
            .is_err()
    );
    client
        .simple_query("CREATE TABLE mysql_erp.erp.fedengine_v6_parent(id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    for _ in 0..2 {
        client
            .simple_query(
                "CREATE TABLE IF NOT EXISTS mysql_erp.erp.fedengine_v6_child(\
                 id INTEGER PRIMARY KEY, \
                 code VARCHAR(32) NOT NULL UNIQUE DEFAULT 'dflt', \
                 parent_id INTEGER, \
                 qty INTEGER NOT NULL DEFAULT 1, \
                 flag BOOLEAN, \
                 at_t TIME, \
                 raw_b BYTEA, \
                 uid UUID, \
                 FOREIGN KEY(parent_id) REFERENCES mysql_erp.erp.fedengine_v6_parent(id))",
            )
            .await
            .unwrap();
        client
            .simple_query(
                "ALTER TABLE mysql_erp.erp.fedengine_v6_child ADD COLUMN IF NOT EXISTS note VARCHAR(64) NOT NULL DEFAULT 'm1'",
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        client
            .simple_query("ALTER TABLE mysql_erp.erp.fedengine_v6_child DROP COLUMN IF EXISTS note")
            .await
            .unwrap();
    }
    client
        .execute(
            "INSERT INTO mysql_erp.erp.fedengine_v6_parent VALUES(10)",
            &[],
        )
        .await
        .unwrap();
    let my_ins = client
        .prepare_typed(
            "INSERT INTO mysql_erp.erp.fedengine_v6_child(id, code, parent_id, qty, flag, at_t, raw_b, uid) \
             VALUES($2, $1, $2, $3, $4, $5, $6, $7)",
            &v6_param_types,
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .execute(
                &my_ins,
                &[
                    &"reused", &10i32, &5i32, &true, &v6_time, &v6_bytes, &v6_uuid
                ],
            )
            .await
            .unwrap(),
        1
    );
    let my_row = client
        .query_one(
            "SELECT code, parent_id::INTEGER, qty::INTEGER, flag, at_t::VARCHAR, raw_b, uid::VARCHAR \
             FROM mysql_erp.erp.fedengine_v6_child WHERE id=10",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(my_row.get::<_, String>(0), "reused");
    assert_eq!(my_row.get::<_, i32>(1), 10);
    assert_eq!(my_row.get::<_, i32>(2), 5);
    assert!(my_row.get::<_, bool>(3));
    assert!(my_row.get::<_, String>(4).starts_with("12:34:56"));
    assert_eq!(my_row.get::<_, Vec<u8>>(5), v6_bytes);
    assert_eq!(
        my_row.get::<_, String>(6).to_ascii_lowercase(),
        v6_uuid.to_string()
    );
    let my_upd = client
        .prepare_typed(
            "UPDATE mysql_erp.erp.fedengine_v6_child SET code=$2 WHERE id=$1 AND parent_id=$1",
            &[
                tokio_postgres::types::Type::INT4,
                tokio_postgres::types::Type::TEXT,
            ],
        )
        .await
        .unwrap();
    assert_eq!(client.execute(&my_upd, &[&10i32, &"upd"]).await.unwrap(), 1);
    assert_eq!(
        client
            .query_one(
                "SELECT code FROM mysql_erp.erp.fedengine_v6_child WHERE id=10",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "upd"
    );
    client
        .simple_query(
            "ALTER TABLE mysql_erp.erp.fedengine_v6_child ALTER COLUMN code DROP NOT NULL",
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .execute(
                "INSERT INTO mysql_erp.erp.fedengine_v6_child(id, code, parent_id, qty) VALUES(11, NULL, 10, 1)",
                &[]
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        client
            .execute(
                "UPDATE mysql_erp.erp.fedengine_v6_child SET code='k11' WHERE id=11",
                &[]
            )
            .await
            .unwrap(),
        1
    );
    client
        .simple_query("ALTER TABLE mysql_erp.erp.fedengine_v6_child ALTER COLUMN code SET NOT NULL")
        .await
        .unwrap();
    assert!(
        client
            .execute(
                "INSERT INTO mysql_erp.erp.fedengine_v6_child(id, code, parent_id, qty) VALUES(12, NULL, 10, 1)",
                &[]
            )
            .await
            .is_err()
    );
    assert_eq!(
        client
            .execute(
                "INSERT INTO mysql_erp.erp.fedengine_v6_child(id, parent_id) VALUES(12, 10)",
                &[]
            )
            .await
            .unwrap(),
        1
    );
    let dflt_row = client
        .query_one(
            "SELECT code, qty::INTEGER FROM mysql_erp.erp.fedengine_v6_child WHERE id=12",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(dflt_row.get::<_, String>(0), "dflt");
    assert_eq!(dflt_row.get::<_, i32>(1), 1);
    for _ in 0..2 {
        client
            .simple_query(
                "CREATE INDEX IF NOT EXISTS fedengine_v6_idx ON mysql_erp.erp.fedengine_v6_child(qty)",
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        client
            .simple_query("DROP INDEX IF EXISTS mysql_erp.erp.fedengine_v6_idx")
            .await
            .unwrap();
    }
    for _ in 0..2 {
        client
            .simple_query("DROP TABLE IF EXISTS mysql_erp.erp.fedengine_v6_child")
            .await
            .unwrap();
        client
            .simple_query("DROP TABLE IF EXISTS mysql_erp.erp.fedengine_v6_parent")
            .await
            .unwrap();
    }
    client
        .simple_query("CREATE SCHEMA mysql_erp.fedengine_test_ns")
        .await
        .unwrap();
    client
        .simple_query("DROP SCHEMA mysql_erp.fedengine_test_ns")
        .await
        .unwrap();
    client
        .simple_query("CREATE SCHEMA pg_crm.sales")
        .await
        .unwrap();
    client
        .simple_query("DROP SCHEMA pg_crm.sales")
        .await
        .unwrap();
    assert!(
        client
            .simple_query("CREATE SCHEMA mysql_erp.unapproved_namespace")
            .await
            .is_err()
    );
    for table in [
        "pg_crm.public.fedengine_test_items",
        "mysql_erp.erp.fedengine_test_items",
    ] {
        client
            .simple_query(&format!("DROP TABLE {table}"))
            .await
            .unwrap();
    }
    drop(client);
    driver.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), engine.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(engine.handle.health().pending.operations, 0);
    assert_eq!(engine.handle.task_metrics().tasks, 0);
    let metrics = engine.metrics();
    assert_eq!(metrics.active_workers, 0);
    assert_eq!(metrics.active_sessions, 0);
    assert_eq!(metrics.metadata_stale, 0);
    assert!(metrics.ddl_confirmed >= 20);
    // Guarded CRUD proves rollback on the PG/MySQL unique-constraint violations above;
    // indeterminate unknown-write drain is verified in tests/clients.rs.
    assert_eq!(metrics.unknown_writes, 0);
    assert_eq!(metrics.admitted, metrics.completed + metrics.failed);

    // Verify virtual database exposes ordinary tables (relkind='r') and allows authorized DML/DDL.
    admin
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS pg_crm.public.fedengine_vdb_seed(id INTEGER PRIMARY KEY, name VARCHAR(32) NOT NULL);
             CREATE TABLE IF NOT EXISTS mysql_erp.erp.fedengine_vdb_seed(id INTEGER PRIMARY KEY, name VARCHAR(32) NOT NULL);",
        )
        .unwrap();
    std::fs::create_dir_all(temporary.path().join("secrets")).unwrap();
    virtual_config.management.enabled = true;
    virtual_config.management.listen = "127.0.0.1:0".into();
    virtual_config.management.credentials_directory = temporary
        .path()
        .join("secrets")
        .to_string_lossy()
        .into_owned();
    virtual_config.duckdb.database_path = temporary
        .path()
        .join("vdb.duckdb")
        .to_string_lossy()
        .into_owned();
    virtual_config.datasources[0].allowed_schemas = vec!["public".into()];
    virtual_config.datasources[1].allowed_schemas = vec!["erp".into()];
    virtual_config.virtual_databases = vec![VirtualDatabaseConfig {
        name: "fedengine".into(),
        owner: "fedengine".into(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![
            VirtualSchemaConfig {
                name: "crm".into(),
                source: DataSourceId("crm".into()),
                remote_schema: "public".into(),
            },
            VirtualSchemaConfig {
                name: "erp".into(),
                source: DataSourceId("erp".into()),
                remote_schema: "erp".into(),
            },
        ],
    }];
    let vdb_engine = FedEngine::start(virtual_config.clone()).await.unwrap();
    let vdb_api = vdb_engine.management_address().unwrap();
    apply_managed_source(vdb_api, "crm").await;
    apply_managed_source(vdb_api, "erp").await;
    let (vdb_client, vdb_conn) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(vdb_engine.address().unwrap().port())
        .user("fedengine")
        .dbname("fedengine")
        .password(std::env::var("USER").unwrap())
        .connect(NoTls)
        .await
        .unwrap();
    let vdb_driver = tokio::spawn(async move { vdb_conn.await.unwrap() });
    let relkinds: Vec<String> = vdb_client
        .query(
            "SELECT DISTINCT c.relkind::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ('crm','erp') AND c.relname='fedengine_vdb_seed'",
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(relkinds, ["r"]);
    assert_eq!(
        vdb_client
            .query_one(
                "SELECT count(*)::BIGINT FROM pg_catalog.pg_foreign_table",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        vdb_client
            .query_one("SELECT count(*)::BIGINT FROM pg_catalog.pg_tables WHERE schemaname IN ('crm','erp') AND tablename='fedengine_vdb_seed'", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    assert!(
        vdb_client
            .query_one(
                "SELECT has_table_privilege('crm.fedengine_vdb_seed', 'INSERT')",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    vdb_client
        .simple_query(
            "CREATE TABLE crm.fedengine_vdb_live(id INTEGER PRIMARY KEY, name VARCHAR(32) NOT NULL)",
        )
        .await
        .unwrap();
    assert_eq!(
        vdb_client
            .execute(
                "INSERT INTO crm.fedengine_vdb_live VALUES(10, 'pg-vdb')",
                &[]
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        vdb_client
            .execute(
                "UPDATE crm.fedengine_vdb_live SET name='pg-vdb2' WHERE id=10",
                &[]
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        vdb_client
            .query_one("SELECT name FROM crm.fedengine_vdb_live WHERE id=10", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "pg-vdb2"
    );
    assert_eq!(
        vdb_client
            .execute("DELETE FROM crm.fedengine_vdb_live WHERE id=10", &[])
            .await
            .unwrap(),
        1
    );
    vdb_client
        .simple_query("DROP TABLE crm.fedengine_vdb_live")
        .await
        .unwrap();
    vdb_client
        .simple_query(
            "CREATE TABLE erp.fedengine_vdb_my(id INTEGER PRIMARY KEY, val INTEGER NOT NULL)",
        )
        .await
        .unwrap();
    assert_eq!(
        vdb_client
            .execute("INSERT INTO erp.fedengine_vdb_my VALUES(1, 42)", &[])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        vdb_client
            .query_one("SELECT val FROM erp.fedengine_vdb_my WHERE id=1", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        42
    );
    vdb_client
        .simple_query("DROP TABLE erp.fedengine_vdb_my")
        .await
        .unwrap();
    drop(vdb_client);
    vdb_driver.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), vdb_engine.shutdown())
        .await
        .unwrap()
        .unwrap();

    // Verify read_only=true sources still deny writes & DDL through a virtual database.
    virtual_config.duckdb.database_path = temporary
        .path()
        .join("ro-vdb.duckdb")
        .to_string_lossy()
        .into_owned();
    virtual_config.datasources[0].read_only = true;
    let ro_engine = FedEngine::start(virtual_config).await.unwrap();
    let ro_api = ro_engine.management_address().unwrap();
    apply_managed_source(ro_api, "crm").await;
    apply_managed_source(ro_api, "erp").await;
    let (ro_client, ro_conn) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(ro_engine.address().unwrap().port())
        .user("fedengine")
        .dbname("fedengine")
        .password(std::env::var("USER").unwrap())
        .connect(NoTls)
        .await
        .unwrap();
    let ro_driver = tokio::spawn(async move { ro_conn.await.unwrap() });
    for sql in [
        "INSERT INTO crm.fedengine_vdb_seed VALUES(99, 'denied')",
        "CREATE TABLE crm.fedengine_vdb_denied(id INTEGER)",
    ] {
        let err = ro_client.simple_query(sql).await.unwrap_err();
        assert_eq!(err.code().unwrap().code(), "42501", "{sql}");
    }
    drop(ro_client);
    ro_driver.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), ro_engine.shutdown())
        .await
        .unwrap()
        .unwrap();
    admin
        .execute_batch(
            "DROP TABLE IF EXISTS pg_crm.public.fedengine_vdb_seed;
             DROP TABLE IF EXISTS mysql_erp.erp.fedengine_vdb_seed;",
        )
        .unwrap();
}

async fn http_json(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };
    let mut socket = TcpStream::connect(address).await.unwrap();
    let body = body.to_string();
    let key = key
        .map(|k| format!("Idempotency-Key: {k}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} /api/v1/{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\n{key}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let value =
        serde_json::from_str(body).unwrap_or_else(|_| panic!("invalid JSON response: {response}"));
    (status, value)
}

async fn apply_managed_source(address: std::net::SocketAddr, id: &str) {
    let (status, source) = http_json(
        address,
        "GET",
        &format!("sources/{id}"),
        None,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{source}");
    let body = serde_json::json!({
        "target": {
            "incarnation": source["incarnation"],
            "expected_revision": source["desired_revision"],
        }
    });
    let (status, operation) = http_json(
        address,
        "POST",
        &format!("sources/{id}/apply"),
        Some(&format!("apply-{id}")),
        body,
    )
    .await;
    assert_eq!(status, 202, "{operation}");
    let op_id = operation["id"].as_str().expect("operation id");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (status, result) = http_json(
                address,
                "GET",
                &format!("operations/{op_id}"),
                None,
                serde_json::Value::Null,
            )
            .await;
            assert_eq!(status, 200, "{result}");
            match result["status"].as_str().unwrap() {
                "Succeeded" => return,
                "Failed" | "Interrupted" => panic!("apply operation failed for {id}: {result}"),
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for apply on {id}"));
}
