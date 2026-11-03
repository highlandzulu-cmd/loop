//! Method wire-format tests: the JSON here is the contract with clients of
//! `loop-wire` (and the SDK generated from the schema).

use loop_protocol::rpc::{
    error_codes, negotiate, InitializeParams, InitializeResult, PeerInfo, PromptParams,
    PromptResult, RpcErrorBody,
};
use loop_protocol::{
    BackendError, Capabilities, ContentBlock, ErrorCategory, ErrorInfo, SessionRef, SessionSpec,
    StopReason, TurnOutcome, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

fn session() -> SessionRef {
    SessionRef {
        instance: "native".into(),
        key: "s1".into(),
        native_id: "n1".into(),
    }
}

fn roundtrip<T>(value: &T, expected: Value)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_value(value).unwrap(), expected);
    assert_eq!(&serde_json::from_value::<T>(expected).unwrap(), value);
}

#[test]
fn initialize() {
    roundtrip(
        &InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: PeerInfo {
                name: "loop".into(),
                version: "0.5.0".into(),
            },
            client_capabilities: Default::default(),
        },
        json!({
            "protocol_version": 1,
            "client_info": {"name": "loop", "version": "0.5.0"},
            "client_capabilities": {"interact": false}
        }),
    );
    let result: InitializeResult = serde_json::from_value(json!({
        "protocol_version": 1,
        "server_info": {"name": "loop", "version": "0.5.0"},
        "instance": "native",
        "capabilities": {"usage": true, "a_future_capability": true}
    }))
    .unwrap();
    assert!(result.capabilities.usage);
    assert_eq!(
        result.capabilities,
        Capabilities {
            usage: true,
            ..Default::default()
        }
    );
}

#[test]
fn sessions_and_prompts() {
    roundtrip(
        &session(),
        json!({"instance": "native", "key": "s1", "native_id": "n1"}),
    );
    roundtrip(&SessionSpec::default(), json!({}));
    roundtrip(
        &SessionSpec {
            cwd: Some("/work".into()),
            name: Some("x".into()),
        },
        json!({"cwd": "/work", "name": "x"}),
    );
    roundtrip(
        &PromptParams {
            session: session(),
            content: vec![ContentBlock::text("hi")],
        },
        json!({
            "session": {"instance": "native", "key": "s1", "native_id": "n1"},
            "content": [{"type": "text", "text": "hi"}]
        }),
    );
}

#[test]
fn every_turn_outcome_survives_the_wire() {
    let failed = ErrorInfo::new(ErrorCategory::RateLimit, "slow down");
    let cases = [
        (
            TurnOutcome::Completed(StopReason::EndTurn),
            json!({"stop": "end_turn"}),
        ),
        (
            TurnOutcome::Completed(StopReason::MaxTokens),
            json!({"stop": "max_tokens"}),
        ),
        (TurnOutcome::Cancelled, json!({"stop": "cancelled"})),
        (
            TurnOutcome::Failed(failed.clone()),
            json!({"stop": "error", "error": {"category": "rate_limit", "retryable": true, "message": "slow down"}}),
        ),
        (
            TurnOutcome::Uncertain,
            json!({"stop": "error", "uncertain": true}),
        ),
    ];
    for (outcome, expected) in cases {
        let result = PromptResult::from(outcome.clone());
        assert_eq!(serde_json::to_value(&result).unwrap(), expected);
        let back: PromptResult = serde_json::from_value(expected).unwrap();
        assert_eq!(TurnOutcome::from(back), outcome);
    }
}

#[test]
fn every_backend_error_survives_the_wire() {
    let errors = [
        BackendError::Busy,
        BackendError::Unsupported("images".into()),
        BackendError::SessionNotFound("s9".into()),
        BackendError::failed(ErrorCategory::Auth, "sign in again"),
        BackendError::Conflict { revision: 7 },
    ];
    for error in errors {
        let body = RpcErrorBody::from(&error);
        let json = serde_json::to_value(&body).unwrap();
        let back: RpcErrorBody = serde_json::from_value(json).unwrap();
        assert_eq!(back.into_backend_error(), error);
    }
    assert_eq!(
        RpcErrorBody::from(&BackendError::Busy).code,
        error_codes::BUSY
    );
    let unknown = RpcErrorBody {
        code: -32601,
        message: "Method not found".into(),
        data: None,
    };
    assert!(matches!(
        unknown.into_backend_error(),
        BackendError::Failed(ErrorInfo {
            category: ErrorCategory::Protocol,
            ..
        })
    ));
}

#[test]
fn negotiation_picks_a_version_both_speak() {
    assert_eq!(negotiate(PROTOCOL_VERSION), PROTOCOL_VERSION);
    assert_eq!(
        negotiate(PROTOCOL_VERSION + 5),
        PROTOCOL_VERSION,
        "a newer client"
    );
    assert_eq!(
        negotiate(0),
        PROTOCOL_VERSION,
        "a version that never existed"
    );
}

#[test]
fn session_list_info_wire() {
    use loop_protocol::rpc::SessionInfo;
    roundtrip(
        &vec![SessionInfo {
            summary: Default::default(),
            r#ref: session(),
            cwd: "/work/repo".into(),
            name: Some("Review".into()),
            parent: Some(session()),
            live: true,
            busy: false,
            created_ms: 1793400000000,
            last_active_ms: 1793400001000,
        }],
        json!([{"ref": {"instance":"native","key":"s1","native_id":"n1"},
            "cwd":"/work/repo", "name":"Review", "parent":{"instance":"native","key":"s1","native_id":"n1"},
            "live":true,"busy":false,"created_ms":1793400000000u64,"last_active_ms":1793400001000u64}]),
    );
    let minimal: SessionInfo = serde_json::from_value(json!({
        "ref":session(),"cwd":"/work","live":false,"busy":false,
        "created_ms":0,"last_active_ms":0,"future":true
    }))
    .unwrap();
    assert_eq!(minimal.name, None);
    assert_eq!(minimal.parent, None);
    assert_eq!(
        serde_json::to_value(minimal).unwrap()["parent"],
        Value::Null
    );
}

#[test]
fn session_control_wire_and_defaults() {
    use loop_protocol::rpc::{AttachParams, AttachResult, CloseParams, SessionParams};
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    roundtrip(
        &CloseParams {
            session: session(),
            force: true,
        },
        json!({"session":reference,"force":true}),
    );
    let close: CloseParams =
        serde_json::from_value(json!({"session":reference,"future":true})).unwrap();
    assert!(!close.force);
    roundtrip(&close, json!({"session":reference}));
    roundtrip(
        &AttachParams {
            session: session(),
            after_seq: 40,
        },
        json!({"session":reference,"after_seq":40}),
    );
    let attach: AttachParams =
        serde_json::from_value(json!({"session":reference,"future":true})).unwrap();
    assert_eq!(attach.after_seq, 0);
    roundtrip(&attach, json!({"session":reference}));
    roundtrip(
        &AttachResult {
            latest_seq: 42,
            generation: 3,
            snapshot: None,
        },
        json!({"latest_seq":42,"generation":3}),
    );
    roundtrip(
        &SessionParams { session: session() },
        json!({"session":reference}),
    );
}

#[test]
fn runtime_status_wire() {
    use loop_protocol::rpc::RuntimeStatus;
    roundtrip(
        &RuntimeStatus {
            pid: 42,
            socket: "/a/run/runtime.sock".into(),
            generation: 3,
            loaded: 2,
            busy: 1,
            max_sessions: 16,
            max_active_turns: 8,
            max_sessions_from: None,
            max_active_turns_from: None,
        },
        json!({"pid":42,"socket":"/a/run/runtime.sock","generation":3,"loaded":2,"busy":1,
            "max_sessions":16,"max_active_turns":8}),
    );
    roundtrip(
        &RuntimeStatus {
            pid: 42,
            socket: "/s".into(),
            generation: 3,
            loaded: 0,
            busy: 0,
            max_sessions: 3,
            max_active_turns: 8,
            max_sessions_from: Some("settings".into()),
            max_active_turns_from: Some("default".into()),
        },
        json!({"pid":42,"socket":"/s","generation":3,"loaded":0,"busy":0,
            "max_sessions":3,"max_active_turns":8,
            "max_sessions_from":"settings","max_active_turns_from":"default"}),
    );
}

#[test]
fn runtime_capabilities_are_opt_in() {
    let old: Capabilities = serde_json::from_value(json!({"usage":true,"future":true})).unwrap();
    assert!(!old.multi_session);
    assert!(!old.attach);
    let capabilities = Capabilities {
        multi_session: true,
        attach: true,
        ..Default::default()
    };
    roundtrip(
        &capabilities,
        json!({"load_session":false,"resume_session":false,"set_model":false,
        "modes":false,"usage":false,"images":false,"steering":false,"fs_client":false,
        "terminal_client":false,"multi_session":true,"attach":true,"efforts":false,
        "attach_snapshot":false}),
    );
}

#[test]
fn interactions_route_to_a_session_and_withdraw_by_native_id() {
    use loop_protocol::rpc::{InteractParams, InteractWithdrawnParams};
    use loop_protocol::InteractionRequest;
    let cases = [
        json!({"kind":"permission","id":"p1","options":[]}),
        json!({"kind":"question","id":"p1","prompt":"Which?"}),
        json!({"kind":"form","id":"p1","schema":{"type":"object"}}),
        json!({"kind":"url","id":"p1","url":"https://example.com","purpose":"Sign in"}),
        json!({"kind":"unsupported","id":"p1","method":"future","raw":{}}),
    ];
    for legacy in cases {
        let request: InteractionRequest = serde_json::from_value(legacy.clone()).unwrap();
        let old: InteractParams = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(old.session, None);
        roundtrip(&old, legacy.clone());
        let mut routed = legacy;
        routed["session"] = serde_json::to_value(session()).unwrap();
        roundtrip(
            &InteractParams {
                session: Some(session()),
                request: request.clone(),
            },
            routed.clone(),
        );
        routed["future"] = json!(true);
        let extended: InteractParams = serde_json::from_value(routed.clone()).unwrap();
        assert_eq!(extended.request, request);
        // Old v1 readers still understand the interaction and ignore routing.
        assert_eq!(
            serde_json::from_value::<InteractionRequest>(routed).unwrap(),
            request
        );
    }
    roundtrip(
        &InteractWithdrawnParams {
            session: session(),
            id: "p1".into(),
        },
        json!({"session":{"instance":"native","key":"s1","native_id":"n1"},"id":"p1"}),
    );
}

#[test]
fn session_tree_v1_capabilities_and_event_goldens() {
    use loop_protocol::rpc::*;
    use loop_protocol::SessionUpdate;
    let legacy: ClientCapabilities = serde_json::from_value(json!({"interact":true})).unwrap();
    assert!(!legacy.branch_reset);
    let capabilities: Capabilities = serde_json::from_value(json!({})).unwrap();
    assert!(
        !capabilities.session_tree
            && !capabilities.branch_snapshot
            && !capabilities.session_transfer
    );
    roundtrip(
        &NavigateParams {
            session: session(),
            target_entry_id: None,
            summarize: false,
            expected_revision: Some(2),
        },
        json!({"session":session(),"target_entry_id":null,"summarize":false,"expected_revision":2}),
    );
    roundtrip(
        &SessionUpdate::BranchReset {
            branch_revision: 3,
            view_id: "view-1".into(),
            reason: "navigate".into(),
        },
        json!({"type":"branch_reset","branch_revision":3,"view_id":"view-1","reason":"navigate"}),
    );
}

#[test]
fn revision_conflicts_preserve_the_current_revision_over_the_wire() {
    let error = loop_protocol::BackendError::Conflict { revision: 7 };
    let body = loop_protocol::rpc::RpcErrorBody::from(&error);
    assert_eq!(body.code, loop_protocol::rpc::error_codes::CONFLICT);
    assert_eq!(body.data, Some(json!({"revision":7})));
    assert_eq!(body.into_backend_error(), error);
}

#[test]
fn model_control_methods_wire() {
    use loop_protocol::rpc::{
        methods, ChoiceInfo, ChoicesResult, ModelInfo, ModelsResult, SetEffortParams,
        SetModeParams, SetModelParams,
    };
    use loop_protocol::{ModelChoice, ModelChoices, MutationResult};
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    assert_eq!(
        [
            methods::SESSION_MODELS,
            methods::SESSION_SET_MODEL,
            methods::SESSION_EFFORTS,
            methods::SESSION_SET_EFFORT,
            methods::SESSION_MODES,
            methods::SESSION_SET_MODE,
        ],
        [
            "session/models",
            "session/set_model",
            "session/efforts",
            "session/set_effort",
            "session/modes",
            "session/set_mode",
        ]
    );
    let models = ModelsResult {
        current: Some("faux/one".into()),
        models: vec![ModelInfo {
            id: "faux/one".into(),
            name: "One".into(),
            description: None,
            backend: "loop".into(),
        }],
        revision: 4,
    };
    roundtrip(
        &models,
        json!({"current":"faux/one","models":[{"id":"faux/one","name":"One","backend":"loop"}],
            "revision":4}),
    );
    let choices = ModelChoices::from(models.clone());
    assert_eq!(
        choices,
        ModelChoices {
            current: Some("faux/one".into()),
            models: vec![ModelChoice {
                id: "faux/one".into(),
                name: "One".into(),
                description: None,
            }],
            revision: 4,
        }
    );
    assert_eq!(ModelsResult::from_choices(choices, &"loop".into()), models);
    let efforts = ChoicesResult {
        current: None,
        choices: vec![ChoiceInfo {
            id: "off".into(),
            name: "off".into(),
        }],
        revision: 0,
    };
    roundtrip(
        &efforts,
        json!({"choices":[{"id":"off","name":"off"}],"revision":0}),
    );
    assert_eq!(
        ChoicesResult::from(ModelChoices::from(efforts.clone())),
        efforts
    );
    roundtrip(
        &SetModelParams {
            session: session(),
            model: "faux/two".into(),
            expected_revision: Some(4),
        },
        json!({"session":reference,"model":"faux/two","expected_revision":4}),
    );
    roundtrip(
        &SetEffortParams {
            session: session(),
            effort: "off".into(),
            expected_revision: None,
        },
        json!({"session":reference,"effort":"off"}),
    );
    roundtrip(
        &SetModeParams {
            session: session(),
            mode: "plan".into(),
            expected_revision: None,
        },
        json!({"session":reference,"mode":"plan"}),
    );
    roundtrip(
        &MutationResult {
            revision: 5,
            seq: 12,
            pending: true,
            applies_after_turn: Some("turn-3".into()),
        },
        json!({"revision":5,"seq":12,"pending":true,"applies_after_turn":"turn-3"}),
    );
    let old: MutationResult = serde_json::from_value(json!({"revision":1,"seq":2})).unwrap();
    assert!(!old.pending);
}

#[test]
fn a_conflict_carries_the_current_revision() {
    let body = RpcErrorBody::from(&BackendError::Conflict { revision: 9 });
    assert_eq!(body.code, error_codes::CONFLICT);
    assert_eq!(body.data, Some(json!({"revision": 9})));
}

#[test]
fn config_changed_and_attach_snapshot_wire() {
    use loop_protocol::rpc::{AttachResult, SessionSnapshot};
    use loop_protocol::{ConfigChanged, SessionConfig, SessionMetadata, SessionUpdate};
    let config = SessionConfig {
        model: Some("faux/one".into()),
        effort: Some("off".into()),
        mode: None,
    };
    let update = SessionUpdate::ConfigChanged(ConfigChanged {
        revision: 3,
        metadata: SessionMetadata {
            name: Some("Review".into()),
        },
        config: config.clone(),
        effective: SessionConfig {
            effort: Some("high".into()),
            ..config.clone()
        },
        pending: true,
        applies_after_turn: Some("turn-1".into()),
        workspace_revision: 0,
    });
    roundtrip(
        &update,
        json!({"type":"config_changed","revision":3,"metadata":{"name":"Review"},
            "config":{"model":"faux/one","effort":"off"},
            "effective":{"model":"faux/one","effort":"high"},
            "pending":true,"applies_after_turn":"turn-1","workspace_revision":0}),
    );
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    let attached = AttachResult {
        latest_seq: 8,
        generation: 2,
        snapshot: Some(SessionSnapshot {
            session: session(),
            at_seq: 8,
            revision: 3,
            metadata: SessionMetadata::default(),
            config: config.clone(),
            effective: config,
            pending: false,
            capabilities: Capabilities::default(),
            workspace_id: String::new(),
            branch_revision: 0,
            view_id: None,
            turn_state: None,
            stats: serde_json::Value::Null,
            workspace_revision: 0,
        }),
    };
    let value = serde_json::to_value(&attached).unwrap();
    assert_eq!(value["snapshot"]["session"], reference);
    assert_eq!(value["snapshot"]["at_seq"], 8);
    assert_eq!(value["snapshot"]["config"]["effort"], "off");
    assert_eq!(
        serde_json::from_value::<AttachResult>(value).unwrap(),
        attached
    );
    // An old server's result has no snapshot.
    let old: AttachResult = serde_json::from_value(json!({"latest_seq":8,"generation":2})).unwrap();
    assert_eq!(old.snapshot, None);
}
