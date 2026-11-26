use modorch_fedengine_contract::PgType;
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
        &[PgType::Int4, PgType::Text],
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
        &[PgType::Int4, PgType::Text],
    )
    .unwrap()
    .unwrap();
    assert!(my_plan.native_sql.contains("`erp`.`items`"));
    assert!(!my_plan.native_sql.contains("RETURNING"));
    assert!(my_plan.native_sql.contains("VALUES (?, ?)"));
    assert_eq!(my_plan.returning.unwrap().column, "id");

    let my_cast = a
        .analyze("INSERT INTO mysql_erp.erp.items (id, name) VALUES ($1::INTEGER, $2::VARCHAR)")
        .unwrap();
    let my_cast_plan = modorch_fedengine_ddl::build_guarded_mutation_plan(
        &my_cast.sql,
        my_source,
        my_cast.primary.as_ref().unwrap(),
        None,
        &[PgType::Int4, PgType::Text],
    )
    .unwrap()
    .unwrap();
    assert!(
        my_cast_plan.native_sql.contains("VALUES (?, ?)"),
        "{}",
        my_cast_plan.native_sql
    );
    assert!(
        !my_cast_plan.native_sql.contains("::"),
        "{}",
        my_cast_plan.native_sql
    );
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
    let my_nn_plan = my_not_null
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&a.sources[1], "erp".into(), "erp".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(my_nn_plan.contract_version, 4);
    assert_eq!(my_nn_plan.action, "set_not_null");
}

#[test]
fn pg_catalog_json_cast_unqualifies_to_json() {
    let a = analyzer();
    let res = a.analyze("SELECT '{\"a\":1}'::pg_catalog.json").unwrap();
    assert!(res.sql.contains("JSON"), "{}", res.sql);
    assert!(!res.sql.contains("pg_catalog.json"), "{}", res.sql);
}

#[test]
fn mariadb_guarded_mutation_and_ddl_reuse_mysql_contract() {
    let source = DataSourceDefinition {
        id: DataSourceId("mdb".into()),
        kind: DataSourceKind::Mariadb,
        catalog_alias: "mdb_erp".into(),
        secret_ref: "unused".into(),
        organization: None,
        default_schema: "erp".into(),
        read_only: false,
        ddl_enabled: true,
        allowed_schemas: vec!["erp".into()],
        enabled: true,
    };
    let a = Analyzer {
        namespace: None,
        sources: vec![source.clone()],
        indexes: vec![],
    };
    let ins = a
        .analyze("INSERT INTO mdb_erp.erp.items (id, name) VALUES ($1, $2) RETURNING id")
        .unwrap();
    let mut_plan = modorch_fedengine_ddl::build_guarded_mutation_plan(
        &ins.sql,
        &source,
        ins.primary.as_ref().unwrap(),
        ins.returning.as_ref(),
        &[PgType::Int4, PgType::Text],
    )
    .unwrap()
    .unwrap();
    assert!(
        mut_plan.native_sql.contains("`erp`.`items`"),
        "{}",
        mut_plan.native_sql
    );
    assert!(
        !mut_plan
            .native_sql
            .to_ascii_uppercase()
            .contains("RETURNING"),
        "{}",
        mut_plan.native_sql
    );
    assert_eq!(mut_plan.returning.unwrap().column, "id");

    let ddl = a
        .analyze("CREATE TABLE mdb_erp.erp.notes (id INTEGER, body TEXT)")
        .unwrap();
    let ddl_plan = ddl
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&source, "erp".into(), "erp".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(ddl_plan.contract_version, 5);

    let mdb_not_null = a
        .analyze("ALTER TABLE mdb_erp.erp.items ALTER COLUMN name SET NOT NULL")
        .unwrap();
    let mdb_nn_plan = mdb_not_null
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&source, "erp".into(), "erp".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(mdb_nn_plan.contract_version, 4);
    assert_eq!(mdb_nn_plan.action, "set_not_null");
}

#[test]
fn reindex_v5_ddl_starrocks_gate_and_oracle_mutation_rewrites() {
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

    let reindex = a.analyze("REINDEX INDEX pg_crm.public.items_idx").unwrap();
    assert_eq!(reindex.action, "REINDEX");
    assert_eq!(reindex.capability, "REINDEX");
    let reindex_plan = reindex
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
    assert_eq!(reindex_plan.contract_version, 4);
    assert_eq!(reindex_plan.action, "reindex");
    assert_eq!(reindex_plan.owner, "items");
    assert!(a.analyze("REINDEX TABLE pg_crm.public.items").is_err());

    let drop_col = a
        .analyze("ALTER TABLE pg_crm.public.items DROP COLUMN note")
        .unwrap()
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
    assert_eq!(drop_col.contract_version, 5);
    assert_eq!(drop_col.action, "drop_column");
    assert_eq!(drop_col.column_name, "note");

    let alter_col = a
        .analyze("ALTER TABLE mysql_erp.erp.items ALTER COLUMN amount TYPE DECIMAL(18, 4)")
        .unwrap()
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&a.sources[1], "erp".into(), "erp".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(alter_col.contract_version, 5);
    assert_eq!(alter_col.action, "alter_column_type");
    assert_eq!(alter_col.column_name, "amount");
    assert_eq!(alter_col.columns[0].precision, 18);

    let create_schema = a
        .analyze("CREATE SCHEMA pg_crm.sales")
        .unwrap()
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&a.sources[0], "crmdb".into(), "sales".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(create_schema.contract_version, 5);
    assert_eq!(create_schema.action, "create_schema");
    assert_eq!(create_schema.table_name, "sales");

    let oracle_source = DataSourceDefinition {
        id: DataSourceId("ora".into()),
        kind: DataSourceKind::Oracle,
        catalog_alias: "ora_db".into(),
        secret_ref: "unused".into(),
        organization: None,
        default_schema: "HR".into(),
        read_only: false,
        ddl_enabled: true,
        allowed_schemas: vec!["HR".into()],
        enabled: true,
    };
    let hana_source = DataSourceDefinition {
        id: DataSourceId("hana".into()),
        kind: DataSourceKind::Hana,
        catalog_alias: "hana_db".into(),
        secret_ref: "unused".into(),
        organization: None,
        default_schema: "APP".into(),
        read_only: false,
        ddl_enabled: true,
        allowed_schemas: vec!["APP".into()],
        enabled: true,
    };
    let sr_source = DataSourceDefinition {
        id: DataSourceId("sr".into()),
        kind: DataSourceKind::Starrocks,
        catalog_alias: "sr_db".into(),
        secret_ref: "unused".into(),
        organization: None,
        default_schema: "analytics".into(),
        read_only: false,
        ddl_enabled: true,
        allowed_schemas: vec!["analytics".into()],
        enabled: true,
    };
    let ext_analyzer = Analyzer {
        namespace: None,
        sources: vec![
            oracle_source.clone(),
            hana_source.clone(),
            sr_source.clone(),
        ],
        indexes: vec![],
    };

    let ora_ins = ext_analyzer
        .analyze("INSERT INTO ora_db.\"HR\".events (id, created_at) VALUES ($1, $2), ($3, $4)")
        .unwrap();
    let ora_plan = modorch_fedengine_ddl::build_guarded_mutation_plan(
        &modorch_fedengine_ddl::compact_parameters(&ora_ins.sql)
            .unwrap()
            .0,
        &oracle_source,
        ora_ins.primary.as_ref().unwrap(),
        None,
        &[
            PgType::Int8,
            PgType::Timestamp,
            PgType::Int8,
            PgType::Timestamp,
        ],
    )
    .unwrap()
    .unwrap();
    assert!(
        ora_plan.native_sql.starts_with("INSERT ALL "),
        "{}",
        ora_plan.native_sql
    );
    assert!(
        ora_plan
            .native_sql
            .contains("TO_TIMESTAMP(?, 'YYYY-MM-DD HH24:MI:SS.FF6')"),
        "{}",
        ora_plan.native_sql
    );
    assert!(
        ora_plan.native_sql.ends_with("SELECT 1 FROM DUAL"),
        "{}",
        ora_plan.native_sql
    );

    let hana_ins = ext_analyzer
        .analyze("INSERT INTO hana_db.\"APP\".items (id) VALUES ($1)")
        .unwrap();
    let hana_plan = modorch_fedengine_ddl::build_guarded_mutation_plan(
        &modorch_fedengine_ddl::compact_parameters(&hana_ins.sql)
            .unwrap()
            .0,
        &hana_source,
        hana_ins.primary.as_ref().unwrap(),
        None,
        &[PgType::Int8],
    )
    .unwrap()
    .unwrap();
    assert!(hana_plan.native_sql.contains("\"APP\".\"items\""));

    let sr_ins = ext_analyzer
        .analyze("INSERT INTO sr_db.analytics.items (id) VALUES ($1)")
        .unwrap();
    assert!(
        modorch_fedengine_ddl::build_guarded_mutation_plan(
            &modorch_fedengine_ddl::compact_parameters(&sr_ins.sql)
                .unwrap()
                .0,
            &sr_source,
            sr_ins.primary.as_ref().unwrap(),
            None,
            &[PgType::Int8],
        )
        .is_err()
    );
}

fn test_source(id: &str, kind: DataSourceKind, alias: &str, schema: &str) -> DataSourceDefinition {
    DataSourceDefinition {
        id: DataSourceId(id.into()),
        kind,
        catalog_alias: alias.into(),
        secret_ref: "unused".into(),
        organization: None,
        default_schema: schema.into(),
        read_only: false,
        ddl_enabled: true,
        allowed_schemas: vec![schema.into()],
        enabled: true,
    }
}

#[test]
fn ddl_v6_constraints_conditional_and_ctas_across_dialects() {
    let pg = test_source("pg", DataSourceKind::Postgres, "pg_s", "public");
    let my = test_source("my", DataSourceKind::Mysql, "my_s", "app");
    let mdb = test_source("mdb", DataSourceKind::Mariadb, "mdb_s", "app");
    let ora = test_source("ora", DataSourceKind::Oracle, "ora_s", "HR");
    let mssql = test_source("ms", DataSourceKind::Sqlserver, "ms_s", "dbo");
    let dm = test_source("dm", DataSourceKind::Dameng, "dm_s", "DM");
    let db2 = test_source("db2", DataSourceKind::Db2, "db2_s", "DB2INST1");
    let hana = test_source("hana", DataSourceKind::Hana, "hana_s", "APP");

    let sources = vec![
        pg.clone(),
        my.clone(),
        mdb.clone(),
        ora.clone(),
        mssql.clone(),
        dm.clone(),
        db2.clone(),
        hana.clone(),
    ];
    let mut a = Analyzer {
        namespace: None,
        sources,
        indexes: vec![],
    };

    // v6 CREATE TABLE with inline column options + table constraints
    let ct = a
        .analyze(
            "CREATE TABLE IF NOT EXISTS pg_s.public.orders (\
             id BIGINT PRIMARY KEY, \
             code VARCHAR(32) NOT NULL UNIQUE DEFAULT 'new', \
             parent_id BIGINT, \
             qty INTEGER NOT NULL DEFAULT 1, \
             FOREIGN KEY (parent_id) REFERENCES pg_s.public.parents(id), \
             CHECK (qty > 0))",
        )
        .unwrap();
    let ct_plan = ct
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&pg, "db".into(), "public".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(ct_plan.contract_version, 6);
    assert!(ct_plan.conditional);
    assert_eq!(ct_plan.columns.len(), 4);
    assert!(ct_plan.columns[0].primary_key);
    assert!(ct_plan.columns[1].not_null);
    assert!(ct_plan.columns[1].unique);
    assert_eq!(ct_plan.columns[1].default_kind.as_deref(), Some("string"));
    assert_eq!(ct_plan.columns[1].default_value.as_deref(), Some("new"));
    assert!(ct_plan.columns[3].not_null);
    assert_eq!(ct_plan.columns[3].default_kind.as_deref(), Some("integer"));
    assert_eq!(ct_plan.columns[3].default_value.as_deref(), Some("1"));
    assert_eq!(ct_plan.constraints.len(), 2);
    assert_eq!(ct_plan.constraints[0].kind, "foreign_key");
    assert_eq!(ct_plan.constraints[1].kind, "check");

    // v6 ADD COLUMN with inline modifiers
    let add_col = a
        .analyze(
            "ALTER TABLE my_s.app.orders ADD COLUMN IF NOT EXISTS note VARCHAR(64) NOT NULL DEFAULT 'ok'",
        )
        .unwrap();
    let add_plan = add_col
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&my, "app".into(), "app".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(add_plan.contract_version, 6);
    assert!(add_plan.conditional);
    assert_eq!(add_plan.columns.len(), 1);
    assert!(add_plan.columns[0].not_null);
    assert_eq!(add_plan.columns[0].default_kind.as_deref(), Some("string"));

    // Conditional DDL actions + DROP NOT NULL + DROP INDEX IF EXISTS
    for (src, db, schema, sql) in [
        (&my, "app", "app", "DROP TABLE IF EXISTS my_s.app.orders"),
        (&my, "app", "app", "CREATE SCHEMA IF NOT EXISTS my_s.app"),
        (&my, "app", "app", "DROP SCHEMA IF EXISTS my_s.app"),
        (
            &my,
            "app",
            "app",
            "ALTER TABLE my_s.app.orders DROP COLUMN IF EXISTS note",
        ),
        (
            &my,
            "app",
            "app",
            "CREATE INDEX IF NOT EXISTS idx_o ON my_s.app.orders (code)",
        ),
        (
            &my,
            "app",
            "app",
            "ALTER TABLE my_s.app.orders ALTER COLUMN qty DROP NOT NULL",
        ),
    ] {
        let p = a
            .analyze(sql)
            .unwrap()
            .ddl_intent
            .unwrap()
            .into_guarded_plan(src, db.into(), schema.into(), String::new())
            .unwrap()
            .unwrap();
        if sql.contains("IF ") {
            assert!(p.conditional, "{sql}");
        } else {
            assert_eq!(p.action, "drop_not_null");
        }
    }
    let drop_missing_ok = a
        .analyze("DROP INDEX IF EXISTS pg_s.public.missing_idx")
        .unwrap();
    let drop_missing_plan = drop_missing_ok
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&pg, "db".into(), "public".into(), String::new())
        .unwrap()
        .unwrap();
    assert!(drop_missing_plan.conditional);
    assert_eq!(
        a.analyze("DROP INDEX pg_s.public.missing_idx")
            .unwrap_err()
            .sqlstate,
        "42704"
    );
    a.indexes.push(IndexOwner {
        index: ObjectRef {
            source: pg.id.clone(),
            catalog: "pg_s".into(),
            schema: "public".into(),
            name: "existing_idx".into(),
        },
        table: ObjectRef {
            source: pg.id.clone(),
            catalog: "pg_s".into(),
            schema: "public".into(),
            name: "orders".into(),
        },
        protected: false,
    });
    assert!(
        a.analyze("DROP INDEX IF EXISTS pg_s.public.existing_idx")
            .is_ok()
    );

    // Expanded column types per dialect & rejections
    for (src, db, schema, col_def) in [
        (
            &my,
            "app",
            "app",
            "flag BOOLEAN, at_t TIME, ts TIMESTAMPTZ, raw_b BYTEA",
        ),
        (
            &mdb,
            "app",
            "app",
            "flag BOOLEAN, at_t TIME, ts TIMESTAMPTZ, raw_b BYTEA",
        ),
        (&ora, "ORCL", "HR", "raw_b BYTEA, ts TIMESTAMPTZ"),
        (&mssql, "master", "dbo", "ts TIMESTAMPTZ, uid UUID"),
        (
            &dm,
            "DAMENG",
            "DM",
            "flag BOOLEAN, at_t TIME, ts TIMESTAMPTZ, raw_b BYTEA",
        ),
        (
            &db2,
            "TESTDB",
            "DB2INST1",
            "flag BOOLEAN, at_t TIME(0), raw_b BYTEA",
        ),
        (
            &hana,
            "HDB",
            "APP",
            "flag BOOLEAN, at_t TIME(0), raw_b BYTEA",
        ),
    ] {
        let stmt = format!(
            "CREATE TABLE {}.\"{schema}\".t ({col_def})",
            src.catalog_alias
        );
        let plan = a
            .analyze(&stmt)
            .unwrap()
            .ddl_intent
            .unwrap()
            .into_guarded_plan(src, db.into(), schema.into(), String::new())
            .unwrap()
            .unwrap();
        assert_eq!(plan.contract_version, 5, "{stmt}");
    }
    for (src, db, schema, col_def) in [
        (&my, "app", "app", "uid UUID"),
        (&mdb, "app", "app", "uid UUID"),
        (&ora, "ORCL", "HR", "flag BOOLEAN"),
        (&ora, "ORCL", "HR", "uid UUID"),
        (&ora, "ORCL", "HR", "at_t TIME"),
        (&dm, "DAMENG", "DM", "uid UUID"),
        (&db2, "TESTDB", "DB2INST1", "uid UUID"),
        (&db2, "TESTDB", "DB2INST1", "ts TIMESTAMPTZ"),
        (&db2, "TESTDB", "DB2INST1", "at_t TIME"),
        (&db2, "TESTDB", "DB2INST1", "at_t TIME(3)"),
        (&hana, "HDB", "APP", "uid UUID"),
        (&hana, "HDB", "APP", "ts TIMESTAMPTZ"),
        (&hana, "HDB", "APP", "at_t TIME"),
        (&hana, "HDB", "APP", "at_t TIME(6)"),
    ] {
        let stmt = format!(
            "CREATE TABLE {}.\"{schema}\".t ({col_def})",
            src.catalog_alias
        );
        let parsed = a.analyze(&stmt).unwrap();
        assert!(
            parsed
                .ddl_intent
                .unwrap()
                .into_guarded_plan(src, db.into(), schema.into(), String::new())
                .is_err(),
            "expected error for {stmt}"
        );
    }

    // ODBC 5-source CTAS -> v6 create_table_as with rewritten query_sql; PG/MySQL -> Ok(None)
    for (src, db, schema) in [
        (&ora, "ORCL", "HR"),
        (&mssql, "master", "dbo"),
        (&dm, "DAMENG", "DM"),
        (&db2, "TESTDB", "DB2INST1"),
        (&hana, "HDB", "APP"),
    ] {
        let sql = format!(
            "CREATE TABLE {0}.\"{1}\".copy_t AS SELECT id, name FROM {0}.\"{1}\".src_t WHERE id > 0",
            src.catalog_alias, schema
        );
        let analyzed = a.analyze(&sql).unwrap();
        let plan = analyzed
            .ddl_intent
            .unwrap()
            .into_guarded_plan(src, db.into(), schema.into(), String::new())
            .unwrap()
            .unwrap();
        assert_eq!(plan.contract_version, 6);
        assert_eq!(plan.action, "create_table_as");
        let q = plan.query_sql.unwrap();
        assert!(!q.contains(&src.catalog_alias), "{q}");
        assert!(q.contains("src_t"), "{q}");
    }
    let pg_ctas = a
        .analyze("CREATE TABLE pg_s.public.copy_t AS SELECT id FROM pg_s.public.src_t")
        .unwrap();
    assert!(
        pg_ctas
            .ddl_intent
            .unwrap()
            .into_guarded_plan(&pg, "db".into(), "public".into(), String::new())
            .unwrap()
            .is_none()
    );
    let ms_cte = a
        .analyze(
            "CREATE TABLE ms_s.dbo.copy_t AS WITH c AS (SELECT id FROM ms_s.dbo.src_t) SELECT id FROM c",
        )
        .unwrap();
    assert!(
        ms_cte
            .ddl_intent
            .unwrap()
            .into_guarded_plan(&mssql, "master".into(), "dbo".into(), String::new())
            .is_err()
    );
    let ora_param_ctas = a.analyze(
        "CREATE TABLE ora_s.\"HR\".copy_t AS SELECT id FROM ora_s.\"HR\".src_t WHERE id = $1",
    );
    if let Ok(parsed) = ora_param_ctas {
        assert!(
            parsed
                .ddl_intent
                .unwrap()
                .into_guarded_plan(&ora, "ORCL".into(), "HR".into(), String::new())
                .is_err()
        );
    }
}

#[test]
fn guarded_mutation_param_reuse_reorder_and_all_dialect_returning() {
    let pg = test_source("pg", DataSourceKind::Postgres, "pg_s", "public");
    let gp = test_source("gp", DataSourceKind::Greenplum, "gp_s", "public");
    let kb = test_source("kb", DataSourceKind::Kingbasees, "kb_s", "public");
    let my = test_source("my", DataSourceKind::Mysql, "my_s", "app");
    let mdb = test_source("mdb", DataSourceKind::Mariadb, "mdb_s", "app");
    let ora = test_source("ora", DataSourceKind::Oracle, "ora_s", "HR");
    let mssql = test_source("ms", DataSourceKind::Sqlserver, "ms_s", "dbo");
    let dm = test_source("dm", DataSourceKind::Dameng, "dm_s", "DM");
    let db2 = test_source("db2", DataSourceKind::Db2, "db2_s", "DB2INST1");
    let hana = test_source("hana", DataSourceKind::Hana, "hana_s", "APP");

    let a = Analyzer {
        namespace: None,
        sources: vec![
            pg.clone(),
            gp.clone(),
            kb.clone(),
            my.clone(),
            mdb.clone(),
            ora.clone(),
            mssql.clone(),
            dm.clone(),
            db2.clone(),
            hana.clone(),
        ],
        indexes: vec![],
    };

    // Out-of-order ($2 ... $1) and reuse ($1 ... $1) on ?-placeholder dialect
    let upd_reorder = a
        .analyze("UPDATE my_s.app.items SET name = $2, note = $1 WHERE id = $1")
        .unwrap();
    let (sql, _) = modorch_fedengine_ddl::compact_parameters(&upd_reorder.sql).unwrap();
    let plan = modorch_fedengine_ddl::build_guarded_mutation_plan(
        &sql,
        &my,
        upd_reorder.primary.as_ref().unwrap(),
        None,
        &[PgType::Int8, PgType::Text],
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.param_map, vec![1, 0, 0]);
    assert!(
        plan.native_sql.contains("name = ?, note = ? WHERE id = ?"),
        "{}",
        plan.native_sql
    );

    // INSERT ... RETURNING allowed across all 10 guarded dialects
    for src in [&pg, &gp, &kb, &my, &mdb, &ora, &mssql, &dm, &db2, &hana] {
        let sql = format!(
            "INSERT INTO {}.\"{}\".items (id, name) VALUES ($1, $2) RETURNING id",
            src.catalog_alias, src.default_schema
        );
        let ins = a.analyze(&sql).unwrap();
        let (compact, _) = modorch_fedengine_ddl::compact_parameters(&ins.sql).unwrap();
        let mp = modorch_fedengine_ddl::build_guarded_mutation_plan(
            &compact,
            src,
            ins.primary.as_ref().unwrap(),
            ins.returning.as_ref(),
            &[PgType::Int8, PgType::Text],
        )
        .unwrap()
        .unwrap();
        match src.kind {
            DataSourceKind::Sqlserver => {
                assert!(
                    mp.native_sql.contains("OUTPUT INSERTED.[id]"),
                    "{}",
                    mp.native_sql
                )
            }
            DataSourceKind::Db2 => assert!(
                mp.native_sql
                    .starts_with("SELECT \"id\" FROM FINAL TABLE ("),
                "{}",
                mp.native_sql
            ),
            DataSourceKind::Oracle | DataSourceKind::Dameng => {
                assert!(
                    mp.native_sql.ends_with(" RETURNING \"id\" INTO ?"),
                    "{}",
                    mp.native_sql
                );
            }
            DataSourceKind::Mysql | DataSourceKind::Mariadb | DataSourceKind::Hana => {
                assert!(
                    !mp.native_sql.to_ascii_uppercase().contains("RETURNING"),
                    "{}",
                    mp.native_sql
                );
            }
            _ => assert!(mp.native_sql.contains("RETURNING id"), "{}", mp.native_sql),
        }
    }

    // Oracle/Dameng multi-row INSERT RETURNING -> 0A000
    for src in [&ora, &dm] {
        let sql = format!(
            "INSERT INTO {}.\"{}\".items (id, name) VALUES ($1, $2), ($3, $4) RETURNING id",
            src.catalog_alias, src.default_schema
        );
        let ins = a.analyze(&sql).unwrap();
        let (compact, _) = modorch_fedengine_ddl::compact_parameters(&ins.sql).unwrap();
        let err = modorch_fedengine_ddl::build_guarded_mutation_plan(
            &compact,
            src,
            ins.primary.as_ref().unwrap(),
            ins.returning.as_ref(),
            &[PgType::Int8, PgType::Text, PgType::Int8, PgType::Text],
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, "0A000");
    }

    // UPDATE ... RETURNING
    for src in [&pg, &gp, &kb, &ora, &mssql, &dm, &db2] {
        let sql = format!(
            "UPDATE {}.\"{}\".items SET name = $1 WHERE id = $2 RETURNING id",
            src.catalog_alias, src.default_schema
        );
        let upd = a.analyze(&sql).unwrap();
        let (compact, _) = modorch_fedengine_ddl::compact_parameters(&upd.sql).unwrap();
        let mp = modorch_fedengine_ddl::build_guarded_mutation_plan(
            &compact,
            src,
            upd.primary.as_ref().unwrap(),
            upd.returning.as_ref(),
            &[PgType::Text, PgType::Int8],
        )
        .unwrap()
        .unwrap();
        match src.kind {
            DataSourceKind::Sqlserver => {
                assert!(
                    mp.native_sql.contains("OUTPUT INSERTED.[id]"),
                    "{}",
                    mp.native_sql
                )
            }
            DataSourceKind::Db2 => assert!(
                mp.native_sql
                    .starts_with("SELECT \"id\" FROM FINAL TABLE ("),
                "{}",
                mp.native_sql
            ),
            DataSourceKind::Oracle | DataSourceKind::Dameng => {
                assert!(
                    mp.native_sql.ends_with(" RETURNING \"id\" INTO ?"),
                    "{}",
                    mp.native_sql
                );
            }
            _ => assert!(mp.native_sql.contains("RETURNING id"), "{}", mp.native_sql),
        }
    }
    for src in [&my, &mdb, &hana] {
        let sql = format!(
            "UPDATE {}.\"{}\".items SET name = $1 WHERE id = $2 RETURNING id",
            src.catalog_alias, src.default_schema
        );
        let upd = a.analyze(&sql).unwrap();
        let err = modorch_fedengine_ddl::build_guarded_mutation_plan(
            &upd.sql,
            src,
            upd.primary.as_ref().unwrap(),
            upd.returning.as_ref(),
            &[PgType::Text, PgType::Int8],
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, "0A000");
    }

    // DELETE ... RETURNING
    for src in [&pg, &gp, &kb, &mdb, &ora, &mssql, &dm, &db2] {
        let sql = format!(
            "DELETE FROM {}.\"{}\".items WHERE id = $1 RETURNING id",
            src.catalog_alias, src.default_schema
        );
        let del = a.analyze(&sql).unwrap();
        let mp = modorch_fedengine_ddl::build_guarded_mutation_plan(
            &del.sql,
            src,
            del.primary.as_ref().unwrap(),
            del.returning.as_ref(),
            &[PgType::Int8],
        )
        .unwrap()
        .unwrap();
        match src.kind {
            DataSourceKind::Sqlserver => {
                assert!(
                    mp.native_sql.contains("OUTPUT DELETED.[id]"),
                    "{}",
                    mp.native_sql
                )
            }
            DataSourceKind::Db2 => assert!(
                mp.native_sql.starts_with("SELECT \"id\" FROM OLD TABLE ("),
                "{}",
                mp.native_sql
            ),
            DataSourceKind::Oracle | DataSourceKind::Dameng => {
                assert!(
                    mp.native_sql.ends_with(" RETURNING \"id\" INTO ?"),
                    "{}",
                    mp.native_sql
                );
            }
            _ => assert!(mp.native_sql.contains("RETURNING id"), "{}", mp.native_sql),
        }
    }
    for src in [&my, &hana] {
        let sql = format!(
            "DELETE FROM {}.\"{}\".items WHERE id = $1 RETURNING id",
            src.catalog_alias, src.default_schema
        );
        let del = a.analyze(&sql).unwrap();
        let err = modorch_fedengine_ddl::build_guarded_mutation_plan(
            &del.sql,
            src,
            del.primary.as_ref().unwrap(),
            del.returning.as_ref(),
            &[PgType::Int8],
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, "0A000");
    }
}

#[test]
fn fqe_9_00_ddl_precision_gates_widening_and_dml_casts() {
    let pg = test_source("pg", DataSourceKind::Postgres, "pg_s", "public");
    let gp = test_source("gp", DataSourceKind::Greenplum, "gp_s", "public");
    let kb = test_source("kb", DataSourceKind::Kingbasees, "kb_s", "public");
    let my = test_source("my", DataSourceKind::Mysql, "my_s", "app");
    let mdb = test_source("mdb", DataSourceKind::Mariadb, "mdb_s", "app");
    let ora = test_source("ora", DataSourceKind::Oracle, "ora_s", "HR");
    let mssql = test_source("ms", DataSourceKind::Sqlserver, "ms_s", "dbo");
    let dm = test_source("dm", DataSourceKind::Dameng, "dm_s", "DM");
    let db2 = test_source("db2", DataSourceKind::Db2, "db2_s", "DB2INST1");
    let hana = test_source("hana", DataSourceKind::Hana, "hana_s", "APP");

    let a = Analyzer {
        namespace: None,
        sources: vec![
            pg.clone(),
            gp.clone(),
            kb.clone(),
            my.clone(),
            mdb.clone(),
            ora.clone(),
            mssql.clone(),
            dm.clone(),
            db2.clone(),
            hana.clone(),
        ],
        indexes: vec![],
    };

    // Omitted temporal precision defaults to 6 for TIME, TIMESTAMP, TIMESTAMPTZ
    let temporal = a
        .analyze("CREATE TABLE pg_s.public.t (a TIME, b TIMESTAMP, c TIMESTAMPTZ, d TIME(0))")
        .unwrap();
    let plan = temporal
        .ddl_intent
        .unwrap()
        .into_guarded_plan(&pg, "db".into(), "public".into(), String::new())
        .unwrap()
        .unwrap();
    assert_eq!(plan.columns[0].precision, 6);
    assert_eq!(plan.columns[1].precision, 6);
    assert_eq!(plan.columns[2].precision, 6);
    assert_eq!(plan.columns[3].precision, 0);

    // Invalid VARCHAR(0) and temporal precision > 6 rejected early
    assert!(
        a.analyze("CREATE TABLE pg_s.public.t (s VARCHAR(0))")
            .is_err()
    );
    assert!(
        a.analyze("CREATE TABLE pg_s.public.t (t TIMESTAMP(7))")
            .is_err()
    );

    // NULL / NOT NULL / PRIMARY KEY / DEFAULT NULL contradictions rejected
    for bad_ddl in [
        "CREATE TABLE pg_s.public.t (id BIGINT NOT NULL DEFAULT NULL)",
        "CREATE TABLE pg_s.public.t (id BIGINT PRIMARY KEY DEFAULT NULL)",
        "CREATE TABLE pg_s.public.t (id BIGINT NULL PRIMARY KEY)",
        "CREATE TABLE pg_s.public.t (id BIGINT DEFAULT NULL, PRIMARY KEY (id))",
        "ALTER TABLE pg_s.public.t ADD COLUMN id BIGINT NOT NULL DEFAULT NULL",
    ] {
        assert!(a.analyze(bad_ddl).is_err(), "expected error: {bad_ddl}");
    }

    // Greenplum foreign key rejected; DB2 column ops & Oracle/Dameng/DB2/HANA TRUNCATE RESTART rejected
    for (src, db, schema, sql) in [
        (
            &gp,
            "db",
            "public",
            "ALTER TABLE gp_s.public.orders ADD CONSTRAINT fk_p FOREIGN KEY (parent_id) REFERENCES gp_s.public.parents(id)",
        ),
        (
            &db2,
            "TESTDB",
            "DB2INST1",
            "ALTER TABLE db2_s.\"DB2INST1\".orders DROP COLUMN note",
        ),
        (
            &db2,
            "TESTDB",
            "DB2INST1",
            "ALTER TABLE db2_s.\"DB2INST1\".orders ALTER COLUMN qty SET NOT NULL",
        ),
        (
            &db2,
            "TESTDB",
            "DB2INST1",
            "ALTER TABLE db2_s.\"DB2INST1\".orders ALTER COLUMN qty DROP NOT NULL",
        ),
        (
            &ora,
            "ORCL",
            "HR",
            "CREATE TABLE ora_s.\"HR\".bad (s VARCHAR(10) DEFAULT '')",
        ),
        (
            &ora,
            "ORCL",
            "HR",
            "CREATE TABLE ora_s.\"HR\".bad (b INTEGER DEFAULT true)",
        ),
        (
            &ora,
            "ORCL",
            "HR",
            "ALTER TABLE ora_s.\"HR\".bad ALTER COLUMN s SET DEFAULT ''",
        ),
        (
            &ora,
            "ORCL",
            "HR",
            "ALTER TABLE ora_s.\"HR\".bad ALTER COLUMN b SET DEFAULT false",
        ),
        (
            &ora,
            "ORCL",
            "HR",
            "TRUNCATE TABLE ora_s.\"HR\".orders RESTART IDENTITY",
        ),
        (
            &dm,
            "DAMENG",
            "DM",
            "TRUNCATE TABLE dm_s.\"DM\".orders RESTART IDENTITY",
        ),
        (
            &db2,
            "TESTDB",
            "DB2INST1",
            "TRUNCATE TABLE db2_s.\"DB2INST1\".orders RESTART IDENTITY",
        ),
        (
            &hana,
            "HDB",
            "APP",
            "TRUNCATE TABLE hana_s.\"APP\".orders RESTART IDENTITY",
        ),
    ] {
        let analyzed = a.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let err = analyzed
            .ddl_intent
            .unwrap()
            .into_guarded_plan(src, db.into(), schema.into(), String::new())
            .unwrap_err();
        assert_eq!(err.sqlstate, "0A000", "{sql}");
    }

    // REINDEX allowed on Oracle, SQL Server, Dameng per ddl-capability-matrix.md
    let mut reindex_analyzer = a.clone();
    for (src, schema, tbl, idx) in [
        (&ora, "HR", "orders", "ora_orders_idx"),
        (&mssql, "dbo", "orders", "ms_orders_idx"),
        (&dm, "DM", "orders", "dm_orders_idx"),
    ] {
        reindex_analyzer.indexes.push(IndexOwner {
            index: ObjectRef {
                source: src.id.clone(),
                catalog: src.catalog_alias.clone(),
                schema: schema.into(),
                name: idx.into(),
            },
            table: ObjectRef {
                source: src.id.clone(),
                catalog: src.catalog_alias.clone(),
                schema: schema.into(),
                name: tbl.into(),
            },
            protected: false,
        });
    }
    for (src, db, schema, sql) in [
        (
            &ora,
            "ORCL",
            "HR",
            "REINDEX INDEX ora_s.\"HR\".ora_orders_idx",
        ),
        (
            &mssql,
            "master",
            "dbo",
            "REINDEX INDEX ms_s.dbo.ms_orders_idx",
        ),
        (
            &dm,
            "DAMENG",
            "DM",
            "REINDEX INDEX dm_s.\"DM\".dm_orders_idx",
        ),
    ] {
        let analyzed = reindex_analyzer
            .analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let plan = analyzed
            .ddl_intent
            .unwrap()
            .into_guarded_plan(src, db.into(), schema.into(), "orders".into())
            .unwrap()
            .unwrap();
        assert_eq!(plan.action, "reindex");
    }

    // Lossless ALTER COLUMN TYPE widening rules
    let col = |ty: &str, length: i32, precision: i32, scale: i32| {
        modorch_fedengine_contract::DdlColumnSpec {
            name: "c".into(),
            type_name: ty.into(),
            length,
            precision,
            scale,
            not_null: false,
            primary_key: false,
            unique: false,
            default_kind: None,
            default_value: None,
        }
    };
    assert!(is_lossless_type_widening(
        DataSourceKind::Postgres,
        &col("smallint", 0, 0, 0),
        &col("integer", 0, 0, 0)
    ));
    assert!(is_lossless_type_widening(
        DataSourceKind::Postgres,
        &col("varchar", 16, 0, 0),
        &col("text", 0, 0, 0)
    ));
    assert!(!is_lossless_type_widening(
        DataSourceKind::Mysql,
        &col("varchar", 16, 0, 0),
        &col("text", 0, 0, 0)
    ));
    assert!(!is_lossless_type_widening(
        DataSourceKind::Postgres,
        &col("integer", 0, 0, 0),
        &col("smallint", 0, 0, 0)
    ));
    assert!(is_lossless_type_widening(
        DataSourceKind::Db2,
        &col("varchar", 16, 0, 0),
        &col("varchar", 64, 0, 0)
    ));
    assert!(!is_lossless_type_widening(
        DataSourceKind::Db2,
        &col("integer", 0, 0, 0),
        &col("bigint", 0, 0, 0)
    ));
    assert!(!is_lossless_type_widening(
        DataSourceKind::Hana,
        &col("timestamp", 0, 0, 0),
        &col("timestamp", 0, 6, 0)
    ));
    assert!(is_lossless_type_widening(
        DataSourceKind::Postgres,
        &col("timestamp", 0, 0, 0),
        &col("timestamp", 0, 6, 0)
    ));
    // Target widening gate in into_guarded_plan
    assert!(
        a.analyze("ALTER TABLE db2_s.\"DB2INST1\".t ALTER COLUMN c TYPE BIGINT")
            .unwrap()
            .ddl_intent
            .unwrap()
            .into_guarded_plan(&db2, "TESTDB".into(), "DB2INST1".into(), String::new())
            .is_err()
    );
    assert!(
        a.analyze("ALTER TABLE hana_s.\"APP\".t ALTER COLUMN c TYPE TIMESTAMP(6)")
            .unwrap()
            .ddl_intent
            .unwrap()
            .into_guarded_plan(&hana, "HDB".into(), "APP".into(), String::new())
            .is_err()
    );

    // Oracle DML scalar cast translation keeping TO_DATE/TO_TIMESTAMP/TO_TIMESTAMP_TZ
    let ora_ins = a
        .analyze(
            "INSERT INTO ora_s.\"HR\".events (id, cnt, f4, f8, dt, ts, tstz, lit) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 42::INTEGER)",
        )
        .unwrap();
    let (compact_ora, _) = compact_parameters(&ora_ins.sql).unwrap();
    let ora_mp = build_guarded_mutation_plan(
        &compact_ora,
        &ora,
        ora_ins.primary.as_ref().unwrap(),
        None,
        &[
            PgType::Int8,
            PgType::Int4,
            PgType::Float4,
            PgType::Float8,
            PgType::Date,
            PgType::Timestamp,
            PgType::Timestamptz,
        ],
    )
    .unwrap()
    .unwrap();
    assert!(
        ora_mp.native_sql.contains("CAST(? AS NUMBER(19,0))"),
        "{}",
        ora_mp.native_sql
    );
    assert!(
        ora_mp.native_sql.contains("CAST(? AS NUMBER(10,0))"),
        "{}",
        ora_mp.native_sql
    );
    assert!(
        ora_mp.native_sql.contains("CAST(? AS BINARY_FLOAT)"),
        "{}",
        ora_mp.native_sql
    );
    assert!(
        ora_mp.native_sql.contains("CAST(? AS BINARY_DOUBLE)"),
        "{}",
        ora_mp.native_sql
    );
    assert!(
        ora_mp.native_sql.contains("TO_DATE(?, 'YYYY-MM-DD')"),
        "{}",
        ora_mp.native_sql
    );
    assert!(
        ora_mp
            .native_sql
            .contains("TO_TIMESTAMP(?, 'YYYY-MM-DD HH24:MI:SS.FF6')"),
        "{}",
        ora_mp.native_sql
    );
    assert!(
        ora_mp
            .native_sql
            .contains("TO_TIMESTAMP_TZ(?, 'YYYY-MM-DD HH24:MI:SS.FF6TZH:TZM')"),
        "{}",
        ora_mp.native_sql
    );
    assert!(
        ora_mp.native_sql.contains("CAST(42 AS NUMBER(10,0))"),
        "{}",
        ora_mp.native_sql
    );

    // MySQL TIME parameter wrapper
    let my_time = a
        .analyze("INSERT INTO my_s.app.events (at_t) VALUES ($1)")
        .unwrap();
    let my_mp = build_guarded_mutation_plan(
        &compact_parameters(&my_time.sql).unwrap().0,
        &my,
        my_time.primary.as_ref().unwrap(),
        None,
        &[PgType::Time],
    )
    .unwrap()
    .unwrap();
    assert!(
        my_mp
            .native_sql
            .contains("CAST(CAST(? AS CHAR) AS TIME(6))"),
        "{}",
        my_mp.native_sql
    );

    // KingbaseES pg_catalog.date casts and empty/non-ASCII text literal collection
    let kb_ins = a
        .analyze(
            "INSERT INTO kb_s.public.events (dt, note, raw_dt) VALUES ($1, '', '2026-01-01'::DATE)",
        )
        .unwrap();
    let kb_mp = build_guarded_mutation_plan(
        &compact_parameters(&kb_ins.sql).unwrap().0,
        &kb,
        kb_ins.primary.as_ref().unwrap(),
        None,
        &[PgType::Date],
    )
    .unwrap()
    .unwrap();
    assert!(
        kb_mp.native_sql.contains("CAST($1 AS pg_catalog.date)"),
        "{}",
        kb_mp.native_sql
    );
    assert!(
        kb_mp.native_sql.contains("pg_catalog.date"),
        "{}",
        kb_mp.native_sql
    );
    assert!(kb_mp.text_literals.contains(&String::new()));
    assert!(validate_kingbase_text_literals("INSERT INTO t VALUES ('ok')", true, true).is_ok());
    assert!(validate_kingbase_text_literals("INSERT INTO t VALUES ('')", false, true).is_err());
    assert!(validate_kingbase_text_literals("INSERT INTO t VALUES ('中文')", true, false).is_err());
}

#[test]
fn at_time_zone_and_timezone_fn_and_timetz_casts_rewrite_cleanly() {
    let a = analyzer();
    let res1 = a
        .analyze("SELECT '2026-06-15 00:00:00+00'::TIMESTAMPTZ AT TIME ZONE '+08:00'")
        .unwrap();
    assert!(
        res1.sql.contains("AT TIME ZONE 'Etc/GMT-8'"),
        "{}",
        res1.sql
    );

    let res2 = a
        .analyze("SELECT pg_catalog.timezone('UTC+8', '2026-06-15 00:00:00+00'::TIMESTAMPTZ)")
        .unwrap();
    assert!(res2.sql.contains("timezone('Etc/GMT+8'"), "{}", res2.sql);
    assert!(!res2.sql.contains("pg_catalog.timezone"), "{}", res2.sql);

    let res3 = a
        .analyze("SELECT '2026-06-15 00:00:00+00'::TIMESTAMPTZ AT TIME ZONE INTERVAL '+08:00'")
        .unwrap();
    assert!(
        res3.sql.contains("AT TIME ZONE 'Etc/GMT-8'"),
        "{}",
        res3.sql
    );

    let res4 = a
        .analyze("SELECT '12:34:56+08:00'::TIMETZ, '12:34:56+08:00'::pg_catalog.timetz")
        .unwrap();
    assert!(res4.sql.contains("TIMETZ"), "{}", res4.sql);
    assert!(!res4.sql.contains("pg_catalog.timetz"), "{}", res4.sql);

    assert_eq!(
        a.analyze("SELECT '2026-06-15 00:00:00+00'::TIMESTAMPTZ AT TIME ZONE '+05:30'")
            .unwrap_err()
            .sqlstate,
        "0A000"
    );
    assert_eq!(
        a.analyze("SELECT timezone(INTERVAL '1 day', '2026-06-15 00:00:00+00'::TIMESTAMPTZ)")
            .unwrap_err()
            .sqlstate,
        "22023"
    );
}
