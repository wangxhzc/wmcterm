use super::*;
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, mpsc};

fn config() -> BridgeConfig {
    BridgeConfig {
        endpoint: "ws://127.0.0.1:1".into(),
        control_socket: std::env::temp_dir().join(format!(
            "wmcterm-codex-release-{}-{}.sock",
            std::process::id(),
            next_rpc_id()
        )),
        workspace_id: "workspace".into(),
        root_surface_id: "surface".into(),
        codex_program: "codex".into(),
        workspace_cwd: std::env::temp_dir(),
        max_auto_depth: 0,
    }
}

fn approval(thread: &str) -> Value {
    json!({"id": 91, "method": "item/commandExecution/requestApproval",
        "params": {"threadId": thread, "turnId": "turn", "itemId": "command",
            "availableDecisions": ["accept", "decline"]}})
}

fn input(thread: &str) -> Value {
    json!({"id": "q", "method": "item/tool/requestUserInput",
        "params": {"threadId": thread, "turnId": "turn", "itemId": "question",
            "questions": [{"id": "target", "question": "Target?"}]}})
}

fn control_id(request: &Request) -> &str {
    request
        .params
        .get("approval_id")
        .unwrap_or(&request.params["request_id"])
        .as_str()
        .unwrap_or_default()
}

fn read(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        match socket.read().unwrap() {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(bytes) => socket.send(Message::Pong(bytes)).unwrap(),
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

fn pair() -> (Socket, WebSocket<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let peer = std::thread::spawn(move || {
        let stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        tungstenite::accept(stream).unwrap()
    });
    let (socket, _) = connect(endpoint).unwrap();
    (socket, peer.join().unwrap())
}

#[test]
fn same_rpc_ids_in_different_threads_reconcile_without_crossing_replies() {
    let config = config();
    let calls = Arc::new(Mutex::new(Vec::<Request>::new()));
    let observed = calls.clone();
    let _control = wmcterm_ipc::UnixServer::spawn(&config.control_socket, move |request| {
        let result = match request.method.as_str() {
            "agent.approval.register" | "agent.input.register" => json!({
                "status": "pending",
                "approval_id": format!("{}:{}", request.params["request"]["kind"].as_str().unwrap(), request.params["request"]["thread_id"].as_str().unwrap())
            }),
            "agent.approval.poll" => json!({"status": "resolved", "decision": "once"}),
            "agent.input.poll" => json!({"status": "resolved", "resolution": {"text": r#"{"target":"original thread"}"#}}),
            "agent.request.cancel" => json!({"ok": true}),
            method => panic!("unexpected method {method}"),
        };
        let response = wmcterm_ipc::Response::success(request.id.clone(), result);
        observed.lock().unwrap().push(request);
        response
    }).unwrap();
    let mut watcher = CodexWatcher::new(config.clone());
    let (mut socket, mut peer) = pair();
    for thread in ["root", "child"] {
        watcher
            .handle_server_value(&mut socket, &approval(thread))
            .unwrap();
        watcher
            .handle_server_value(&mut socket, &input(thread))
            .unwrap();
    }
    // A native TUI reply for the child cannot consume the root's same RPC id.
    for id in [json!(91), json!("q")] {
        let resolved = json!({"method": "serverRequest/resolved", "params": {"threadId": "child", "requestId": id}});
        watcher.handle_server_value(&mut socket, &resolved).unwrap();
        watcher.handle_server_value(&mut socket, &resolved).unwrap();
    }
    watcher.inputs.complete_turn(&config, "child", "turn");
    watcher.poll_approvals(&mut socket).unwrap();
    watcher.inputs.poll(&config, &mut socket).unwrap();
    assert_eq!(
        read(&mut peer),
        json!({"id": 91, "result": {"decision": "accept"}})
    );
    assert_eq!(
        read(&mut peer),
        json!({"id": "q", "result": {"answers": {"target": {"answers": ["original thread"]}}}})
    );
    // Neither a second poll nor cleanup may reply or cancel a consumed request.
    watcher.poll_approvals(&mut socket).unwrap();
    watcher.inputs.poll(&config, &mut socket).unwrap();
    watcher.inputs.cancel_all(&config);
    let calls = calls.lock().unwrap();
    let cancellations: Vec<_> = calls
        .iter()
        .filter(|request| request.method == "agent.request.cancel")
        .map(control_id)
        .collect();
    assert_eq!(cancellations, ["command_execution:child", "input:child"]);
    let polls: Vec<_> = calls
        .iter()
        .filter(|request| request.method.ends_with(".poll"))
        .map(control_id)
        .collect();
    assert_eq!(polls, ["command_execution:root", "input:root"]);
}

#[test]
fn disconnected_session_cancels_both_request_types_before_same_ids_are_reused() {
    let config = config();
    let calls = Arc::new(Mutex::new(Vec::<Request>::new()));
    let observed = calls.clone();
    let (registered, mut registrations) = mpsc::channel();
    let _control = wmcterm_ipc::UnixServer::spawn(&config.control_socket, move |request| {
        let result = match request.method.as_str() {
            "agent.approval.register" | "agent.input.register" => {
                let id = format!("{}:{}", request.params["request"]["kind"].as_str().unwrap(), request.params["request"]["thread_id"].as_str().unwrap());
                registered.send(id.clone()).unwrap();
                json!({"status": "pending", "approval_id": id})
            }
            "agent.approval.poll" | "agent.input.poll" => {
                let id = request.params.get("approval_id").unwrap_or(&request.params["request_id"]);
                if id.as_str().unwrap().ends_with(":old") {
                    json!({"status": "pending", "approval_id": id})
                } else {
                    json!({"status": "resolved", "decision": "once", "resolution": {"text": r#"{"target":"new connection"}"#}})
                }
            }
            "agent.request.cancel" => json!({"ok": true}),
            method => panic!("unexpected method {method}"),
        };
        let response = wmcterm_ipc::Response::success(request.id.clone(), result);
        observed.lock().unwrap().push(request);
        response
    }).unwrap();
    let mut watcher = CodexWatcher::new(config);
    for thread in ["old", "new"] {
        let (mut socket, mut peer) = pair();
        let registrations = &mut registrations;
        std::thread::scope(|scope| {
            let server = scope.spawn(move || {
                let initialize = read(&mut peer);
                peer.send(Message::Text(json!({"id": initialize["id"], "result": {}}).to_string().into())).unwrap();
                assert_eq!(read(&mut peer)["method"], "initialized");
                let loaded = read(&mut peer);
                assert_eq!(loaded["method"], "thread/loaded/list");
                for value in [approval(thread), input(thread), json!({"id": loaded["id"], "result": {"data": [], "nextCursor": null}})] {
                    peer.send(Message::Text(value.to_string().into())).unwrap();
                }
                for _ in 0..2 {
                    assert!(registrations.recv_timeout(Duration::from_secs(5)).unwrap().ends_with(thread));
                }
                if thread == "new" {
                    assert_eq!(read(&mut peer), json!({"id": 91, "result": {"decision": "accept"}}));
                    assert_eq!(read(&mut peer), json!({"id": "q", "result": {"answers": {"target": {"answers": ["new connection"]}}}}));
                }
                peer.close(None).unwrap();
            });
            watcher
                .run_session(&mut socket, &AtomicBool::new(false))
                .unwrap();
            server.join().unwrap();
        });
        assert!(watcher.approvals.pending_ids().is_empty());
    }
    let calls = calls.lock().unwrap();
    let cancellations: Vec<_> = calls
        .iter()
        .filter(|request| request.method == "agent.request.cancel")
        .map(|request| {
            (
                request.params["request_id"].as_str().unwrap(),
                request.params["reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        cancellations,
        [
            ("command_execution:old", "app_server_disconnected"),
            ("input:old", "codex_disconnected")
        ]
    );
    let first_new_registration = calls
        .iter()
        .position(|request| {
            request
                .params
                .pointer("/request/thread_id")
                .and_then(Value::as_str)
                == Some("new")
        })
        .unwrap();
    assert!(
        calls[first_new_registration..]
            .iter()
            .all(|request| !control_id(request).ends_with(":old"))
    );
}
