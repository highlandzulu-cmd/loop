//! Detached default-owner startup, using the built CLI with isolated state.
use loop_cli::{
    runtime_launch::{self, Target},
    surface_client::RemoteHost,
};
use loop_protocol::{AgentBackend, SessionSpec};
use std::{path::Path, sync::Arc, time::Duration};

#[tokio::test]
async fn auto_start_and_second_client_reuse_one_owner() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let dir = tempfile::Builder::new()
            .prefix("s36")
            .tempdir_in("/tmp")
            .unwrap();
        let agent = dir.path().join("agent");
        let socket = loop_cli::config::paths::runtime_socket_path(&agent).unwrap();
        let one = runtime_launch::start_and_connect(
            Path::new(env!("CARGO_BIN_EXE_loop")),
            &agent,
            dir.path(),
            &socket,
            Arc::new(RemoteHost::default()),
        )
        .await
        .unwrap();
        let status = one.status().await.unwrap();
        let result = async {
            let discovered = runtime_launch::discover(&agent).await.unwrap();
            assert_eq!(discovered, vec![Target::Unix(socket.clone())]);
            let two =
                runtime_launch::connect(&discovered[0], None, Arc::new(RemoteHost::default()))
                    .await
                    .unwrap();
            assert_eq!(two.status().await.unwrap().pid, status.pid);
            let second_agent = dir.path().join("other-agent");
            let second_socket =
                loop_cli::config::paths::runtime_socket_path(&second_agent).unwrap();
            let other = runtime_launch::start_and_connect(
                Path::new(env!("CARGO_BIN_EXE_loop")),
                &second_agent,
                dir.path(),
                &second_socket,
                Arc::new(RemoteHost::default()),
            )
            .await
            .unwrap();
            let mut inventory = discovered.clone();
            inventory.extend(runtime_launch::discover(&second_agent).await.unwrap());
            let chosen = runtime_launch::select(&inventory, |_| Ok(1))
                .unwrap()
                .unwrap();
            let picked = runtime_launch::connect(&chosen, None, Arc::new(RemoteHost::default()))
                .await
                .unwrap();
            assert_eq!(
                picked.status().await.unwrap().pid,
                other.status().await.unwrap().pid
            );
            assert_ne!(picked.status().await.unwrap().pid, status.pid);
            other.stop_empty().await.unwrap();
            let session = one
                .new_session(SessionSpec {
                    cwd: Some(dir.path().canonicalize().unwrap()),
                    env: Some(Default::default()),
                    ..Default::default()
                })
                .await
                .unwrap();
            two.attach(&session, 0).await.unwrap();
            two.detach(&session).await.unwrap();
            assert_eq!(one.status().await.unwrap().loaded, 1);
        }
        .await;
        one.stop().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while socket.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        result
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn start_failure_is_returned_for_the_callers_in_process_fallback() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let dir = tempfile::tempdir().unwrap();
        let error = runtime_launch::start_and_connect(
            Path::new("/does/not/exist/loop"),
            dir.path(),
            dir.path(),
            &dir.path().join("runtime.sock"),
            Arc::new(RemoteHost::default()),
        )
        .await;
        assert!(error.is_err());
    })
    .await
    .unwrap();
}
