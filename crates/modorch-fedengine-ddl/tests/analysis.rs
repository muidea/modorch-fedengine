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
fn real_parser_splits_strings_comments_and_dollar_quotes() {
    assert_eq!(
        Analyzer::split("SELECT ';'; -- ;\n SELECT $$x;y$$;")
            .unwrap()
            .len(),
        2
    );
    assert!(Analyzer::split("SELECT 1; INSERT INTO pg_crm.public.t VALUES (1)").is_err());
    assert!(Analyzer::split("CREATE TABLE pg_crm.public.t(i INT); SELECT 1").is_err());
}

#[test]
fn cross_source_writes_and_dependencies_are_rejected() {
    let a = analyzer();
    for sql in [
        "INSERT INTO pg_crm.public.t SELECT * FROM mysql_erp.erp.t",
        "UPDATE pg_crm.public.t SET i=(SELECT max(i) FROM mysql_erp.erp.t)",
        "DELETE FROM pg_crm.public.t WHERE i IN (SELECT i FROM mysql_erp.erp.t)",
        "CREATE TABLE pg_crm.public.t AS SELECT * FROM mysql_erp.erp.t",
        "CREATE TABLE pg_crm.public.t (i INT REFERENCES mysql_erp.erp.t(i))",
        "CREATE TABLE pg_crm.public.t (i INT, FOREIGN KEY(i) REFERENCES mysql_erp.erp.t(i))",
        "CREATE INDEX mysql_erp.erp.x ON pg_crm.public.t(i)",
    ] {
        assert!(a.analyze(sql).is_err(), "must reject {sql}");
    }
}

#[test]
fn same_source_crud_ddl_and_federated_reads_bind_registered_identities() {
    let a = analyzer();
    for sql in [
        "SELECT * FROM pg_crm.public.t JOIN mysql_erp.erp.t USING(i)",
        "INSERT INTO pg_crm.public.t VALUES(1)",
        "UPDATE pg_crm.public.t SET i=2 WHERE i=1",
        "DELETE FROM mysql_erp.t WHERE i=1",
        "CREATE TABLE pg_crm.public.t(i INT)",
        "CREATE SCHEMA pg_crm.sales",
        "CREATE TABLE pg_crm.public.t AS SELECT * FROM pg_crm.public.s",
        "CREATE TABLE pg_crm.public.t(i INT REFERENCES pg_crm.public.s(i))",
        "CREATE INDEX x ON pg_crm.public.t(i)",
        "ALTER TABLE pg_crm.public.t ADD COLUMN s VARCHAR",
        "DROP TABLE pg_crm.public.t",
    ] {
        a.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        a.analyze("SELECT * FROM mysql_erp.t")
            .unwrap()
            .objects
            .iter()
            .next()
            .unwrap()
            .source,
        DataSourceId("erp".into())
    );
}

#[test]
fn reject_engine_administration_and_side_effect_functions_at_any_depth() {
    let a = analyzer();
    for sql in [
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "ATTACH ':memory:' AS evil",
        "COPY pg_crm.public.t TO '/tmp/leak'",
        "SELECT * FROM read_csv('/etc/passwd')",
        "SELECT postgres_execute('pg_crm','DROP TABLE t')",
        "WITH x AS (SELECT mysql_execute('mysql_erp', 'DROP TABLE t')) SELECT * FROM x",
        "SELECT * FROM pg_catalog.pg_authid",
        "SELECT * FROM unknown.public.t",
        "CREATE DATABASE x",
        "CREATE TEMP TABLE pg_crm.public.t(i INT)",
        "DROP TABLE pg_crm.public.t CASCADE",
        "CREATE TABLE pg_crm.public.t(i INT) LOCATION '/tmp/x'",
    ] {
        assert!(a.analyze(sql).is_err(), "must reject {sql}");
    }
}

#[test]
fn ambiguous_defaults_and_mysql_namespaces_fail_closed() {
    let a = analyzer();
    assert!(a.analyze("SELECT * FROM t").is_err());
    assert!(a.analyze("CREATE SCHEMA mysql_erp.unauthorized").is_err());
    let sql =
        "WITH x AS (SELECT * FROM pg_crm.public.t) SELECT * FROM x JOIN mysql_erp.erp.t USING(i)";
    assert_eq!(a.analyze(sql).unwrap().sources.len(), 2);
}

#[test]
fn index_drop_requires_unique_real_table_ownership() {
    let mut a = analyzer();
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_err());
    let table = ObjectRef {
        source: DataSourceId("crm".into()),
        catalog: "pg_crm".into(),
        schema: "public".into(),
        name: "t".into(),
    };
    let index = ObjectRef {
        name: "x".into(),
        ..table.clone()
    };
    let owner = IndexOwner {
        index,
        table,
        protected: false,
    };
    a.indexes.push(owner.clone());
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_ok());
    a.indexes.push(owner);
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_err());
    a.indexes.pop();
    a.indexes[0].protected = true;
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_err());
}

#[test]
fn equal_physical_hosts_do_not_merge_datasource_ids() {
    let mut a = analyzer();
    let mut second = a.sources[0].clone();
    second.id = DataSourceId("crm2".into());
    second.catalog_alias = "pg_crm2".into();
    a.sources.push(second);
    assert!(
        a.analyze("CREATE TABLE pg_crm.public.t AS SELECT * FROM pg_crm2.public.t")
            .is_err()
    );
}

#[test]
fn custom_types_nested_writes_and_fake_builtin_tables_are_rejected() {
    let a = analyzer();
    for sql in [
        "CREATE TABLE pg_crm.public.t(i unknown.public.secret)",
        "ALTER TABLE pg_crm.public.t ALTER COLUMN i ADD GENERATED ALWAYS AS IDENTITY",
        "WITH x AS (DELETE FROM pg_crm.public.t) SELECT * FROM x",
        "SELECT * FROM range",
    ] {
        assert!(a.analyze(sql).is_err(), "must reject {sql}");
    }
    assert!(
        a.analyze("CREATE TABLE pg_crm.public.t(i INTEGER PRIMARY KEY)")
            .is_ok()
    );
}

#[test]
fn virtual_schema_bindings_keep_current_database_and_physical_source_distinct() {
    let mut a = analyzer();
    a.namespace = Some(BindingNamespace {
        routes: None,
        catalog: "fedengine".into(),
        allow_cross_database: false,
        search_path: vec!["missing".into(), "b".into(), "a".into()],
        relations: [("a".into(), "t".into()), ("b".into(), "t".into())].into(),
        bindings: [
            ("a".into(), (DataSourceId("crm".into()), "public".into())),
            ("b".into(), (DataSourceId("erp".into()), "erp".into())),
        ]
        .into(),
    });
    let analysis = a
        .analyze("SELECT a.t.i, fedengine.b.t.* FROM a.t JOIN fedengine.b.t USING(i)")
        .unwrap();
    assert_eq!(analysis.sources.len(), 2);
    assert!(analysis.sql.contains("\"pg_crm\".\"public\".\"t\""));
    assert!(analysis.sql.contains("\"mysql_erp\".\"erp\".\"t\""));
    assert_eq!(
        a.analyze("SELECT * FROM t")
            .unwrap()
            .objects
            .iter()
            .next()
            .unwrap()
            .source
            .0,
        "erp"
    );
    assert_eq!(
        a.analyze("SELECT * FROM pg_crm.public.t")
            .unwrap_err()
            .sqlstate,
        "0A000"
    );
    assert!(a.analyze("SELECT * FROM \"A\".t").is_err());
    a.namespace.as_mut().unwrap().allow_cross_database = true;
    assert!(a.analyze("SELECT * FROM pg_crm.public.t").is_ok());
}

#[test]
fn internal_custom_extension_functions_are_denied() {
    let a = analyzer();
    for func in [
        "postgres_crud_metadata",
        "postgres_guarded_mutation",
        "postgres_guarded_insert_returning",
        "postgres_ddl_capabilities",
        "postgres_ddl_describe",
        "postgres_guarded_ddl",
        "pg_clear_cache",
        "postgres_query",
        "postgres_execute",
        "mysql_crud_metadata",
        "mysql_guarded_mutation",
        "mysql_guarded_insert_returning",
        "mysql_ddl_capabilities",
        "mysql_ddl_describe",
        "mysql_guarded_ddl",
        "mysql_clear_cache",
        "mysql_query",
        "mysql_execute",
        "oracle_crud_metadata",
        "oracle_guarded_mutation",
        "oracle_relational_metadata",
        "oracle_clear_cache",
        "oracle_query",
        "oracle_execute",
        "sqlserver_crud_metadata",
        "sqlserver_guarded_mutation",
        "sqlserver_relational_metadata",
        "sqlserver_clear_cache",
        "sqlserver_query",
        "sqlserver_execute",
        "dameng_crud_metadata",
        "dameng_guarded_mutation",
        "db2_crud_metadata",
        "db2_guarded_mutation",
        "odbc_relational_metadata",
        "odbc_catalog_clear_cache",
        "odbc_query",
        "odbc_execute",
    ] {
        assert!(
            a.analyze(&format!("SELECT {func}('x')")).is_err(),
            "scalar call to {func} must be denied"
        );
        assert!(
            a.analyze(&format!("SELECT * FROM {func}('x')")).is_err(),
            "table function call to {func} must be denied"
        );
        assert!(
            a.analyze(&format!("CALL {func}('x')")).is_err(),
            "CALL to {func} must be denied"
        );
    }
}

#[test]
fn invalid_and_unsupported_casts_are_rejected() {
    let a = analyzer();
    for (sql, state) in [
        ("SELECT '{\"a\":1}'::JSONB", "0A000"),
        ("SELECT '{\"a\":1}'::pg_catalog.jsonb", "0A000"),
        ("SELECT 1::pg_catalog.integer", "42704"),
        ("SELECT 1::pg_catalog.bigint", "42704"),
        ("SELECT 1::pg_catalog.decimal", "42704"),
        ("SELECT 1::custom_schema.custom_type", "42704"),
        ("SELECT 1::money", "0A000"),
        ("SELECT '1.2.3.4'::inet", "0A000"),
        ("SELECT 1::DECIMAL(39,0)", "0A000"),
        ("SELECT 1::DECIMAL(10,11)", "22023"),
        ("SELECT ARRAY[ARRAY[1]]::INTEGER[][]", "0A000"),
    ] {
        let err = a.analyze(sql).expect_err(sql);
        assert_eq!(err.sqlstate, state, "{sql}");
    }
    for ok_sql in [
        "SELECT 1::DECIMAL(38,6)",
        "SELECT 1::pg_catalog.int4",
        "SELECT '1 day'::INTERVAL",
        "SELECT '550e8400-e29b-41d4-a716-446655440000'::UUID",
    ] {
        a.analyze(ok_sql).unwrap();
    }
}

#[test]
fn mixed_cast_and_bare_parameter_rewrites_only_bare_occurrences() {
    let (compact, slots) =
        modorch_fedengine_ddl::compact_parameters("SELECT $1::numeric + $3::int + $3, $1").unwrap();
    assert_eq!(slots, vec![0, 2]);
    let rewritten = modorch_fedengine_ddl::cast_parameters(
        &compact,
        &["DECIMAL(7,4)".into(), "INTEGER".into()],
    )
    .unwrap();
    assert!(rewritten.contains("DECIMAL(7,4)"), "{rewritten}");
    assert!(rewritten.contains("CAST($2 AS INTEGER)"), "{rewritten}");
}

#[test]
fn analyzed_virtual_schema_mysql_query_pushes_down_with_remote_schema() {
    let mut a = Analyzer {
        namespace: None,
        sources: vec![DataSourceDefinition {
            id: DataSourceId("mysql_src".into()),
            kind: DataSourceKind::Mariadb,
            catalog_alias: "mdb_crm".into(),
            secret_ref: "unused".into(),
            organization: None,
            default_schema: "crm".into(),
            read_only: true,
            ddl_enabled: false,
            allowed_schemas: vec!["crm".into()],
            enabled: true,
        }],
        indexes: vec![],
    };
    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert(
        "sales".into(),
        (DataSourceId("mysql_src".into()), "crm".into()),
    );
    let mut routes = std::collections::BTreeMap::new();
    routes.insert(
        ("sales".into(), "orders".into()),
        (DataSourceId("mysql_src".into()), "crm".into()),
    );
    a.namespace = Some(BindingNamespace {
        catalog: "vdb".into(),
        allow_cross_database: false,
        bindings,
        routes: Some(routes),
        search_path: vec!["sales".into()],
        relations: RelationNames::from([("sales".into(), "orders".into())]),
    });
    let analysis = a.analyze("SELECT o.id, o.id FROM orders o").unwrap();
    let pushed =
        modorch_fedengine_ddl::rewrite_pushdown(&analysis.sql, &analysis.sources, &a.sources)
            .unwrap();
    assert!(pushed.contains("mysql_query('mdb_crm',"), "{pushed}");
    assert!(pushed.contains("`crm`.`orders`"), "{pushed}");
    assert!(pushed.contains("\"__fed_output_0\" AS \"id\""), "{pushed}");
}

#[test]
fn guarded_mutation_and_insert_returning_contracts() {
    let a = analyzer();
    let ok = a
        .analyze("INSERT INTO pg_crm.public.items (id, name) VALUES ($1, $2) RETURNING id")
        .unwrap();
    assert_eq!(ok.kind, Kind::Insert);
    assert_eq!(ok.returning.as_ref().unwrap().column, "id");
    assert_eq!(ok.primary.as_ref().unwrap().name, "items");

    for bad in [
        "INSERT INTO pg_crm.public.items VALUES (1, 'a') RETURNING *",
        "INSERT INTO pg_crm.public.items VALUES (1, 'a') RETURNING id, name",
        "INSERT INTO pg_crm.public.items VALUES (1, 'a') RETURNING id + 1",
    ] {
        assert_eq!(a.analyze(bad).unwrap_err().sqlstate, "0A000", "{bad}");
    }

    let pg_source = &a.sources[0];
    let pg_compact = modorch_fedengine_ddl::compact_parameters(&ok.sql)
        .unwrap()
        .0;
    let pg_plan = modorch_fedengine_ddl::build_guarded_mutation_plan(
        &pg_compact,
        pg_source,
        ok.primary.as_ref().unwrap(),
        ok.returning.as_ref(),
    )
    .unwrap()
    .unwrap();
    assert!(pg_plan.native_sql.contains("\"public\".\"items\""));
    assert!(pg_plan.native_sql.contains("RETURNING id"));
    assert!(pg_plan.native_sql.contains("$1"));

    let my_ok = a
        .analyze("INSERT INTO mysql_erp.erp.items (id, name) VALUES ($1, $2) RETURNING id")
        .unwrap();
    let my_source = &a.sources[1];
    let my_compact = modorch_fedengine_ddl::compact_parameters(&my_ok.sql)
        .unwrap()
        .0;
    let my_plan = modorch_fedengine_ddl::build_guarded_mutation_plan(
        &my_compact,
        my_source,
        my_ok.primary.as_ref().unwrap(),
        my_ok.returning.as_ref(),
    )
    .unwrap()
    .unwrap();
    assert!(my_plan.native_sql.contains("`erp`.`items`"));
    assert!(!my_plan.native_sql.contains("RETURNING"));
    assert!(my_plan.native_sql.contains("VALUES (?, ?)"));
    assert_eq!(my_plan.returning.unwrap().column, "id");
}

#[test]
fn guarded_ddl_v2_through_v5_intents_and_dialect_bounds() {
    let mut a = analyzer();
    let table = ObjectRef {
        source: DataSourceId("crm".into()),
        catalog: "pg_crm".into(),
        schema: "public".into(),
        name: "items".into(),
    };
    let idx = ObjectRef {
        name: "items_idx".into(),
        ..table.clone()
    };
    a.indexes.push(IndexOwner {
        index: idx,
        table,
        protected: false,
    });

    let rename_tbl = a
        .analyze("ALTER TABLE pg_crm.public.items RENAME TO items2")
        .unwrap();
    assert_eq!(rename_tbl.capability, "ALTER_TABLE_RENAME_TABLE");
    let plan_v3 = rename_tbl
        .ddl_intent
        .unwrap()
        .into_guarded_plan(
            &a.sources[0],
            "crmdb".into(),
            "public".into(),
            String::new(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(plan_v3.contract_version, 3);
    assert_eq!(plan_v3.action, "rename_table");
    assert_eq!(plan_v3.new_name, "items2");

    let rename_idx = a
        .analyze("ALTER INDEX pg_crm.public.items_idx RENAME TO items_idx2")
        .unwrap();
    assert_eq!(rename_idx.capability, "RENAME_INDEX");
    let idx_plan = rename_idx
        .ddl_intent
        .unwrap()
        .into_guarded_plan(
            &a.sources[0],
            "crmdb".into(),
            "public".into(),
            "items".into(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(idx_plan.contract_version, 3);
    assert_eq!(idx_plan.owner, "items");

    let add_dec = a
        .analyze("ALTER TABLE pg_crm.public.items ADD COLUMN amount DECIMAL(18,4)")
        .unwrap();
    let plan_v5 = add_dec
        .ddl_intent
        .unwrap()
        .into_guarded_plan(
            &a.sources[0],
            "crmdb".into(),
            "public".into(),
            String::new(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(plan_v5.contract_version, 5);
    assert_eq!(plan_v5.columns[0].precision, 18);
    assert_eq!(plan_v5.columns[0].scale, 4);

    let add_chk = a
        .analyze("ALTER TABLE pg_crm.public.items ADD CONSTRAINT chk_pos CHECK (amount > 0)")
        .unwrap();
    assert_eq!(add_chk.capability, "ALTER_TABLE_ADD_CONSTRAINT");
    let plan_v4 = add_chk
        .ddl_intent
        .unwrap()
        .into_guarded_plan(
            &a.sources[0],
            "crmdb".into(),
            "public".into(),
            String::new(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(plan_v4.contract_version, 4);
    assert_eq!(plan_v4.constraint.unwrap().kind, "check");

    let my_not_null = a
        .analyze("ALTER TABLE mysql_erp.erp.items ALTER COLUMN name SET NOT NULL")
        .unwrap();
    assert!(
        my_not_null
            .ddl_intent
            .unwrap()
            .into_guarded_plan(&a.sources[1], "erp".into(), "erp".into(), String::new())
            .is_err()
    );
}
