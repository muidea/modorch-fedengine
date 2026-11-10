//! Host-owned paths and independent applications using the production Bundle.
mod common;
use common::TestDirectory;
use modorch::StartupError;
use modorch_fedengine_core::{Config, VirtualDatabaseConfig};
use modorch_fedengine_test_host::FedEngine;

fn config(directory: &TestDirectory) -> Config {
    let mut config = Config::load("config/application.toml").unwrap();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.management.listen = "127.0.0.1:0".into();
    config.duckdb.database_path = directory
        .path()
        .join("state.duckdb")
        .to_string_lossy()
        .into();
    config.duckdb.temp_directory = directory.path().join("tmp").to_string_lossy().into();
    config.management.credentials_directory =
        directory.path().join("secrets").to_string_lossy().into();
    config.users[0].password_secret_ref = "USER".into();
    config.duckdb.max_workers = 1;
    config.duckdb.threads = 1;
    config.virtual_databases = vec![VirtualDatabaseConfig {
        name: "hostdb".into(),
        owner: "fedengine".into(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![],
    }];
    config
}

#[tokio::test]
async fn a_live_management_file_cannot_be_owned_by_two_applications() {
    let directory = TestDirectory::new();
    let config = config(&directory);
    let first = FedEngine::start(config.clone()).await.unwrap();
    let second = FedEngine::start(config).await;
    let error = match second {
        Ok(second) => {
            second.shutdown_until_drained().await.unwrap();
            first.shutdown_until_drained().await.unwrap();
            panic!("two applications were admitted to the same management file");
        }
        Err(error) => error.downcast::<StartupError>().unwrap(),
    };
    assert!(
        error.source.to_string().contains("already owned"),
        "{}",
        error.source
    );
    assert!(first.handle.health().ready);
    first.shutdown_until_drained().await.unwrap();
}

async fn rejected(config: Config, message: &str) {
    let error = match FedEngine::start(config).await {
        Ok(engine) => {
            engine.shutdown_until_drained().await.unwrap();
            panic!("expected startup rejection: {message}");
        }
        Err(error) => error.downcast::<StartupError>().unwrap(),
    };
    assert!(
        error.source.to_string().contains(message),
        "{}",
        error.source
    );
    assert!(matches!(
        error.cleanup,
        Ok(modorch::ShutdownReceipt::GracefullyStopped)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn path_aliases_do_not_bypass_ownership_and_drain_releases_it() {
    let directory = TestDirectory::new();
    let original = config(&directory);
    // The first opening creates a target through an initially dangling symlink.
    std::os::unix::fs::symlink(
        &original.duckdb.database_path,
        directory.path().join("symbolic.duckdb"),
    )
    .unwrap();
    let mut through_link = original.clone();
    through_link.duckdb.database_path = directory
        .path()
        .join("symbolic.duckdb")
        .to_str()
        .unwrap()
        .into();
    let first = FedEngine::start(through_link).await.unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::os::unix::fs::symlink(directory.path(), directory.path().join("alias")).unwrap();
    std::fs::hard_link(
        &original.duckdb.database_path,
        directory.path().join("hard.duckdb"),
    )
    .unwrap();
    for alias in [
        "state.duckdb",
        "child/../state.duckdb",
        "alias/state.duckdb",
        "symbolic.duckdb",
        "hard.duckdb",
    ] {
        let mut config = original.clone();
        config.duckdb.database_path = directory.path().join(alias).to_str().unwrap().into();
        rejected(config, "already owned").await;
        assert!(first.handle.health().ready);
    }
    first.shutdown_until_drained().await.unwrap();
    let mut renamed = original;
    renamed.application.name = "renamed-host".into();
    let restarted = FedEngine::start(renamed).await.unwrap();
    restarted.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
async fn concurrent_new_file_admission_has_exactly_one_owner() {
    let directory = TestDirectory::new();
    let config = config(&directory);
    let (left, right) = tokio::join!(
        FedEngine::start(config.clone()),
        FedEngine::start(config.clone())
    );
    let (winner, loser) = match (left, right) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
        (Ok(left), Ok(right)) => {
            left.shutdown_until_drained().await.unwrap();
            right.shutdown_until_drained().await.unwrap();
            panic!("concurrent applications shared a new management file");
        }
        (Err(left), Err(right)) => panic!("neither admitted: {left}; {right}"),
    };
    let error = loser.downcast::<StartupError>().unwrap();
    assert!(error.source.to_string().contains("already owned"));
    assert!(winner.handle.health().ready);
    winner.shutdown_until_drained().await.unwrap();
    let next = FedEngine::start(config).await.unwrap();
    next.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
async fn failed_setup_releases_ownership_without_resetting_existing_state() {
    let directory = TestDirectory::new();
    let config = config(&directory);
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let created = create_database(
        engine.management_address().unwrap(),
        "persisted",
        "original",
    )
    .await;
    engine.shutdown_until_drained().await.unwrap();
    let mut missing_secret = config.clone();
    missing_secret.users[0].password_secret_ref =
        format!("FEDENGINE_ABSENT_FE09_{}", std::process::id());
    assert!(std::env::var(&missing_secret.users[0].password_secret_ref).is_err());
    rejected(missing_secret, "SecretRef is missing").await;
    let engine = FedEngine::start(config.clone()).await.unwrap();
    assert_eq!(
        get(
            engine.management_address().unwrap(),
            "virtual-databases/persisted"
        )
        .await["oid"],
        created["oid"]
    );
    engine.shutdown_until_drained().await.unwrap();
    {
        let connection = duckdb::Connection::open(&config.duckdb.database_path).unwrap();
        connection.execute_batch("DELETE FROM _fedengine.schema_migrations; INSERT INTO _fedengine.schema_migrations VALUES (99, 'unsupported'); CREATE TABLE untouched AS SELECT 42 AS value").unwrap();
    }
    // Both attempts must report the unsupported schema, rather than a leaked claim.
    for _ in 0..2 {
        rejected(config.clone(), "schema version does not match").await;
        let connection = duckdb::Connection::open(&config.duckdb.database_path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT version FROM _fedengine.schema_migrations",
                    [],
                    |r| r.get::<_, i32>(0)
                )
                .unwrap(),
            99
        );
        assert_eq!(
            connection
                .query_row("SELECT value FROM untouched", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            42
        );
    }
}

use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    key: &str,
    value: Value,
) -> (u16, Value) {
    let mut socket = TcpStream::connect(address).await.unwrap();
    let body = value.to_string();
    socket.write_all(format!("{method} /api/v1/{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nIdempotency-Key: {key}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(bytes).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}
async fn get(address: SocketAddr, path: &str) -> Value {
    let (code, value) = http(address, "GET", path, "read", Value::Null).await;
    assert_eq!(code, 200, "{value}");
    value
}
async fn create_database(address: SocketAddr, name: &str, key: &str) -> Value {
    let (code, value) = http(
        address,
        "POST",
        "virtual-databases",
        key,
        json!({"name":name,"owner":"fedengine","connect":[],"allow_connections":true,"schemas":[]}),
    )
    .await;
    assert_eq!(code, 200, "{value}");
    value
}
async fn register(address: SocketAddr, secret_ref: &str) -> Value {
    let (code, operation) = http(address, "POST", "sources", "same-key", json!({"id":"online","kind":"postgres","catalog_alias":"pg_online","secret_ref":secret_ref,"default_schema":"public","allowed_schemas":["public"],"read_only":true})).await;
    assert_eq!(code, 202, "{operation}");
    let id = operation["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let operation = get(address, &format!("operations/{id}")).await;
            if ["Succeeded", "Failed", "Interrupted"]
                .contains(&operation["status"].as_str().unwrap())
            {
                assert_eq!(operation["status"], "Succeeded", "{operation}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    get(address, "sources/online").await
}

#[tokio::test]
async fn applications_isolate_paths_ports_receipts_and_persist_identity_across_rename() {
    let left = TestDirectory::new();
    let right = TestDirectory::new();
    let mut left_config = config(&left);
    let right_config = config(&right);
    assert_eq!(left_config.application.name, right_config.application.name);
    for (config, sentinel) in [
        (&left_config, b"left-credential" as &[u8]),
        (&right_config, b"right-credential" as &[u8]),
    ] {
        std::fs::create_dir(&config.management.credentials_directory).unwrap();
        std::fs::write(
            std::path::Path::new(&config.management.credentials_directory).join("source.dsn"),
            sentinel,
        )
        .unwrap();
    }
    let a = FedEngine::start(left_config.clone()).await.unwrap();
    let b = FedEngine::start(right_config.clone()).await.unwrap();
    assert_ne!(a.handle.id(), b.handle.id());
    assert_ne!(a.address(), b.address());
    assert_ne!(a.management_address(), b.management_address());
    let a_api = a.management_address().unwrap();
    let b_api = b.management_address().unwrap();
    let a_source = register(a_api, "file:source.dsn").await;
    let b_source = register(b_api, "file:source.dsn").await;
    assert_ne!(a_source["incarnation"], b_source["incarnation"]);
    let receipt = create_database(a_api, "owned_a", "same-key").await;
    create_database(b_api, "owned_b", "same-key").await;
    assert_eq!(
        http(
            a_api,
            "GET",
            "virtual-databases/owned_b",
            "read",
            Value::Null
        )
        .await
        .0,
        404
    );
    assert_eq!(
        http(
            b_api,
            "GET",
            "virtual-databases/owned_a",
            "read",
            Value::Null
        )
        .await
        .0,
        404
    );
    a.shutdown_until_drained().await.unwrap();
    assert!(b.handle.health().ready);
    assert_eq!(
        get(b_api, "sources/online").await["incarnation"],
        b_source["incarnation"]
    );
    left_config.application.name = "renamed-application".into();
    let renamed = FedEngine::start(left_config).await.unwrap();
    let api = renamed.management_address().unwrap();
    assert_eq!(
        get(api, "sources/online").await["incarnation"],
        a_source["incarnation"]
    );
    assert_eq!(
        get(api, "sources/online").await["desired_revision"],
        a_source["desired_revision"]
    );
    assert_eq!(create_database(api, "owned_a", "same-key").await, receipt);
    assert_eq!(
        get(api, "virtual-databases/owned_a").await["oid"],
        receipt["oid"]
    );
    renamed.shutdown_until_drained().await.unwrap();
    b.shutdown_until_drained().await.unwrap();
    for (config, sentinel) in [
        (&config(&left), b"left-credential" as &[u8]),
        (&right_config, b"right-credential" as &[u8]),
    ] {
        assert_eq!(
            std::fs::read(
                std::path::Path::new(&config.management.credentials_directory).join("source.dsn")
            )
            .unwrap(),
            sentinel
        );
    }
}

async fn connect(
    address: SocketAddr,
    database: &str,
) -> (tokio_postgres::Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(address.port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname(database)
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap();
    (
        client,
        tokio::spawn(async move { connection.await.unwrap() }),
    )
}
async fn marker(engine: &FedEngine, expected: &str) -> u32 {
    let (client, driver) = connect(engine.address().unwrap(), "pg_crm").await;
    let value: String = client
        .query_one("SELECT value FROM public.fe09_instance_marker", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(value, expected);
    let oid: u32 = client
        .query_one(
            "SELECT oid FROM pg_catalog.pg_class WHERE relname='fe09_instance_marker'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    drop(client);
    driver.await.unwrap();
    oid
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and prepared DuckDB extensions"]
async fn same_secret_reference_is_scoped_to_each_host_and_survives_rename() {
    let dsn = std::env::var("FEDENGINE_POSTGRES_DSN").expect("disposable DSN required");
    let (admin, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let admin_driver = tokio::spawn(async move { connection.await.unwrap() });
    admin.batch_execute("DROP TABLE IF EXISTS public.fe09_instance_marker; CREATE TABLE public.fe09_instance_marker(value TEXT); INSERT INTO public.fe09_instance_marker VALUES ('left')").await.unwrap();
    admin
        .batch_execute("CREATE DATABASE fe09_second")
        .await
        .unwrap();
    let second_dsn = format!("{dsn} dbname=fe09_second");
    let (other, connection) = tokio_postgres::connect(&second_dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let other_driver = tokio::spawn(async move { connection.await.unwrap() });
    other.batch_execute("CREATE TABLE public.fe09_instance_marker(value TEXT); INSERT INTO public.fe09_instance_marker VALUES ('right')").await.unwrap();
    let left = TestDirectory::new();
    let right = TestDirectory::new();
    let remote_config = |directory: &TestDirectory, dsn: &str| {
        let mut c = config(directory);
        let remote = Config::load("examples/config/federation.toml").unwrap();
        c.datasources = vec![remote.datasources[0].clone()];
        c.datasources[0].secret_ref = "file:source.dsn".into();
        c.datasources[0].allowed_schemas = vec!["public".into()];
        c.grants = vec![remote.grants[0].clone()];
        c.duckdb.extension_directory = Some(
            std::fs::canonicalize("data/extensions")
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
        );
        std::fs::create_dir(&c.management.credentials_directory).unwrap();
        std::fs::write(
            std::path::Path::new(&c.management.credentials_directory).join("source.dsn"),
            dsn,
        )
        .unwrap();
        c
    };
    let mut left_config = remote_config(&left, &dsn);
    let right_config = remote_config(&right, &second_dsn);
    let a = FedEngine::start(left_config.clone()).await.unwrap();
    let b = FedEngine::start(right_config.clone()).await.unwrap();
    let oid = marker(&a, "left").await;
    marker(&b, "right").await;
    let source = get(a.management_address().unwrap(), "sources/crm").await;
    a.shutdown_until_drained().await.unwrap();
    // Changing the host name/bootstrap input cannot overwrite durable source refs.
    left_config.application.name = "new-host-name".into();
    left_config.datasources[0].secret_ref = "file:must-not-import.dsn".into();
    let renamed = FedEngine::start(left_config.clone()).await.unwrap();
    assert_eq!(marker(&renamed, "left").await, oid);
    let retained = get(renamed.management_address().unwrap(), "sources/crm").await;
    assert_eq!(retained["incarnation"], source["incarnation"]);
    assert_eq!(retained["desired_revision"], source["desired_revision"]);
    renamed.shutdown_until_drained().await.unwrap();
    let secret =
        std::path::Path::new(&left_config.management.credentials_directory).join("source.dsn");
    std::fs::remove_file(&secret).unwrap();
    let degraded = FedEngine::start(left_config.clone()).await.unwrap();
    let failed = get(degraded.management_address().unwrap(), "sources/crm").await;
    assert_eq!(failed["status"], "Degraded", "{failed}");
    assert_eq!(failed["incarnation"], source["incarnation"]);
    assert!(
        failed["last_error"]
            .as_str()
            .unwrap()
            .contains("credential file unavailable")
    );
    marker(&b, "right").await;
    degraded.shutdown_until_drained().await.unwrap();
    std::fs::write(&secret, &dsn).unwrap();
    let recovered = FedEngine::start(left_config).await.unwrap();
    assert_eq!(marker(&recovered, "left").await, oid);
    recovered.shutdown_until_drained().await.unwrap();
    b.shutdown_until_drained().await.unwrap();
    assert_eq!(std::fs::read_to_string(secret).unwrap(), dsn);
    assert_eq!(
        std::fs::read_to_string(
            std::path::Path::new(&right_config.management.credentials_directory).join("source.dsn")
        )
        .unwrap(),
        second_dsn
    );
    drop(other);
    other_driver.await.unwrap();
    admin
        .batch_execute("DROP TABLE public.fe09_instance_marker")
        .await
        .unwrap();
    admin
        .batch_execute("DROP DATABASE fe09_second")
        .await
        .unwrap();
    drop(admin);
    admin_driver.await.unwrap();
}
