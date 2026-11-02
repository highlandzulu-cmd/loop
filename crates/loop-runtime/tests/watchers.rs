//! A Session watched by any connection stays loaded at the Session cap, and
//! an attach that races unloading is never acknowledged for a Session it
//! cannot keep loaded. Real wire, in memory.

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::{PeerInfo, RpcErrorBody};
use loop_protocol::{BackendError, SessionKey, SessionRef};
use loop_runtime::log::{EventLog, MemoryEventLog};
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_control, ServeOptions, Served};
use loop_wire::{transport::memory, Connection, Incoming, RpcError};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(60);

fn opts(cwd: &std::path::Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: None,
    }
}

/// A Runtime that holds one Session at a time.
async fn runtime(dir: &std::path::Path, log: Arc<dyn EventLog>) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    Runtime::new(
        globals,
        opts(dir),
        log,
        RuntimeLimits {
            max_sessions: 1,
            max_active_turns: 1,
        },
    )
    .await
    .unwrap()
    .into_shared()
}

/// A raw client; `interact` is what it tells `initialize`.
struct Client {
    conn: Arc<Connection>,
    _served: Served,
}
impl Client {
    async fn connect(rt: &Arc<Runtime>, interact: bool) -> Self {
        let (server, client) = memory();
        let served = serve_with_control(
            rt.clone(),
            rt.clone(),
            server,
            PeerInfo {
                name: "test".into(),
                version: "1".into(),
            },
            ServeOptions {
                owns_backend: false,
            },
        );
        let (conn, mut incoming) = Connection::new(client);
        let reader = conn.clone();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Response { id, result } = message {
                    reader.resolve(id, result).await;
                }
            }
        });
        conn.request(
            "initialize",
            json!({"protocol_version": 1, "client_info": {"name": "t", "version": "1"},
                "client_capabilities": {"interact": interact}}),
        )
        .await
        .unwrap();
        Self {
            conn,
            _served: served,
        }
    }
    async fn call(&self, method: &str, params: Value) -> Result<Value, BackendError> {
        self.conn
            .request(method, params)
            .await
            .map_err(|e| match e {
                RpcError::Remote {
                    code,
                    message,
                    data,
                } => RpcErrorBody {
                    code,
                    message,
                    data,
                }
                .into_backend_error(),
                other => panic!("{method}: {other:?}"),
            })
    }
    async fn new_session(&self) -> Result<SessionRef, BackendError> {
        self.call("session/new", json!({}))
            .await
            .map(|v| serde_json::from_value(v).unwrap())
    }
    async fn attach(&self, session: &SessionRef) -> Result<Value, BackendError> {
        self.call("session/attach", json!({ "session": session }))
            .await
    }
    async fn detach(&self, session: &SessionRef) {
        self.call("session/detach", json!({ "session": session }))
            .await
            .unwrap();
    }
}

async fn loaded(rt: &Runtime) -> Vec<SessionKey> {
    rt.list()
        .await
        .unwrap()
        .into_iter()
        .filter(|s| s.live)
        .map(|s| s.r#ref.key)
        .collect()
}

/// Registration follows the reply on the connection's output task.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_keeps_its_session_loaded() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
        let observer = Client::connect(&rt, false).await;
        let a = observer.new_session().await.unwrap();
        observer.attach(&a).await.unwrap();
        settle().await;
        let other = Client::connect(&rt, true).await;
        assert_eq!(other.new_session().await, Err(BackendError::Busy));
        assert_eq!(loaded(&rt).await, std::slice::from_ref(&a.key));
        // Once it detaches, A can make room.
        observer.detach(&a).await;
        settle().await;
        let b = other.new_session().await.unwrap();
        assert_eq!(loaded(&rt).await, [b.key]);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disconnected_observer_no_longer_keeps_its_session() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
        let observer = Client::connect(&rt, false).await;
        let a = observer.new_session().await.unwrap();
        settle().await;
        observer.conn.close();
        settle().await;
        let other = Client::connect(&rt, true).await;
        let b = other.new_session().await.unwrap();
        assert_eq!(loaded(&rt).await, [b.key]);
        rt.shutdown().await;
        let _ = a;
    })
    .await
    .unwrap();
}
