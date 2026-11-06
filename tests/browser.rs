//! Real HTTP regressions for the embedded virtual database workspace.
mod common;
use fedengine_app::FedEngine;
use fedengine_core::*;
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
async fn mutate(api: SocketAddr, body: Value) -> Value {
    let key = uuid::Uuid::new_v4().to_string();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let (status, value) = http_with_key(
                api,
                "PUT",
                "virtual-databases/fedengine",
                body.clone(),
                &key,
            )
            .await;
            if status == 409 && value["code"] == "55P03" {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            assert_eq!(status, 200, "{value}");
            return value;
        }
    })
    .await
    .unwrap()
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
#[tokio::test]
async fn empty_database_browsing_query_text_precision_and_fail_closed_sql() {
    let dir = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&dir)).await.unwrap();
    let api = engine.management_address().unwrap();
    let db = get(api, "virtual-databases/fedengine").await;
    let schemas = get(api, &browse(&db, "schemas")).await;
    assert_eq!(
        schemas["schemas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["public"]
    );
    let stamp = schemas["stamp"].as_str().unwrap();
    let objects = get(
        api,
        &format!("{}&schema=public&stamp={stamp}", browse(&db, "objects")),
    )
    .await;
    assert!(objects["objects"].as_array().unwrap().is_empty());
    let system = get(api, &format!("{}&system=true", browse(&db, "schemas"))).await;
    assert!(
        system["schemas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "pg_catalog")
    );
    assert!(!system.to_string().contains("_fedengine"));
    let tables = get(
        api,
        &format!(
            "{}&system=true&schema=pg_catalog&search=pg_class",
            browse(&db, "objects")
        ),
    )
    .await;
    assert!(
        tables["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "pg_class")
    );
    let detail = get(
        api,
        &format!(
            "{}&system=true&schema=pg_catalog&object=pg_class",
            browse(&db, "object")
        ),
    )
    .await;
    assert_eq!(detail["external"], false);
    assert!(!detail["columns"]["rows"].as_array().unwrap().is_empty());
    let result=query(api,&db,"SELECT 9223372036854775807::BIGINT AS exact_value, NULL::TEXT AS missing, ''::TEXT AS empty").await;
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["principal"], "fedengine");
    assert_eq!(
        result["result"]["rows"],
        json!([["9223372036854775807", null, ""]])
    );
    assert_eq!(result["result"]["columns"][0]["type_oid"], 20);
    let error = query(api, &db, "SELECT missing_column").await;
    assert_eq!(error["status"], "failed");
    assert!(error["error"]["code"].is_string());
    for sql in [
        "SELECT * FROM read_csv('/etc/passwd')",
        "SELECT * FROM _fedengine.catalog_objects",
        "SELECT pg_terminate_backend(1)",
    ] {
        let denied = query(api, &db, sql).await;
        assert_eq!(denied["status"], "failed", "{denied}");
    }
    for sql in [
        "DELETE FROM t",
        "SELECT 1; SELECT 2",
        "SELECT 1 INTO stolen",
        "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
        "BEGIN",
        "SET search_path=public",
    ] {
        let (status, _) = http(
            api,
            "POST",
            &format!("browser/queries/{}", uuid::Uuid::new_v4()),
            start(&db, sql),
        )
        .await;
        assert_eq!(status, 400, "{sql}");
    }
    for extra in ["&stamp=old", "&offset=50", "&schema=_fedengine"] {
        let (status, _) = http(
            api,
            "GET",
            &format!(
                "{}{extra}",
                browse(
                    &db,
                    if extra.contains("schema") {
                        "objects"
                    } else {
                        "schemas"
                    }
                )
            ),
            Value::Null,
        )
        .await;
        assert!(status >= 400, "{extra}");
    }
    let mut wrong = db.clone();
    wrong["oid"] = json!(1);
    assert_eq!(
        http(api, "GET", &browse(&wrong, "schemas"), Value::Null)
            .await
            .0,
        409
    );
    engine.shutdown_until_drained().await.unwrap();
}
#[tokio::test]
async fn query_idempotency_limits_cancel_timeout_and_checked_shutdown() {
    let dir = common::TestDirectory::new();
    let mut config = configuration(&dir);
    config.pgwire.query_timeout_ms = 1500;
    let engine = FedEngine::start(config).await.unwrap();
    let api = engine.management_address().unwrap();
    let db = get(api, "virtual-databases/fedengine").await;
    let id = uuid::Uuid::new_v4().to_string();
    let body = start(&db, "SELECT 1");
    assert_eq!(
        http(api, "POST", &format!("browser/queries/{id}"), body.clone())
            .await
            .0,
        200
    );
    let original = finish(api, &id).await;
    assert_eq!(
        http(api, "POST", &format!("browser/queries/{id}"), body)
            .await
            .1,
        original
    );
    assert_eq!(
        http(
            api,
            "POST",
            &format!("browser/queries/{id}"),
            start(&db, "SELECT 2")
        )
        .await
        .0,
        409
    );
    let limited = query(api, &db, "SELECT i::INTEGER FROM range(1001) t(i)").await;
    assert_eq!(limited["status"], "succeeded", "{limited}");
    assert_eq!(limited["result"]["rows"].as_array().unwrap().len(), 1000);
    assert_eq!(limited["result"]["truncated"], true);
    let huge = query(
        api,
        &db,
        "SELECT string_agg('xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx', '') FROM range(125000)",
    )
    .await;
    assert_eq!(huge["status"], "succeeded", "{huge}");
    assert_eq!(huge["result"]["truncated"], true);
    assert!(huge["result"]["rows"].as_array().unwrap().is_empty());
    let slow = "SELECT sum(a.i*b.j) FROM range(1000000000) a(i),range(1000000000) b(j)";
    let id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        http(
            api,
            "POST",
            &format!("browser/queries/{id}"),
            start(&db, slow)
        )
        .await
        .0,
        200
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, cancel) = http(
        api,
        "POST",
        &format!("browser/queries/{id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(cancel["status"], "canceling");
    assert_eq!(finish(api, &id).await["error"]["code"], "57014");
    let timeout = query(api, &db, slow).await;
    assert_eq!(timeout["error"]["code"], "57014");
    let mut running = Vec::new();
    for _ in 0..4 {
        let id = uuid::Uuid::new_v4().to_string();
        let (status, view) = http(
            api,
            "POST",
            &format!("browser/queries/{id}"),
            start(&db, slow),
        )
        .await;
        assert_eq!(status, 200, "{view}");
        running.push(id);
    }
    assert_eq!(
        http(
            api,
            "POST",
            &format!("browser/queries/{}", uuid::Uuid::new_v4()),
            start(&db, slow)
        )
        .await
        .0,
        503
    );
    for id in &running {
        http(
            api,
            "POST",
            &format!("browser/queries/{id}/cancel"),
            json!({}),
        )
        .await;
    }
    for id in &running {
        assert_eq!(finish(api, id).await["error"]["code"], "57014");
    }
    let id = uuid::Uuid::new_v4().to_string();
    http(
        api,
        "POST",
        &format!("browser/queries/{id}"),
        start(&db, slow),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(8), engine.shutdown_until_drained())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(engine.metrics().active_workers, 0);
}

async fn operation(api: SocketAddr, path: &str, body: Value) -> Value {
    let (status, op) = http(api, "POST", path, body).await;
    assert_eq!(status, 202, "{op}");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let v = get(api, &format!("operations/{}", op["id"].as_str().unwrap())).await;
            match v["status"].as_str().unwrap() {
                "Succeeded" => return v,
                "Failed" | "Interrupted" => panic!("{v}"),
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL and MySQL fixtures"]
async fn mapped_sources_paging_details_grants_refresh_and_definition_conflicts() {
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
    let schema = format!("browser_{}", uuid::Uuid::new_v4().simple());
    let admin = duckdb::Connection::open_in_memory().unwrap();
    admin
        .execute_batch(&format!(
            "SET extension_directory={}; LOAD postgres; LOAD mysql",
            quote_literal(c.duckdb.extension_directory.as_ref().unwrap())
        ))
        .unwrap();
    for (id, kind, var) in [
        ("pg", "postgres", "FEDENGINE_POSTGRES_DSN"),
        ("my", "mysql", "FEDENGINE_MYSQL_DSN"),
    ] {
        let dsn = std::env::var(var).unwrap();
        let source_dsn = if id == "my" {
            dsn.replace("database=erp", &format!("database={schema}"))
        } else {
            dsn.clone()
        };
        std::fs::write(dir.path().join(id), &source_dsn).unwrap();
        admin.execute_batch(&format!("ATTACH {} AS {id} (TYPE {kind}); CREATE SCHEMA {id}.{schema}; CALL {kind}_execute('{id}',{}); INSERT INTO {id}.{schema}.items VALUES (1,'one')",quote_literal(&dsn),quote_literal(&format!("CREATE TABLE {schema}.items(id INTEGER PRIMARY KEY, label VARCHAR(30) DEFAULT 'example')")))).unwrap();
        c.virtual_databases[0].schemas.push(VirtualSchemaConfig {
            name: format!("mapped_{id}"),
            source: DataSourceId(id.into()),
            remote_schema: schema.clone(),
        });
        c.grants.push(Grant {
            principal: "fedengine".into(),
            source: DataSourceId(id.into()),
            schema: schema.clone(),
            object: "*".into(),
            actions: ["SELECT".into()].into(),
        });
    }
    for object in ["items", "literal_%'"] {
        c.grants.push(Grant {
            principal: "reader".into(),
            source: DataSourceId("pg".into()),
            schema: schema.clone(),
            object: object.into(),
            actions: ["SELECT".into()].into(),
        });
    }
    let mut sql = String::new();
    for i in 0..55 {
        sql.push_str(&format!(
            "CREATE TABLE pg.{schema}.paged_{i:02}(id INTEGER);"
        ));
    }
    sql.push_str(&format!(
        "CREATE TABLE pg.{schema}.\"literal_%'\"(id INTEGER);"
    ));
    admin.execute_batch(&sql).unwrap();
    admin
        .execute_batch(&format!(
            "CALL postgres_execute('pg',{});CALL postgres_execute('pg',{})",
            quote_literal(&format!(
                "CREATE VIEW {schema}.items_view AS SELECT id,label FROM {schema}.items"
            )),
            quote_literal(&format!(
                "CREATE INDEX items_label_idx ON {schema}.items(label)"
            ))
        ))
        .unwrap();
    let engine = FedEngine::start(c.clone()).await.unwrap();
    let api = engine.management_address().unwrap();
    for (id, kind) in [("pg", "postgres"), ("my", "mysql")] {
        operation(api,"sources",json!({"id":id,"kind":kind,"catalog_alias":format!("browser_{id}"),"secret_ref":format!("file:{id}"),"default_schema":schema,"allowed_schemas":[schema]})).await;
        let source = get(api, &format!("sources/{id}")).await;
        operation(api,&format!("sources/{id}/apply"),json!({"target":{"incarnation":source["incarnation"],"expected_revision":source["desired_revision"]}})).await;
    }
    let db = get(api, "virtual-databases/fedengine").await;
    let schemas = get(api, &browse(&db, "schemas")).await;
    let stamp = schemas["stamp"].as_str().unwrap();
    assert_eq!(schemas["schemas"].as_array().unwrap().len(), 3);
    let first = get(
        api,
        &format!("{}&schema=mapped_pg&stamp={stamp}", browse(&db, "objects")),
    )
    .await;
    assert_eq!(first["objects"].as_array().unwrap().len(), 50);
    assert_eq!(first["has_more"], true);
    let second = get(
        api,
        &format!(
            "{}&schema=mapped_pg&stamp={stamp}&offset=50",
            browse(&db, "objects")
        ),
    )
    .await;
    assert_eq!(second["objects"].as_array().unwrap().len(), 8);
    let special = get(
        api,
        &format!(
            "{}&schema=mapped_pg&search=literal_%25%27",
            browse(&db, "objects")
        ),
    )
    .await;
    assert_eq!(special["objects"].as_array().unwrap().len(), 1);
    assert_eq!(special["objects"][0]["name"], "literal_%'");
    let views = get(
        api,
        &format!("{}&schema=mapped_pg&kind=view", browse(&db, "objects")),
    )
    .await;
    assert_eq!(views["objects"].as_array().unwrap().len(), 1);
    for id in ["pg", "my"] {
        let detail = get(
            api,
            &format!("{}&schema=mapped_{id}&object=items", browse(&db, "object")),
        )
        .await;
        assert_eq!(detail["external"], true);
        assert_eq!(detail["source"]["schema"], schema);
        assert_eq!(detail["object"]["columns"].as_array().unwrap().len(), 2);
        assert!(
            !detail["object"]["metadata"]["constraints"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{detail}"
        );
        let result = query(api, &db, &format!("SELECT id,label FROM mapped_{id}.items")).await;
        assert_eq!(result["status"], "succeeded", "{result}");
        assert_eq!(result["result"]["rows"], json!([["1", "one"]]));
    }
    let detail = get(
        api,
        &format!(
            "{}&schema=mapped_pg&object=items_view",
            browse(&db, "object")
        ),
    )
    .await;
    assert!(
        detail["object"]["metadata"]["view_definition"]["source_text"]
            .as_str()
            .unwrap()
            .contains("SELECT")
    );
    admin
        .execute_batch(&format!(
            "CREATE TABLE pg.{schema}.after_refresh(id INTEGER)"
        ))
        .unwrap();
    let source = get(api, "sources/pg").await;
    operation(api,"sources/pg/refresh",json!({"target":{"incarnation":source["incarnation"],"expected_revision":source["desired_revision"]}})).await;
    assert_eq!(
        http(
            api,
            "GET",
            &format!(
                "{}&schema=mapped_pg&stamp={stamp}&offset=50",
                browse(&db, "objects")
            ),
            Value::Null
        )
        .await
        .0,
        409
    );
    let mut definition = db["config"].clone();
    definition["owner"] = json!("reader");
    mutate(api,json!({"target":{"oid":db["oid"],"expected_revision":db["revision"]},"definition":definition})).await;
    assert_eq!(
        http(api, "GET", &browse(&db, "schemas"), Value::Null)
            .await
            .0,
        409
    );
    let db = get(api, "virtual-databases/fedengine").await;
    let schemas = get(api, &browse(&db, "schemas")).await;
    assert_eq!(schemas["schemas"].as_array().unwrap().len(), 3); // Owner sees the local directory, including schemas without source SELECT.
    let allowed = get(api, &format!("{}&schema=mapped_pg", browse(&db, "objects"))).await;
    assert_eq!(
        allowed["objects"].as_array().unwrap().len(),
        50,
        "{allowed}"
    );
    assert_eq!(allowed["has_more"], true);
    assert!(
        allowed["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["name"] == "literal_%'")
    );
    let denied = query(api, &db, "SELECT * FROM mapped_pg.paged_00").await;
    assert_eq!(denied["status"], "failed");
    assert_eq!(denied["error"]["code"], "42501");
    let (status, detail) = http(
        api,
        "GET",
        &format!("{}&schema=mapped_pg&object=paged_00", browse(&db, "object")),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(detail["name"], "paged_00");
    assert_eq!(detail["external"], true);
    assert!(detail.get("object").is_none()); // No unauthorized source definitions or statistics.
    assert_eq!(detail["columns"]["rows"].as_array().unwrap().len(), 1);
    let privileges = query(api,&db,"SELECT has_table_privilege('mapped_pg.paged_00','SELECT'),has_table_privilege('mapped_pg.items','SELECT')").await;
    assert_eq!(privileges["status"], "succeeded", "{privileges}");
    assert_eq!(privileges["result"]["rows"][0], json!(["f", "t"]));
    let mut definition = db["config"].clone();
    definition["allow_connections"] = json!(false);
    mutate(api,json!({"target":{"oid":db["oid"],"expected_revision":db["revision"]},"definition":definition})).await;
    let db = get(api, "virtual-databases/fedengine").await;
    assert_eq!(
        http(api, "GET", &browse(&db, "schemas"), Value::Null)
            .await
            .0,
        409
    );
    engine.shutdown_until_drained().await.unwrap();
    admin
        .execute_batch(&format!(
            "CALL postgres_execute('pg',{}); CALL mysql_execute('my',{})",
            quote_literal(&format!("DROP SCHEMA {schema} CASCADE")),
            quote_literal(&format!("DROP DATABASE {schema}"))
        ))
        .unwrap();
}
