//! Native target organization, discovery, grants and durable identity regressions.
mod common;
use modorch_fedengine_core::*;
use modorch_fedengine_test_host::FedEngine;
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
fn configuration(dir: &common::TestDirectory) -> Config {
    let mut c = Config::load("config/application.toml").unwrap();
    c.pgwire.listen = "127.0.0.1:0".into();
    c.management.listen = "127.0.0.1:0".into();
    c.users[0].password_secret_ref = "USER".into();
    c.duckdb.database_path = dir.path().join("state.duckdb").to_string_lossy().into();
    c.duckdb.temp_directory = dir.path().join("tmp").to_string_lossy().into();
    c.duckdb.threads = 1;
    c.duckdb.max_workers = 2;
    c.virtual_databases = vec![VirtualDatabaseConfig {
        name: "fedengine".into(),
        owner: "fedengine".into(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![],
    }];
    c
}
async fn http(address: SocketAddr, method: &str, path: &str, body: Value) -> (u16, Value) {
    http_with_key(
        address,
        method,
        path,
        body,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
}
async fn http_with_key(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Value,
    key: &str,
) -> (u16, Value) {
    let mut socket = TcpStream::connect(address).await.unwrap();
    let body = body.to_string();
    socket.write_all(format!("{method} /api/v1/{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nIdempotency-Key: test-{}\r\nContent-Length: {}\r\n\r\n{body}",key,body.len()).as_bytes()).await.unwrap();
    let mut response = vec![];
    tokio::time::timeout(Duration::from_secs(15), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}
async fn get(address: SocketAddr, path: &str) -> Value {
    let (s, v) = http(address, "GET", path, Value::Null).await;
    assert_eq!(s, 200, "{path}: {v}");
    v
}
async fn finish(address: SocketAddr, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let v = get(address, &format!("browser/queries/{id}")).await;
            if !["running", "canceling"].contains(&v["status"].as_str().unwrap()) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn start(db: &Value, sql: &str) -> Value {
    json!({"database":db["config"]["name"],"oid":db["oid"],"revision":db["revision"],"sql":sql,"issued_at":chrono::Utc::now().timestamp()})
}
fn browse(db: &Value, path: &str) -> String {
    format!(
        "browser/{path}?database={}&oid={}&revision={}",
        db["config"]["name"].as_str().unwrap(),
        db["oid"],
        db["revision"]
    )
}
async fn query(address: SocketAddr, db: &Value, sql: &str) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    let (s, v) = http(
        address,
        "POST",
        &format!("browser/queries/{id}"),
        start(db, sql),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    finish(address, &id).await
}
async fn operation(api: SocketAddr, path: &str, body: Value) -> Value {
    operation_method(api, "POST", path, body).await
}
async fn operation_method(api: SocketAddr, method: &str, path: &str, body: Value) -> Value {
    let (status, operation) = http(api, method, path, body).await;
    assert_eq!(status, 202, "{operation}");
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let result = get(
                api,
                &format!("operations/{}", operation["id"].as_str().unwrap()),
            )
            .await;
            if ["Succeeded", "Failed", "Interrupted"].contains(&result["status"].as_str().unwrap())
            {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["status"], "Succeeded", "{result}");
    result
}
async fn discover(api: SocketAddr, connection: Value) -> Value {
    let id = uuid::Uuid::new_v4();
    let path = format!("discovery/{id}");
    let input = json!({"issued_at":chrono::Utc::now().timestamp(),"connection":connection});
    let (status, _) = http(api, "POST", &path, input.clone()).await;
    assert_eq!(status, 202);
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let v = get(api, &path).await;
            if v["status"] != "running" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(http(api, "POST", &path, input.clone()).await.1, result);
    let mut changed = input;
    changed["connection"]["id"] = json!("changed");
    assert_eq!(http(api, "POST", &path, changed).await.1["code"], "40001");
    result
}
fn target(source: &Value) -> Value {
    json!({"incarnation":source["incarnation"],"expected_revision":source["desired_revision"]})
}
fn definition(
    id: &str,
    connection: &Value,
    database: &str,
    virtual_name: &str,
    schemas: &[&str],
) -> Value {
    json!({"id":id,"kind":connection["kind"],"catalog_alias":format!("remote_{id}"),"secret_ref":connection["secret_ref"],"organization":{"connection_id":connection["id"],"instance":connection["instance"],"database":database,"virtual_database":virtual_name,"owner":"fedengine","connect":["reader"]},"allowed_schemas":schemas,"default_schema":schemas.first().copied().unwrap_or(""),"read_only":true,"ddl_enabled":false})
}
async fn activate(api: SocketAddr, definition: Value) {
    let id = definition["id"].as_str().unwrap().to_owned();
    operation(api, "sources", definition).await;
    let source = get(api, &format!("sources/{id}")).await;
    operation(
        api,
        &format!("sources/{id}/apply"),
        json!({"target":target(&source)}),
    )
    .await;
}
async fn pg_client(
    address: SocketAddr,
    database: &str,
    user: &str,
) -> (tokio_postgres::Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(address.port())
        .user(user)
        .password(std::env::var("USER").unwrap())
        .dbname(database)
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        connection.await.unwrap();
    });
    (client, task)
}

#[tokio::test]
async fn automatic_target_reservations_reject_collisions_before_network_io() {
    let dir = common::TestDirectory::new();
    let mut c = configuration(&dir);
    c.users.push(UserConfig {
        name: "reader".into(),
        password_secret_ref: "USER".into(),
    });
    let engine = FedEngine::start(c).await.unwrap();
    let api = engine.management_address().unwrap();
    let connection = json!({"id":"connection","kind":"postgres","instance":{"host":"DB.EXAMPLE.","port":5432},"secret_ref":"file:unused.json"});
    let first = definition("first", &connection, "native", "business", &["public"]);
    operation(api, "sources", first.clone()).await;
    let mut second = definition("second", &connection, "native", "other", &["public"]);
    second["organization"]["instance"]["host"] = json!("db.example");
    let (status, conflict) = http(api, "POST", "sources", second.clone()).await;
    assert_eq!(status, 409);
    assert!(conflict["error"].as_str().unwrap().contains("business"));
    second["organization"]["virtual_database"] = json!("business");
    operation(api, "sources", second.clone()).await;
    let mut third = definition("third", &connection, "unrelated", "business", &["public"]);
    assert_eq!(http(api, "POST", "sources", third.clone()).await.0, 409);
    third["organization"]["virtual_database"] = json!("fedengine");
    assert_eq!(http(api, "POST", "sources", third.clone()).await.0, 409);
    third["organization"]["virtual_database"] = json!("separate");
    third["organization"]["instance"]["port"] = json!(5433);
    third["organization"]["connection_id"] = json!("other_connection");
    operation(api, "sources", third).await;
    // No discovery/registration implicitly creates a business database or username fallback.
    assert_eq!(
        get(api, "virtual-databases")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    engine.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and MySQL fixtures"]
async fn native_discovery_merge_routing_grants_removal_and_restart() {
    use std::str::FromStr;
    let dir = common::TestDirectory::new();
    let mut c = configuration(&dir);
    c.management.credentials_directory = dir.path().to_string_lossy().into();
    c.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into(),
    );
    c.users.push(UserConfig {
        name: "reader".into(),
        password_secret_ref: "USER".into(),
    });
    for id in ["one", "two", "other", "my"] {
        c.grants.push(Grant {
            principal: "fedengine".into(),
            source: DataSourceId(id.into()),
            schema: "*".into(),
            object: "*".into(),
            actions: ["SELECT".into()].into(),
        });
    }
    c.grants.push(Grant {
        principal: "reader".into(),
        source: DataSourceId("two".into()), // A later authorized contributor must replace the unauthorized fallback route.
        schema: "public".into(),
        object: "shared".into(),
        actions: ["SELECT".into()].into(),
    });
    let pg = tokio_postgres::Config::from_str(&std::env::var("FEDENGINE_POSTGRES_DSN").unwrap())
        .unwrap();
    let host = match &pg.get_hosts()[0] {
        tokio_postgres::config::Host::Tcp(host) => host.clone(),
        _ => panic!("TCP fixture required"),
    };
    let port = pg.get_ports()[0];
    let user = pg.get_user().unwrap();
    let (admin, admin_conn) = pg.connect(tokio_postgres::NoTls).await.unwrap();
    let admin_task = tokio::spawn(async move {
        admin_conn.await.unwrap();
    });
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..10].to_owned();
    let dbname = format!("org_{suffix}");
    let other = format!("other_{suffix}");
    let blocked = format!("blocked_{suffix}");
    let limited = format!("limited_{suffix}");
    admin
        .batch_execute(&format!(
            "CREATE ROLE {limited} LOGIN PASSWORD 'test-role-only'"
        ))
        .await
        .unwrap();
    for db in [&dbname, &other, &blocked] {
        admin
            .batch_execute(&format!("CREATE DATABASE {db}"))
            .await
            .unwrap();
    }
    admin
        .batch_execute(&format!("ALTER DATABASE {blocked} ALLOW_CONNECTIONS false"))
        .await
        .unwrap();
    let mut fixture = pg.clone();
    fixture.dbname(&dbname);
    let (native, native_conn) = fixture.connect(tokio_postgres::NoTls).await.unwrap();
    let native_task = tokio::spawn(async move {
        native_conn.await.unwrap();
    });
    native.batch_execute(&format!("CREATE SCHEMA business; CREATE SCHEMA empty_schema; CREATE TABLE public.shared(id INTEGER); INSERT INTO public.shared VALUES(7); CREATE TABLE public.private_table(id INTEGER); INSERT INTO public.private_table VALUES(11); CREATE TABLE business.orders(id INTEGER); INSERT INTO business.orders VALUES(13); CREATE VIEW public.route_user AS SELECT current_user::TEXT AS username; GRANT USAGE ON SCHEMA public TO {limited}; GRANT SELECT ON public.shared,public.route_user TO {limited};")).await.unwrap();
    let credentials = json!({"username":user,"password":String::from_utf8(pg.get_password().unwrap().to_vec()).unwrap(),"options":{"sslmode":"disable"}});
    std::fs::write(dir.path().join("pg.json"), credentials.to_string()).unwrap();
    std::fs::write(
        dir.path().join("limited.json"),
        json!({"username":limited,"password":"test-role-only","options":{"sslmode":"disable"}})
            .to_string(),
    )
    .unwrap();
    let connection = json!({"id":"two","kind":"postgres","instance":{"host":host,"port":port},"secret_ref":"file:pg.json"});
    let mut one = connection.clone();
    one["id"] = json!("one");
    one["secret_ref"] = json!("file:limited.json");
    let engine = FedEngine::start(c.clone()).await.unwrap();
    let api = engine.management_address().unwrap();
    let enumeration = discover(api, connection.clone()).await;
    let candidates = enumeration["candidates"].as_array().unwrap();
    assert!(
        candidates
            .iter()
            .any(|d| d["database"] == dbname && d["status"] == "available")
    );
    assert!(
        candidates
            .iter()
            .any(|d| d["database"] == blocked && d["status"] == "forbidden")
    );
    assert!(
        candidates
            .iter()
            .any(|d| d["database"] == "postgres" && d["status"] == "excluded")
    );
    activate(
        api,
        definition("first", &one, &dbname, "business_db", &["public"]),
    )
    .await;
    activate(
        api,
        definition(
            "second",
            &connection,
            &dbname,
            "business_db",
            &["public", "business", "empty_schema"],
        ),
    )
    .await;
    let limited_facts = get(api, "sources/first/catalog?schema=public&object=shared").await;
    for category in ["details", "column_properties", "constraints", "indexes"] {
        assert_eq!(
            limited_facts["objects"][0]["metadata"]["status"][category]["complete"], true,
            "authorized relation facts must survive inaccessible neighboring tables: {limited_facts}"
        );
    }
    let mut other_connection = connection.clone();
    other_connection["id"] = json!("other");
    activate(
        api,
        definition("other", &other_connection, &other, "other_db", &["public"]),
    )
    .await;
    let database = get(api, "virtual-databases/business_db").await;
    assert_eq!(database["managed"], true);
    let oid = database["oid"].clone();
    let shared = get(
        api,
        &format!(
            "{}&schema=public&object=shared",
            browse(&database, "object")
        ),
    )
    .await;
    assert_eq!(shared["contributors"].as_array().unwrap().len(), 2);
    assert_eq!(shared["metadata_conflict"], false);
    let shared_oid = shared["oid"].clone();
    let objects = get(
        api,
        &format!("{}&schema=public", browse(&database, "objects")),
    )
    .await;
    assert_eq!(
        objects["objects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["name"] == "shared")
            .count(),
        1
    );
    let q = query(api, &database, "SELECT id FROM public.private_table").await;
    assert_eq!(q["status"], "succeeded", "{q}");
    assert_eq!(q["result"]["rows"][0][0], "11");
    let route = query(api, &database, "SELECT username FROM public.route_user").await;
    assert_eq!(route["status"], "succeeded", "{route}");
    assert_eq!(route["result"]["rows"][0][0], limited);
    let (reader, reader_task) = pg_client(engine.address().unwrap(), "business_db", "reader").await;
    assert_eq!(
        reader
            .query_one("SELECT count(*) FROM pg_class WHERE relname='shared'", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        reader
            .query("SELECT * FROM public.private_table", &[])
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "42501"
    );
    assert_eq!(
        reader
            .query_one(
                "SELECT count(*) FROM pg_class WHERE relname='private_table'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        reader
            .query_one(
                "SELECT count(*) FROM information_schema.tables WHERE table_name='private_table'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert!(
        !reader
            .query_one(
                "SELECT has_table_privilege('public.private_table','SELECT')",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert_eq!(
        reader
            .query_one("SELECT id FROM public.shared", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        7
    );
    drop(reader);
    reader_task.await.unwrap();
    let (owner, owner_task) =
        pg_client(engine.address().unwrap(), "business_db", "fedengine").await;
    let public_oid = owner
        .query_one("SELECT oid FROM pg_namespace WHERE nspname='public'", &[])
        .await
        .unwrap()
        .get::<_, u32>(0);
    let names = owner
        .query("SELECT datname FROM pg_database ORDER BY datname", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["business_db", "fedengine", "other_db"]);
    assert_eq!(
        owner
            .query_one("SELECT id FROM shared", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        7
    );
    drop(owner);
    owner_task.await.unwrap();
    let discovered = discover(api, connection.clone()).await;
    assert!(
        discovered["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["database"] == dbname
                && d["merged"] == true
                && d["virtual_database"] == "business_db")
    );
    // Facts from independently refreshed credentials are never silently mixed.
    native
        .batch_execute("ALTER TABLE public.shared ADD COLUMN extra TEXT")
        .await
        .unwrap();
    let source = get(api, "sources/second").await;
    operation(
        api,
        "sources/second/refresh",
        json!({"target":target(&source)}),
    )
    .await;
    let conflicted = get(api, "virtual-databases/business_db").await;
    let facts = get(
        api,
        &format!(
            "{}&schema=public&object=shared",
            browse(&conflicted, "object")
        ),
    )
    .await;
    assert_eq!(facts["metadata_conflict"], true);
    assert_eq!(facts["source"]["id"], "first");
    let source = get(api, "sources/first").await;
    operation(
        api,
        "sources/first/refresh",
        json!({"target":target(&source)}),
    )
    .await;
    let consistent = get(api, "virtual-databases/business_db").await;
    let facts = get(
        api,
        &format!(
            "{}&schema=public&object=shared",
            browse(&consistent, "object")
        ),
    )
    .await;
    assert_eq!(facts["metadata_conflict"], false);
    let source = get(api, "sources/first").await;
    operation(
        api,
        "sources/first/disable",
        json!({"target":target(&source)}),
    )
    .await;
    let database = get(api, "virtual-databases/business_db").await;
    assert_eq!(database["oid"], oid);
    let remaining = get(
        api,
        &format!(
            "{}&schema=public&object=shared",
            browse(&database, "object")
        ),
    )
    .await;
    assert_eq!(remaining["oid"], shared_oid);
    assert_eq!(remaining["contributors"].as_array().unwrap().len(), 1);
    let (owner, owner_task) =
        pg_client(engine.address().unwrap(), "business_db", "fedengine").await;
    assert_eq!(
        owner
            .query_one("SELECT oid FROM pg_namespace WHERE nspname='public'", &[])
            .await
            .unwrap()
            .get::<_, u32>(0),
        public_oid
    );
    drop(owner);
    owner_task.await.unwrap();
    let source = get(api, "sources/first").await;
    operation_method(
        api,
        "DELETE",
        "sources/first",
        json!({"target":target(&source)}),
    )
    .await;
    engine.shutdown_until_drained().await.unwrap();
    let engine = FedEngine::start(c).await.unwrap();
    let api = engine.management_address().unwrap();
    let database = get(api, "virtual-databases/business_db").await;
    assert_eq!(database["oid"], oid);
    let remaining = get(
        api,
        &format!(
            "{}&schema=public&object=shared",
            browse(&database, "object")
        ),
    )
    .await;
    assert_eq!(remaining["oid"], shared_oid);
    let source = get(api, "sources/second").await;
    operation(
        api,
        "sources/second/disable",
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(get(api, "virtual-databases/business_db").await["oid"], oid);
    let (owner, owner_task) =
        pg_client(engine.address().unwrap(), "business_db", "fedengine").await;
    assert_eq!(
        owner
            .query_one("SELECT oid FROM pg_namespace WHERE nspname='public'", &[])
            .await
            .unwrap()
            .get::<_, u32>(0),
        public_oid
    );
    drop(owner);
    owner_task.await.unwrap();
    let source = get(api, "sources/second").await;
    operation(
        api,
        "sources/second/apply",
        json!({"target":target(&source)}),
    )
    .await;
    let resumed = get(api, "virtual-databases/business_db").await;
    let resumed_item = get(
        api,
        &format!("{}&schema=public&object=shared", browse(&resumed, "object")),
    )
    .await;
    assert_eq!(
        resumed_item["oid"],
        shared_oid,
        "schema identities before={}, after={}; native identity before={}, after={}",
        database["schemas"],
        resumed["schemas"],
        shared["object"]["metadata"]["source_id"],
        resumed_item["object"]["metadata"]["source_id"]
    );
    let source = get(api, "sources/second").await;
    operation(
        api,
        "sources/second/disable",
        json!({"target":target(&source)}),
    )
    .await;
    let source = get(api, "sources/second").await;
    operation_method(
        api,
        "DELETE",
        "sources/second",
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(
        http(api, "GET", "virtual-databases/business_db", Value::Null)
            .await
            .0,
        404
    );
    engine.shutdown_until_drained().await.unwrap();
    drop(native);
    native_task.await.unwrap();
    for db in [&dbname, &other, &blocked] {
        admin
            .batch_execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
            .await
            .unwrap();
    }
    admin
        .batch_execute(&format!("DROP ROLE {limited}"))
        .await
        .unwrap();
    drop(admin);
    admin_task.await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable MySQL fixture"]
async fn mysql_instance_databases_are_schemas_in_one_virtual_database() {
    mysql_organization(false).await;
}
#[tokio::test]
#[ignore = "requires disposable MySQL fixture"]
async fn mysql_strict_lifetimes_reset_all_contributors_together() {
    mysql_organization(true).await;
}
async fn mysql_organization(strict: bool) {
    let dir = common::TestDirectory::new();
    let mut c = configuration(&dir);
    if strict {
        c.management.mysql_identity_policy = MysqlIdentityPolicy::Strict;
    }
    c.management.credentials_directory = dir.path().to_string_lossy().into();
    c.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into(),
    );
    c.users.push(UserConfig {
        name: "reader".into(),
        password_secret_ref: "USER".into(),
    });
    for id in ["my", "overlap"] {
        c.grants.push(Grant {
            principal: "fedengine".into(),
            source: DataSourceId(id.into()),
            schema: "*".into(),
            object: "*".into(),
            actions: ["SELECT".into()].into(),
        });
    }
    // The fixture explicitly uses keyword DSN fields with test-only values.
    let dsn = std::env::var("FEDENGINE_MYSQL_DSN").unwrap();
    let fields = dsn
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .collect::<std::collections::BTreeMap<_, _>>();
    let port = fields["port"].parse::<u16>().unwrap();
    let connection = json!({"id":"my","kind":"mysql","instance":{"host":fields["host"],"port":port},"secret_ref":"file:my.json"});
    std::fs::write(
        dir.path().join("my.json"),
        json!({"username":fields["user"],"password":fields["password"]}).to_string(),
    )
    .unwrap();
    let native = duckdb::Connection::open_in_memory().unwrap();
    native
        .execute_batch(&format!(
            "SET extension_directory={};LOAD mysql;ATTACH {} AS native (TYPE mysql)",
            quote_literal(c.duckdb.extension_directory.as_ref().unwrap()),
            quote_literal(&dsn)
        ))
        .unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..10].to_owned();
    let a = format!("sales_{suffix}");
    let b = format!("App_{suffix}");
    for (database, value) in [(&a, 19), (&b, 23)] {
        native.execute_batch(&format!("CALL mysql_execute('native',{});CALL mysql_execute('native',{});CALL mysql_execute('native',{})",quote_literal(&format!("CREATE DATABASE {database}")),quote_literal(&format!("CREATE TABLE {database}.items(id INT)")),quote_literal(&format!("INSERT INTO {database}.items VALUES({value})")))).unwrap();
    }
    let engine = FedEngine::start(c.clone()).await.unwrap();
    let api = engine.management_address().unwrap();
    let enumeration = discover(api, connection.clone()).await;
    for database in [&a, &b] {
        assert!(enumeration["candidates"].as_array().unwrap().iter().any(
            |candidate| candidate["database"] == database.as_str()
                && candidate["schemas"] == json!([database])
                && candidate["status"] == "available"
        ));
    }
    assert!(
        enumeration["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["database"] == "mysql" && candidate["status"] == "excluded")
    );
    activate(
        api,
        definition("my_a", &connection, &a, "mysql_business", &[&a]),
    )
    .await;
    activate(
        api,
        definition("my_b", &connection, &b, "mysql_business", &[&b]),
    )
    .await;
    let mut overlap = connection.clone();
    overlap["id"] = json!("overlap");
    activate(
        api,
        definition("my_overlap", &overlap, &a, "mysql_business", &[&a]),
    )
    .await;
    let database = get(api, "virtual-databases/mysql_business").await;
    assert_eq!(database["config"]["schemas"].as_array().unwrap().len(), 3);
    let schemas = get(api, &browse(&database, "schemas")).await;
    let names = schemas["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&a.as_str()) && names.contains(&b.as_str()));
    let q = query(
        api,
        &database,
        &format!("SELECT a.id+b.id AS total FROM {a}.items a CROSS JOIN \"{b}\".items b"),
    )
    .await;
    assert_eq!(q["status"], "succeeded", "{q}");
    assert_eq!(q["result"]["rows"][0][0], "42");
    let item = get(
        api,
        &format!("{}&schema={a}&object=items", browse(&database, "object")),
    )
    .await;
    assert_eq!(item["contributors"].as_array().unwrap().len(), 2);
    let oid = item["oid"].clone();
    let source = get(api, "sources/my_a").await;
    operation(
        api,
        "sources/my_a/disable",
        json!({"target":target(&source)}),
    )
    .await;
    let database = get(api, "virtual-databases/mysql_business").await;
    let item = get(
        api,
        &format!("{}&schema={a}&object=items", browse(&database, "object")),
    )
    .await;
    assert_eq!(item["oid"], oid);
    engine.shutdown_until_drained().await.unwrap();
    let engine = FedEngine::start(c).await.unwrap();
    let api = engine.management_address().unwrap();
    let database = get(api, "virtual-databases/mysql_business").await;
    let item = get(
        api,
        &format!("{}&schema={a}&object=items", browse(&database, "object")),
    )
    .await;
    if strict {
        assert_ne!(item["oid"], oid);
    } else {
        assert_eq!(item["oid"], oid);
    }
    engine.shutdown_until_drained().await.unwrap();
    for database in [&a, &b] {
        native
            .execute_batch(&format!(
                "CALL mysql_execute('native',{})",
                quote_literal(&format!("DROP DATABASE {database}"))
            ))
            .unwrap();
    }
}
