//! Development test-host contracts using the same public Bundle as service hosts.
mod common;
use common::TestDirectory;
use modorch::{
    ApplicationBuilder, ApplicationState, FrameworkError, Phase, ShutdownReceipt, StartupError,
};
use modorch_fedengine::{FedEngineBundle, FedEngineConfig};
use modorch_fedengine_test_host::FedEngine;
use std::{collections::BTreeSet, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const DEADLINE: Duration = Duration::from_secs(5);

fn configuration(directory: &TestDirectory) -> FedEngineConfig {
    let mut config = FedEngineConfig::load("config/application.toml").unwrap();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.management.listen = "127.0.0.1:0".into();
    config.users[0].password_secret_ref = "USER".into();
    assert!(std::env::var("USER").is_ok());
    config.duckdb.database_path = directory
        .path()
        .join("engine.duckdb")
        .to_string_lossy()
        .into();
    config.duckdb.temp_directory = directory.path().join("tmp").to_string_lossy().into();
    config.management.credentials_directory =
        directory.path().join("secrets").to_string_lossy().into();
    config.duckdb.max_workers = 1;
    config.duckdb.threads = 1;
    config.shutdown.grace_period_secs = 1;
    config
}

#[tokio::test]
async fn default_profiles_use_bundle_components_and_hide_addresses_after_shutdown() {
    for management in [false, true] {
        let directory = TestDirectory::new();
        let mut config = configuration(&directory);
        config.management.enabled = management;
        let mut builder = ApplicationBuilder::new("expected-profile");
        let registered = FedEngineBundle::new(config.clone())
            .register(&mut builder)
            .unwrap();
        let expected: BTreeSet<_> = registered.components().cloned().collect();
        drop(builder);

        let engine = FedEngine::start(config).await.unwrap();
        let health = engine.handle.health();
        assert!(health.ready);
        assert_eq!(
            health.components.keys().cloned().collect::<BTreeSet<_>>(),
            expected
        );
        let pg = engine
            .address()
            .expect("associated Bundle exposes ready PGWire");
        let http = engine.management_address();
        assert_eq!(http.is_some(), management);
        let mut stream = TcpStream::connect(pg).await.unwrap();
        stream
            .write_all(&[0, 0, 0, 8, 4, 210, 22, 47])
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(DEADLINE, stream.read_u8())
                .await
                .unwrap()
                .unwrap(),
            b'N'
        );
        drop(stream);
        if let Some(http) = http {
            let mut stream = TcpStream::connect(http).await.unwrap();
            stream
                .write_all(
                    b"GET /api/v1/sources HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let mut response = String::new();
            tokio::time::timeout(DEADLINE, stream.read_to_string(&mut response))
                .await
                .unwrap()
                .unwrap();
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        }
        assert!(matches!(
            engine.shutdown_until_drained().await.unwrap(),
            ShutdownReceipt::GracefullyStopped
        ));
        assert_eq!(engine.handle.health().state, ApplicationState::Stopped);
        assert!(engine.address().is_none());
        assert!(engine.management_address().is_none());
        assert_eq!(engine.handle.task_metrics().tasks, 0);
        assert_eq!(engine.metrics().active_workers, 0);
        TcpListener::bind(pg).await.expect("PGWire released");
        if let Some(http) = http {
            TcpListener::bind(http).await.expect("HTTP released");
        }
    }
}

#[tokio::test]
async fn startup_errors_preserve_framework_cause_cleanup_and_handle_then_allow_restart() {
    for management in [false, true] {
        let directory = TestDirectory::new();
        let mut config = configuration(&directory);
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        if management {
            config.management.listen = address.to_string();
        } else {
            config.pgwire.listen = address.to_string();
        }
        let error = FedEngine::start(config.clone())
            .await
            .err()
            .expect("occupied port");
        let error = error
            .downcast::<StartupError>()
            .expect("original StartupError retained");
        assert_eq!(error.source.phase(), Some(Phase::Setup));
        assert!(matches!(
            error.cleanup,
            Ok(ShutdownReceipt::GracefullyStopped)
        ));
        assert_eq!(error.handle.health().state, ApplicationState::Stopped);
        assert_eq!(error.handle.task_metrics().tasks, 0);
        assert!(
            error
                .handle
                .health()
                .components
                .values()
                .any(|component| component.last_failure.is_some())
        );
        drop(error);
        drop(occupied);
        // Reopen the same management state after compensation, without deleting it.
        let engine = FedEngine::start(config).await.unwrap();
        engine.shutdown_until_drained().await.unwrap();
        TcpListener::bind(address)
            .await
            .expect("restarted ingress released");
    }
}

#[tokio::test]
async fn slow_http_request_keeps_adapter_alive_until_retry_really_drains() {
    let directory = TestDirectory::new();
    let engine = FedEngine::start(configuration(&directory)).await.unwrap();
    let http = engine.management_address().unwrap();
    let pg = engine.address().unwrap();
    let mut stream = TcpStream::connect(http).await.unwrap();
    stream.write_all(b"POST /api/v1/sources HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 1000\r\n\r\n").await.unwrap();
    let mut interim = [0; 25];
    tokio::time::timeout(DEADLINE, stream.read_exact(&mut interim))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    assert!(matches!(
        engine.shutdown().await,
        Err(FrameworkError::StopIncomplete { .. })
    ));
    assert_eq!(
        engine.handle.health().state,
        ApplicationState::StopIncomplete
    );
    assert!(engine.address().is_none());
    assert!(engine.management_address().is_none());
    assert!(engine.handle.task_metrics().tasks > 0);

    let draining = engine.shutdown_until_drained();
    tokio::pin!(draining);
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), &mut draining)
            .await
            .is_err()
    );
    assert!(engine.handle.task_metrics().tasks > 0);
    drop(stream);
    assert!(matches!(
        tokio::time::timeout(DEADLINE, &mut draining)
            .await
            .unwrap()
            .unwrap(),
        ShutdownReceipt::GracefullyStopped
    ));
    assert_eq!(engine.handle.task_metrics().tasks, 0);
    TcpListener::bind(pg).await.expect("PGWire released");
    TcpListener::bind(http).await.expect("HTTP released");
}
