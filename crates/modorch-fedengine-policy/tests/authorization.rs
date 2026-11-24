use modorch_fedengine_core::*;
use modorch_fedengine_ddl::*;
fn configuration() -> Config {
    let config: Config =
        toml::from_str(include_str!("../../../examples/config/federation.toml")).unwrap();
    config.validate().unwrap();
    config
}
fn analyzer() -> Analyzer {
    Analyzer {
        namespace: None,
        sources: configuration().datasources,
        indexes: vec![],
    }
}
#[test]
fn default_deny_readonly_and_granular_ddl_capabilities() {
    use modorch_fedengine_policy::{Authorize, authorize};
    let mut c = configuration();
    let request = Authorize {
        principal: "fedengine".into(),
        analysis: analyzer().analyze("SELECT * FROM pg_crm.public.t").unwrap(),
    };
    assert!(authorize(&c, &request).is_ok());
    c.grants.clear();
    assert!(authorize(&c, &request).is_err());
    let mut c = configuration();
    c.datasources[0].read_only = true;
    let request = Authorize {
        principal: "fedengine".into(),
        analysis: analyzer()
            .analyze("INSERT INTO pg_crm.public.t VALUES(1)")
            .unwrap(),
    };
    assert!(authorize(&c, &request).is_err());
    c.datasources[0].read_only = false;
    let request = Authorize {
        principal: "fedengine".into(),
        analysis: analyzer()
            .analyze("ALTER TABLE pg_crm.public.t ADD COLUMN i INTEGER")
            .unwrap(),
    };
    assert!(authorize(&c, &request).is_err());
    c.capabilities.push(CapabilityRecord {
        source: DataSourceId("crm".into()),
        duckdb_version: "v2.0.0".into(),
        extension_version: "tested".into(),
        remote_version: "tested".into(),
        evidence: "unit policy decision only".into(),
        operations: ["ALTER_TABLE".into()].into(),
        index_adapter: false,
    });
    assert!(authorize(&c, &request).is_err());
    c.capabilities[0]
        .operations
        .insert("ALTER_TABLE_ADD_COLUMN".into());
    assert!(authorize(&c, &request).is_ok());
}

#[test]
fn starrocks_blocks_insert_and_ddl_with_0a000_even_when_writable() {
    use modorch_fedengine_policy::{Authorize, authorize};

    let mut c = configuration();
    c.datasources[0].kind = DataSourceKind::Starrocks;
    c.datasources[0].read_only = false;
    c.capabilities.push(CapabilityRecord {
        source: DataSourceId("crm".into()),
        duckdb_version: "v2.0.0".into(),
        extension_version: "tested".into(),
        remote_version: "tested".into(),
        evidence: "unit policy decision only".into(),
        operations: [
            "INSERT".into(),
            "CREATE_TABLE".into(),
            "ALTER_TABLE".into(),
            "ALTER_TABLE_ADD_COLUMN".into(),
        ]
        .into(),
        index_adapter: false,
    });
    let analyzer = Analyzer {
        namespace: None,
        sources: c.datasources.clone(),
        indexes: vec![],
    };

    for sql in [
        "INSERT INTO pg_crm.public.t VALUES(1)",
        "CREATE TABLE pg_crm.public.t2 (id INTEGER)",
        "ALTER TABLE pg_crm.public.t ADD COLUMN i INTEGER",
    ] {
        let request = Authorize {
            principal: "fedengine".into(),
            analysis: analyzer.analyze(sql).unwrap(),
        };
        let err = authorize(&c, &request).expect_err(sql);
        assert_eq!(err.sqlstate, "0A000", "{err:?}");
    }
}
