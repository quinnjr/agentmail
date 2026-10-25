//! Task 6: wire protocol, daemon, client and autostart.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::pin;
use std::time::{Duration, Instant};

use agentmail::client::Client;
use agentmail::daemon::handler::TOUCH_INTERVAL_MS;
use agentmail::daemon::{DaemonConfig, MAX_SOCKET_PATH_BYTES, serve};
use agentmail::model::HUMAN;
use agentmail::paths::Paths;
use agentmail::protocol::{
    Caller, Envelope, ErrorCode, Event, MAX_REQUEST_BYTES, PROTOCOL_VERSION, Request, Response,
};
use common::{TestDaemon, agent, bin, register, send_req};
use futures_util::StreamExt;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Send one raw line on a fresh connection and return the first response.
async fn raw_exchange(paths: &Paths, lines: &[String]) -> Vec<Response> {
    let stream = UnixStream::connect(&paths.socket).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut out = Vec::new();
    for line in lines {
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        let mut buf = String::new();
        reader.read_line(&mut buf).await.unwrap();
        out.push(serde_json::from_str(&buf).unwrap());
    }
    out
}

fn line(caller: Caller, req: Request) -> String {
    serde_json::to_string(&Envelope { caller, req }).unwrap()
}

fn err_code(r: &Response) -> ErrorCode {
    match r {
        Response::Err { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// PIDs of live `agentmail daemon run` processes serving `data_dir`.
fn daemons_for(data_dir: &Path) -> Vec<u32> {
    let needle = format!("AGENTMAIL_DATA_DIR={}", data_dir.display()).into_bytes();
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        if !args.windows(2).any(|w| w == [&b"daemon"[..], &b"run"[..]]) {
            continue;
        }
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        if environ.split(|b| *b == 0).any(|v| v == needle.as_slice()) {
            out.push(pid);
        }
    }
    out
}

/// How long to wait for spawned daemons to come and go. Generous, as
/// process startup can take seconds on a heavily loaded machine.
const PROCESS_WAIT: Duration = Duration::from_secs(10);

/// Stop spawned daemons and wait until no process serves `paths`.
///
/// Autostart spawns a spare daemon whenever the first is slow to answer.
/// A spare normally exits on finding the lock held, but one that is slower
/// still can take the lock once the first has shut down, and then serve.
/// So `Shutdown` is sent again for as long as any daemon remains.
async fn stop_spawned(paths: &Paths) {
    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        let live = daemons_for(&paths.data_dir);
        if live.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "spawned daemons did not stop: {live:?}"
        );
        let _ = Client::shutdown(paths).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until `pid` is the only daemon for `data_dir`: spare daemons that
/// autostart spawned while the first was slow to answer exit once they
/// find the lock held, but may still be running when the client returns.
fn assert_sole_daemon(data_dir: &Path, pid: u32) {
    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        let live = daemons_for(data_dir);
        if live == [pid] {
            return;
        }
        assert!(
            live.contains(&pid) && Instant::now() < deadline,
            "expected only daemon {pid}, found {live:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

async fn hello_pid(c: &mut Client) -> u64 {
    let data = c
        .call(
            &Caller::Human,
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
    assert_eq!(data["version"], PROTOCOL_VERSION);
    data["pid"].as_u64().unwrap()
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn register_send_inbox_over_socket() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    assert!(a.starts_with("frontend-"), "{a}");
    assert!(b.starts_with("backend-"), "{b}");

    let sent = c
        .call(
            &agent(&a),
            send_req(&[&b], "orders", "POST /orders is live"),
        )
        .await
        .unwrap();
    let id = sent["id"].as_str().unwrap().to_string();
    assert_eq!(sent["from"], a.as_str());
    assert_eq!(sent["to"][0], b.as_str());

    let inbox = c
        .call(&agent(&b), Request::Inbox { all: false })
        .await
        .unwrap();
    assert_eq!(ids(&inbox), vec![id.clone()]);
    assert_eq!(inbox[0]["body"], "POST /orders is live");
    assert_eq!(inbox[0]["from_contact"], false);
    let counts = c.call(&agent(&b), Request::Counts).await.unwrap();
    assert_eq!(counts["unread"], 1);
    assert_eq!(counts["open_questions"], 0);

    let read = c
        .call(&agent(&b), Request::Read { id: id.clone() })
        .await
        .unwrap();
    assert_eq!(read["id"], id.as_str());
    assert_eq!(read["body"], "POST /orders is live");

    let inbox = c
        .call(&agent(&b), Request::Inbox { all: false })
        .await
        .unwrap();
    assert!(ids(&inbox).is_empty(), "{inbox}");
    let all = c
        .call(&agent(&b), Request::Inbox { all: true })
        .await
        .unwrap();
    assert_eq!(ids(&all), vec![id]);
    let counts = c.call(&agent(&b), Request::Counts).await.unwrap();
    assert_eq!(counts["unread"], 0);
}

#[tokio::test]
async fn version_mismatch_rejected() {
    let d = TestDaemon::start().await;
    let resp = raw_exchange(
        &d.paths,
        &[line(Caller::Human, Request::Hello { version: 999 })],
    )
    .await;
    assert_eq!(err_code(&resp[0]), ErrorCode::VersionMismatch);
}

#[tokio::test]
async fn failed_rehello_revokes_the_handshake() {
    let d = TestDaemon::start().await;
    let resp = raw_exchange(
        &d.paths,
        &[
            line(
                Caller::Human,
                Request::Hello {
                    version: PROTOCOL_VERSION,
                },
            ),
            line(Caller::Human, Request::Hello { version: 999 }),
            line(Caller::Human, Request::Counts),
        ],
    )
    .await;
    assert!(matches!(resp[0], Response::Ok { .. }), "{:?}", resp[0]);
    assert_eq!(err_code(&resp[1]), ErrorCode::VersionMismatch);
    assert_eq!(err_code(&resp[2]), ErrorCode::VersionMismatch);
}

#[tokio::test]
async fn request_before_hello_rejected() {
    let d = TestDaemon::start().await;
    let resp = raw_exchange(&d.paths, &[line(Caller::Human, Request::Counts)]).await;
    assert_eq!(err_code(&resp[0]), ErrorCode::VersionMismatch);
}

#[tokio::test]
async fn malformed_line_is_invalid_input_and_connection_survives() {
    let d = TestDaemon::start().await;
    let resp = raw_exchange(
        &d.paths,
        &[
            "{not json".to_string(),
            line(
                Caller::Human,
                Request::Hello {
                    version: PROTOCOL_VERSION,
                },
            ),
        ],
    )
    .await;
    assert_eq!(err_code(&resp[0]), ErrorCode::InvalidInput);
    assert!(matches!(resp[1], Response::Ok { .. }), "{:?}", resp[1]);
}

#[tokio::test]
async fn connect_without_daemon_fails_and_does_not_spawn() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    assert!(Client::connect(&paths).await.is_err());
    assert!(!paths.socket.exists());
}

#[tokio::test]
async fn stale_socket_is_replaced() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    std::fs::write(&paths.socket, b"stale").unwrap();

    let mut c = Client::connect_or_spawn_with(&paths, &bin()).await.unwrap();
    let pid = hello_pid(&mut c).await;
    assert_sole_daemon(&paths.data_dir, pid as u32);
    let counts = c.call(&Caller::Human, Request::Counts).await.unwrap();
    assert_eq!(counts["unread"], 0);
    stop_spawned(&paths).await;
}

#[tokio::test]
async fn stale_unix_socket_without_listener_is_replaced() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    // A real socket file left behind by a crashed daemon: bound, then closed.
    drop(UnixListener::bind(&paths.socket).unwrap());
    assert!(paths.socket.exists());

    let mut c = Client::connect_or_spawn_with(&paths, &bin()).await.unwrap();
    hello_pid(&mut c).await;
    stop_spawned(&paths).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_autostart_single_daemon() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let spawn = |paths: Paths| {
        tokio::spawn(async move {
            let mut c = Client::connect_or_spawn_with(&paths, &bin()).await.unwrap();
            let pid = hello_pid(&mut c).await;
            c.call(&Caller::Human, Request::Counts).await.unwrap();
            pid
        })
    };
    let (x, y) = tokio::join!(spawn(paths.clone()), spawn(paths.clone()));
    let (x, y) = (x.unwrap(), y.unwrap());
    assert_eq!(x, y, "both clients must reach the same daemon");

    // Daemons that lost the lock exit; exactly one remains.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let live = daemons_for(&paths.data_dir);
        if live.len() <= 1 {
            assert_eq!(live, vec![x as u32]);
            break;
        }
        assert!(Instant::now() < deadline, "several daemons: {live:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    stop_spawned(&paths).await;
}

#[tokio::test]
async fn version_mismatch_triggers_shutdown_and_respawn() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    // A fake "old" daemon: refuses Hello, then honours Shutdown and goes away.
    let listener = UnixListener::bind(&paths.socket).unwrap();
    let socket = paths.socket.clone();
    let fake = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let mut ops = Vec::new();
        while let Some(l) = lines.next_line().await.unwrap() {
            let env: Envelope = serde_json::from_str(&l).unwrap();
            let resp = match env.req {
                Request::Hello { .. } => {
                    ops.push("hello");
                    Response::Err {
                        code: ErrorCode::VersionMismatch,
                        message: "old daemon".into(),
                        candidates: Vec::new(),
                    }
                }
                Request::Shutdown => {
                    ops.push("shutdown");
                    Response::Ok { data: Value::Null }
                }
                _ => panic!("unexpected request"),
            };
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            w.write_all(out.as_bytes()).await.unwrap();
            if ops.last() == Some(&"shutdown") {
                break;
            }
        }
        std::fs::remove_file(&socket).unwrap();
        drop(listener);
        ops
    });

    let mut c = Client::connect_or_spawn_with(&paths, &bin()).await.unwrap();
    assert_eq!(fake.await.unwrap(), vec!["hello", "shutdown"]);
    let pid = hello_pid(&mut c).await;
    assert_sole_daemon(&paths.data_dir, pid as u32);
    stop_spawned(&paths).await;
}

#[tokio::test]
async fn second_daemon_exits_quietly_when_locked() {
    let d = TestDaemon::start().await;
    let out = tokio::process::Command::new(bin())
        .args(["daemon", "run"])
        .env("AGENTMAIL_DATA_DIR", &d.paths.data_dir)
        .env("AGENTMAIL_SOCKET", &d.paths.socket)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(5), out)
        .await
        .expect("second daemon did not exit")
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    // The running daemon's socket was left alone.
    let mut c = d.client().await;
    c.call(&Caller::Human, Request::Counts).await.unwrap();
}

#[tokio::test]
async fn mail_survives_restart() {
    let mut d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let sent = c
        .call(&agent(&a), send_req(&[&b], "s", "kept"))
        .await
        .unwrap();
    drop(c);
    d.shutdown().await;
    assert!(!d.paths.socket.exists(), "shutdown removes the socket");

    d.restart().await;
    let mut c = d.client().await;
    let inbox = c
        .call(&agent(&b), Request::Inbox { all: false })
        .await
        .unwrap();
    assert_eq!(ids(&inbox), vec![sent["id"].as_str().unwrap().to_string()]);
}

#[tokio::test]
async fn registered_presence_event() {
    let d = TestDaemon::start().await;
    let mut watcher = d.client().await;
    let events = watcher
        .stream(&Caller::Human, Request::Watch { agent: None })
        .await
        .unwrap();
    let mut events = pin!(events);

    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;

    let ev = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("no event")
        .expect("stream ended")
        .unwrap();
    match ev {
        Event::Presence { agent } => assert_eq!(agent.agent.address, a),
        other => panic!("expected presence, got {other:?}"),
    }
}

#[tokio::test]
async fn watch_forwards_deliveries_filtered_by_agent() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let x = register(&mut c, "other", d.dir.path()).await;

    let mut watcher = d.client().await;
    let events = watcher
        .stream(
            &Caller::Human,
            Request::Watch {
                agent: Some(b.clone()),
            },
        )
        .await
        .unwrap();
    let mut events = pin!(events);

    // Not involving b: filtered out.
    c.call(&agent(&a), send_req(&[&x], "s", "not for b"))
        .await
        .unwrap();
    let sent = c
        .call(&agent(&a), send_req(&[&b], "s", "for b"))
        .await
        .unwrap();

    let ev = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("no event")
        .expect("stream ended")
        .unwrap();
    match ev {
        Event::Delivered { to, message } => {
            assert_eq!(to, b);
            assert_eq!(message.id, sent["id"].as_str().unwrap());
        }
        other => panic!("expected delivery, got {other:?}"),
    }
}

#[tokio::test]
async fn send_to_unknown_address_is_refused() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let err = c
        .call(&agent(&a), send_req(&["backend-zzzzzzzz"], "s", "b"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownRecipient, "{err:?}");
    let err = c
        .call(&agent(&a), send_req(&[&a], "s", "b"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRecipient, "{err:?}");
}

#[tokio::test]
async fn unknown_caller_address_is_no_identity() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let err = c
        .call(&agent("ghost-zzzzzzzz"), Request::Whoami)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity, "{err:?}");
}

#[tokio::test]
async fn whoami_agents_status_contacts() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;

    let me = c.call(&agent(&a), Request::Whoami).await.unwrap();
    assert_eq!(me["address"], a.as_str());
    assert_eq!(me["agent"]["name"], "frontend");
    let me = c.call(&Caller::Human, Request::Whoami).await.unwrap();
    assert_eq!(me["address"], HUMAN);
    assert!(me["agent"].is_null());

    c.call(
        &agent(&a),
        Request::Status {
            working_on: "checkout form".into(),
        },
    )
    .await
    .unwrap();
    c.call(&agent(&a), Request::ContactAdd { address: b.clone() })
        .await
        .unwrap();
    let contacts = c.call(&agent(&a), Request::Contacts).await.unwrap();
    assert_eq!(contacts.as_array().unwrap().len(), 1);
    assert_eq!(contacts[0]["address"], b.as_str());
    assert_eq!(contacts[0]["is_contact"], true);

    // Unbound agents are gone; `all` includes them.
    let listed = c
        .call(
            &agent(&a),
            Request::Agents {
                name_prefix: None,
                repo: None,
                all: false,
            },
        )
        .await
        .unwrap();
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    let listed = c
        .call(
            &agent(&a),
            Request::Agents {
                name_prefix: Some("back".into()),
                repo: None,
                all: true,
            },
        )
        .await
        .unwrap();
    let rows = listed.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["address"], b.as_str());
    assert_eq!(rows[0]["is_contact"], true);
    assert_eq!(rows[0]["presence"], "gone");

    let me = c.call(&agent(&a), Request::Whoami).await.unwrap();
    assert_eq!(me["agent"]["working_on"], "checkout form");

    c.call(&agent(&a), Request::ContactRemove { address: b.clone() })
        .await
        .unwrap();
    let contacts = c.call(&agent(&a), Request::Contacts).await.unwrap();
    assert!(contacts.as_array().unwrap().is_empty());

    c.call(&agent(&a), Request::Signoff).await.unwrap();
    let me = c.call(&agent(&a), Request::Whoami).await.unwrap();
    assert_eq!(me["agent"]["signed_off"], true);
}

#[tokio::test]
async fn threads_search_prune_and_read_thread() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let sent = c
        .call(&agent(&a), send_req(&[&b], "schema", "needle in body"))
        .await
        .unwrap();
    let id = sent["id"].as_str().unwrap().to_string();

    let threads = c
        .call(
            &Caller::Human,
            Request::Threads {
                filter: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(threads[0]["thread_id"], id.as_str());

    let found = c
        .call(
            &Caller::Human,
            Request::Search {
                query: "NEEDLE".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![id.clone()]);

    // Reading a thread root also returns the whole thread.
    let read = c
        .call(&agent(&b), Request::Read { id: id.clone() })
        .await
        .unwrap();
    assert_eq!(ids(&read["thread"]), vec![id.clone()]);

    let pruned = c
        .call(
            &Caller::Human,
            Request::Prune {
                older_than_secs: 3600,
            },
        )
        .await
        .unwrap();
    assert_eq!(pruned["pruned"], 0);
    tokio::time::sleep(Duration::from_millis(5)).await;
    // Only the human prunes.
    let err = c
        .call(&agent(&a), Request::Prune { older_than_secs: 0 })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput, "{err:?}");
    assert!(err.message.contains("human"), "{err:?}");
    let pruned = c
        .call(&Caller::Human, Request::Prune { older_than_secs: 0 })
        .await
        .unwrap();
    assert_eq!(pruned["pruned"], 1);
    let err = c.call(&agent(&b), Request::Read { id }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn socket_and_data_dir_permissions() {
    let d = TestDaemon::start().await;
    let sock = std::fs::metadata(&d.paths.socket).unwrap();
    assert_eq!(sock.permissions().mode() & 0o777, 0o600);

    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("nested").join("agentmail");
    let paths = Paths {
        db: data_dir.join("agentmail.redb"),
        lock: data_dir.join("agentmail.lock"),
        socket: data_dir.join("agentmail.sock"),
        data_dir: data_dir.clone(),
    };
    let mut c = Client::connect_or_spawn_with(&paths, &bin()).await.unwrap();
    hello_pid(&mut c).await;
    let meta = std::fs::metadata(&data_dir).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    stop_spawned(&paths).await;
}

#[tokio::test]
async fn daemon_exits_when_its_socket_is_removed() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let mut c = Client::connect_or_spawn_with(&paths, &bin()).await.unwrap();
    let pid = hello_pid(&mut c).await;
    // A spare daemon still waiting for the lock would take over once ours
    // exits, and serve on.
    assert_sole_daemon(&paths.data_dir, pid as u32);
    std::fs::remove_file(&paths.socket).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !daemons_for(&paths.data_dir).is_empty() {
        assert!(Instant::now() < deadline, "daemon kept running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn daemon_cli_start_status_stop() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let run = |args: &[&str]| -> (Option<i32>, String) {
        let out = assert_cmd::Command::new(bin())
            .args(args)
            .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
            .env("AGENTMAIL_SOCKET", &paths.socket)
            .timeout(Duration::from_secs(10))
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (code, out) = run(&["daemon", "status"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("not running"), "{out}");

    let (code, out) = run(&["daemon", "start"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("running (pid"), "{out}");
    let pid: u32 = out
        .split("running (pid ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no pid in {out:?}"));
    assert_sole_daemon(&paths.data_dir, pid);

    let (code, out) = run(&["daemon", "status"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("running (pid"), "{out}");

    let (code, out) = run(&["daemon", "stop"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("stopped"), "{out}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !daemons_for(&paths.data_dir).is_empty() {
        assert!(Instant::now() < deadline, "daemon kept running");
        std::thread::sleep(Duration::from_millis(25));
    }

    let (code, out) = run(&["daemon", "stop"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("not running"), "{out}");
}

#[tokio::test]
async fn register_records_git_toplevel_as_repo() {
    let d = TestDaemon::start().await;
    let repo = TempDir::new().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(ok.success());
    let sub = repo.path().join("web");
    std::fs::create_dir(&sub).unwrap();

    let mut c = d.client().await;
    let data = c
        .call(&Caller::Human, common::register_req("frontend", &sub))
        .await
        .unwrap();
    let toplevel = repo.path().canonicalize().unwrap();
    assert_eq!(data["agent"]["repo"], toplevel.to_str().unwrap());
    assert_eq!(data["agent"]["cwd"], sub.to_str().unwrap());
    assert_eq!(data["reclaimed"], false);

    // Outside any repository: no repo.
    let a = c
        .call(&Caller::Human, common::register_req("other", d.dir.path()))
        .await
        .unwrap();
    assert!(a["agent"]["repo"].is_null(), "{a}");

    // A relative cwd is refused.
    let err = c
        .call(
            &Caller::Human,
            common::register_req("x", std::path::Path::new("rel")),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput, "{err:?}");
}

/// A fake "old" daemon on `paths.socket`: refuses `Hello` with
/// `VersionMismatch` and honours `Shutdown` without one, then goes away.
/// Returns the requests it saw, one list per connection.
fn spawn_old_daemon(paths: &Paths) -> tokio::task::JoinHandle<Vec<Vec<&'static str>>> {
    let listener = UnixListener::bind(&paths.socket).unwrap();
    let socket = paths.socket.clone();
    tokio::spawn(async move {
        let mut conns = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let mut ops = Vec::new();
            while let Some(l) = lines.next_line().await.unwrap() {
                let env: Envelope = serde_json::from_str(&l).unwrap();
                let resp = match env.req {
                    Request::Hello { .. } => {
                        ops.push("hello");
                        Response::Err {
                            code: ErrorCode::VersionMismatch,
                            message: "old daemon".into(),
                            candidates: Vec::new(),
                        }
                    }
                    Request::Shutdown => {
                        ops.push("shutdown");
                        Response::Ok { data: Value::Null }
                    }
                    _ => {
                        ops.push("other");
                        Response::Err {
                            code: ErrorCode::VersionMismatch,
                            message: "send hello first".into(),
                            candidates: Vec::new(),
                        }
                    }
                };
                let mut out = serde_json::to_string(&resp).unwrap();
                out.push('\n');
                w.write_all(out.as_bytes()).await.unwrap();
            }
            let done = ops.contains(&"shutdown");
            conns.push(ops);
            if done {
                break;
            }
        }
        std::fs::remove_file(&socket).unwrap();
        conns
    })
}

async fn run_cli(paths: &Paths, args: &[&str]) -> (Option<i32>, String) {
    let out = tokio::process::Command::new(bin())
        .args(args)
        .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
        .env("AGENTMAIL_SOCKET", &paths.socket)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(10), out)
        .await
        .expect("agentmail timed out")
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[tokio::test]
async fn daemon_cli_status_and_stop_handle_a_daemon_with_another_protocol() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let fake = spawn_old_daemon(&paths);

    let (code, out) = run_cli(&paths, &["daemon", "status"]).await;
    assert_eq!(code, Some(3), "{out}");
    assert!(!out.contains("not running"), "{out}");
    assert!(out.contains("different protocol"), "{out}");
    assert!(out.contains("agentmail daemon stop"), "{out}");

    let (code, out) = run_cli(&paths, &["daemon", "stop"]).await;
    assert_eq!(code, Some(0), "{out}");
    assert!(!out.contains("not running"), "{out}");
    assert!(out.contains("stopped"), "{out}");

    let conns = tokio::time::timeout(Duration::from_secs(5), fake)
        .await
        .expect("fake daemon was never stopped")
        .unwrap();
    assert_eq!(conns, vec![vec!["hello"], vec!["hello", "shutdown"]]);
}

#[tokio::test]
async fn client_shutdown_reports_whether_a_daemon_answered() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    assert!(!Client::shutdown(&paths).await.unwrap());
    // A stale socket file with no listener is not a daemon either. A child
    // process another test is spawning can hold the dropped listener open
    // between fork and exec, so a connection may briefly be accepted and
    // then lost; it must never count as a daemon that answered.
    drop(UnixListener::bind(&paths.socket).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Client::shutdown(&paths).await {
            Ok(answered) => {
                assert!(!answered, "a stale socket answered");
                break;
            }
            Err(e) => assert!(Instant::now() < deadline, "{e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::remove_file(&paths.socket).unwrap();

    let fake = spawn_old_daemon(&paths);
    assert!(Client::shutdown(&paths).await.unwrap());
    assert_eq!(fake.await.unwrap(), vec![vec!["hello", "shutdown"]]);
}

#[tokio::test]
async fn client_shutdown_stops_a_daemon_that_does_not_report_its_data_dir() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let listener = UnixListener::bind(&paths.socket).unwrap();
    let fake = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let mut ops = Vec::new();
        while let Some(l) = lines.next_line().await.unwrap() {
            let env: Envelope = serde_json::from_str(&l).unwrap();
            let data = match env.req {
                Request::Hello { .. } => {
                    ops.push("hello");
                    serde_json::json!({ "version": PROTOCOL_VERSION })
                }
                Request::Shutdown => {
                    ops.push("shutdown");
                    Value::Null
                }
                _ => {
                    ops.push("other");
                    Value::Null
                }
            };
            let mut out = serde_json::to_string(&Response::Ok { data }).unwrap();
            out.push('\n');
            w.write_all(out.as_bytes()).await.unwrap();
        }
        ops
    });
    assert!(Client::shutdown(&paths).await.unwrap());
    assert_eq!(fake.await.unwrap(), vec!["hello", "shutdown"]);
}

#[tokio::test]
async fn oversized_request_is_too_large_and_closes_the_connection() {
    let d = TestDaemon::start().await;
    let stream = UnixStream::connect(&d.paths.socket).await.unwrap();
    let (r, mut w) = stream.into_split();
    // One byte past the limit, with no newline. The daemon stops reading
    // there, so write from a task and ignore a broken pipe.
    let writer = tokio::spawn(async move {
        let chunk = vec![b'x'; 64 * 1024];
        let mut left = MAX_REQUEST_BYTES + 1;
        while left > 0 {
            let n = left.min(chunk.len());
            if w.write_all(&chunk[..n]).await.is_err() {
                break;
            }
            left -= n;
        }
        w
    });
    let mut reader = BufReader::new(r);
    let mut buf = String::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut buf))
        .await
        .expect("no response to an oversized request")
        .unwrap();
    let resp: Response = serde_json::from_str(&buf).unwrap();
    assert_eq!(err_code(&resp), ErrorCode::TooLarge, "{resp:?}");
    buf.clear();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut buf))
        .await
        .expect("connection stayed open")
        .unwrap();
    assert_eq!(n, 0, "expected EOF, got {buf:?}");
    drop(writer.await.unwrap());

    let mut c = d.client().await;
    let me = c.call(&Caller::Human, Request::Whoami).await.unwrap();
    assert_eq!(me["address"], HUMAN);
}

#[tokio::test]
async fn unopenable_store_fails_without_leaving_a_socket() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    std::fs::create_dir(&paths.db).unwrap();
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        serve(DaemonConfig {
            paths: paths.clone(),
        }),
    )
    .await
    .expect("serve hung");
    assert!(
        res.is_err(),
        "serve succeeded with a directory for a database"
    );
    assert!(
        std::fs::symlink_metadata(&paths.socket).is_err(),
        "socket left behind"
    );
}

#[tokio::test]
async fn bind_leaves_no_staging_files_and_replaces_leftovers() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    // A crashed predecessor's staging directory and socket.
    let staging = dir.path().join(".agentmail.sock.d");
    std::fs::create_dir(&staging).unwrap();
    let _old = std::os::unix::net::UnixListener::bind(staging.join("s")).unwrap();
    std::fs::write(&paths.socket, b"stale").unwrap();

    let d = TestDaemon::start_in(dir).await;
    let mut c = d.client().await;
    c.call(&Caller::Human, Request::Whoami).await.unwrap();
    assert!(!staging.exists(), "staging directory left behind");
    let mode = std::fs::metadata(&d.paths.socket)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn last_seen_is_refreshed_at_most_once_per_interval() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "poller", d.dir.path()).await;
    let seen = |v: &Value| v["agent"]["last_seen"].as_i64().unwrap();
    let t0 = seen(&c.call(&agent(&a), Request::Whoami).await.unwrap());

    // Requests inside the interval do not write.
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.call(&agent(&a), Request::Counts).await.unwrap();
    let t1 = seen(&c.call(&agent(&a), Request::Whoami).await.unwrap());
    assert_eq!(t1, t0, "last_seen was rewritten within the interval");

    // Once the interval has passed, a request refreshes it.
    tokio::time::sleep(Duration::from_millis(TOUCH_INTERVAL_MS as u64 + 100)).await;
    c.call(&agent(&a), Request::Counts).await.unwrap();
    let t2 = seen(&c.call(&agent(&a), Request::Whoami).await.unwrap());
    assert!(
        t2 >= t0 + TOUCH_INTERVAL_MS,
        "last_seen did not advance: {t0} -> {t2}"
    );
}

/// A listener at `paths.socket` that accepts connections and never answers.
fn silent_listener(paths: &Paths) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(&paths.socket).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    })
}

#[tokio::test]
async fn handshake_with_a_silent_peer_times_out() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let task = silent_listener(&paths);
    // A binary that would never serve: the client must not wait on it.
    let start = Instant::now();
    let err = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect_or_spawn_with(&paths, Path::new("/bin/true")),
    )
    .await
    .expect("connect_or_spawn_with hung")
    .err()
    .expect("connected to a silent peer");
    let msg = format!("{err:#}");
    assert!(msg.contains("did not answer"), "{msg}");
    assert!(msg.contains(&paths.socket.display().to_string()), "{msg}");
    assert!(start.elapsed() < Duration::from_secs(6));

    let err = tokio::time::timeout(Duration::from_secs(10), Client::connect(&paths))
        .await
        .expect("connect hung")
        .err()
        .expect("connected to a silent peer");
    assert!(format!("{err:#}").contains("did not answer"), "{err:#}");

    let err = tokio::time::timeout(Duration::from_secs(10), Client::shutdown(&paths))
        .await
        .expect("shutdown hung")
        .expect_err("a silent peer acknowledged shutdown");
    assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
    task.abort();
}

#[tokio::test]
async fn autostart_reports_a_lock_held_without_a_socket() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    // A daemon that holds the lock but listens somewhere else.
    let lock = std::fs::File::create(&paths.lock).unwrap();
    lock.try_lock().unwrap();
    assert!(paths.lock_held());

    let err = tokio::time::timeout(
        Duration::from_secs(15),
        Client::connect_or_spawn_with(&paths, &bin()),
    )
    .await
    .expect("connect_or_spawn_with hung")
    .err()
    .expect("connected without a daemon");
    let msg = format!("{err:#}");
    assert!(msg.contains(&paths.lock.display().to_string()), "{msg}");
    assert!(msg.contains("not listening"), "{msg}");
    assert!(msg.contains("XDG_RUNTIME_DIR"), "{msg}");
    // The daemons autostart spawned give up on the held lock and exit. Wait
    // for that before releasing it, or one still retrying would take it
    // and serve on.
    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        let live = daemons_for(&paths.data_dir);
        if live.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "spawned daemons kept running: {live:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(lock);
    // A child another test thread is forking shares our lock until it
    // execs, so the release can take a moment to show.
    let deadline = Instant::now() + Duration::from_secs(2);
    while paths.lock_held() {
        assert!(Instant::now() < deadline, "lock still held after release");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Paths in `dir` with a socket path exactly `len` bytes long.
fn paths_with_socket_len(dir: &Path, len: usize) -> Paths {
    let mut paths = Paths::in_dir(dir);
    let base = dir.as_os_str().len() + 1;
    assert!(len > base + 5, "temp dir path too long for this test");
    paths.socket = dir.join("s".repeat(len - base));
    assert_eq!(paths.socket.as_os_str().len(), len);
    paths
}

#[tokio::test]
async fn longest_socket_path_that_fits_is_served() {
    let dir = TempDir::new().unwrap();
    let paths = paths_with_socket_len(dir.path(), MAX_SOCKET_PATH_BYTES);
    let task = tokio::spawn({
        let paths = paths.clone();
        async move { serve(DaemonConfig { paths }).await }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut c = loop {
        if let Ok(c) = Client::connect(&paths).await {
            break c;
        }
        assert!(!task.is_finished(), "{:?}", task.await.unwrap());
        assert!(Instant::now() < deadline, "daemon did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    c.call(&Caller::Human, Request::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn too_long_socket_path_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let paths = paths_with_socket_len(dir.path(), MAX_SOCKET_PATH_BYTES + 1);
    let err = tokio::time::timeout(
        Duration::from_secs(10),
        serve(DaemonConfig {
            paths: paths.clone(),
        }),
    )
    .await
    .expect("serve hung")
    .expect_err("served on a socket path that is too long");
    let msg = format!("{err:#}");
    assert!(msg.contains("too long"), "{msg}");
    assert!(msg.contains(&paths.socket.display().to_string()), "{msg}");
    assert!(!msg.contains(".d/s"), "{msg}");
    assert!(std::fs::symlink_metadata(&paths.socket).is_err());
}

#[tokio::test]
async fn hello_reports_the_data_dir() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let data = c
        .call(
            &Caller::Human,
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        data["data_dir"].as_str().unwrap(),
        d.paths.data_dir.to_str().unwrap()
    );
}

#[tokio::test]
async fn client_refuses_a_daemon_serving_another_data_dir() {
    let d = TestDaemon::start().await;
    let other = TempDir::new().unwrap();
    // Shares the daemon's socket (as XDG_RUNTIME_DIR would) but names
    // another data directory.
    let paths = Paths {
        socket: d.paths.socket.clone(),
        ..Paths::in_dir(other.path())
    };
    let check = |err: anyhow::Error| {
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&d.paths.data_dir.display().to_string()),
            "{msg}"
        );
        assert!(msg.contains(&other.path().display().to_string()), "{msg}");
        assert!(msg.contains("AGENTMAIL_SOCKET"), "{msg}");
    };

    check(Client::connect(&paths).await.err().expect("connected"));
    // Autostart must not start another daemon either: it would take over
    // the shared socket.
    check(
        Client::connect_or_spawn_with(&paths, &bin())
            .await
            .err()
            .expect("connected"),
    );
    assert!(daemons_for(other.path()).is_empty());
    assert!(!paths.lock_held());
    // The daemon itself is unaffected.
    d.client().await;
}

#[tokio::test]
async fn client_accepts_another_spelling_of_the_same_data_dir() {
    let d = TestDaemon::start().await;
    let mut spelled = d.paths.data_dir.join("sub").join("..");
    spelled.push(".");
    let paths = Paths {
        data_dir: spelled,
        ..d.paths.clone()
    };
    Client::connect(&paths).await.unwrap();
}

#[tokio::test]
async fn daemon_status_reports_a_daemon_for_another_data_dir() {
    let d = TestDaemon::start().await;
    let other = TempDir::new().unwrap();
    let paths = Paths {
        socket: d.paths.socket.clone(),
        ..Paths::in_dir(other.path())
    };
    let (code, out) = run_cli(&paths, &["daemon", "status"]).await;
    assert_eq!(code, Some(1), "{out}");
    assert!(!out.contains("not running"), "{out}");
    assert!(out.contains("not for this data directory"), "{out}");
    assert!(out.contains("AGENTMAIL_SOCKET"), "{out}");
}

#[tokio::test]
async fn daemon_stop_refuses_a_daemon_for_another_data_dir() {
    let d = TestDaemon::start().await;
    let other = TempDir::new().unwrap();
    let paths = Paths {
        socket: d.paths.socket.clone(),
        ..Paths::in_dir(other.path())
    };
    let (code, out) = run_cli(&paths, &["daemon", "stop"]).await;
    assert_eq!(code, Some(1), "{out}");
    assert!(!out.contains("daemon stopped"), "{out}");
    assert!(out.contains("not for this data directory"), "{out}");
    assert!(out.contains("AGENTMAIL_SOCKET"), "{out}");

    let err = Client::shutdown(&paths).await.expect_err("stopped it");
    assert!(
        err.downcast_ref::<agentmail::client::DataDirMismatch>()
            .is_some(),
        "{err:#}"
    );
    // The daemon is still serving.
    let mut c = d.client().await;
    c.call(&Caller::Human, Request::Whoami).await.unwrap();
    assert!(d.paths.lock_held());
}

#[tokio::test]
async fn daemon_starts_despite_a_momentary_lock_probe() {
    // `Paths::lock_held` (run by `daemon stop` while it waits) takes the
    // lock for an instant when it is free. A daemon starting then must not
    // give up.
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let probe = std::fs::File::create(&paths.lock).unwrap();
    probe.try_lock().unwrap();
    let handle = tokio::spawn(serve(DaemonConfig {
        paths: paths.clone(),
    }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    // A child another test thread forks right now shares the probe's lock
    // until it execs, so the lock may stay held a little longer than this;
    // the daemon's retry window allows for that.
    drop(probe);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut c = loop {
        if let Ok(c) = Client::connect(&paths).await {
            break c;
        }
        assert!(
            !handle.is_finished(),
            "daemon gave up on a briefly held lock: {:?}",
            handle.await
        );
        assert!(Instant::now() < deadline, "daemon did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    c.call(&Caller::Human, Request::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("daemon did not stop")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn client_refuses_calls_after_streaming() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    {
        let events = c
            .stream(&Caller::Human, Request::Watch { agent: None })
            .await
            .unwrap();
        drop(events);
    }
    // Something is published while the stream is gone, so a confused client
    // would read the event as its reply.
    let mut other = d.client().await;
    register(&mut other, "frontend", d.dir.path()).await;

    let err = c.call(&Caller::Human, Request::Counts).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(err.message.contains("watch mode"), "{}", err.message);
    let err = match c
        .stream(&Caller::Human, Request::Watch { agent: None })
        .await
    {
        Ok(_) => panic!("stream on a watching connection must fail"),
        Err(e) => e,
    };
    assert!(err.message.contains("watch mode"), "{}", err.message);
}

/// A daemon that reads `Shutdown` and closes the connection without
/// replying is going away, which is what was asked for.
#[tokio::test]
async fn shutdown_closed_without_reply_counts_as_acknowledged() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let listener = UnixListener::bind(&paths.socket).unwrap();
    let fake = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let mut ops = Vec::new();
        while let Some(l) = lines.next_line().await.unwrap() {
            let env: Envelope = serde_json::from_str(&l).unwrap();
            match env.req {
                Request::Hello { .. } => {
                    ops.push("hello");
                    let resp = Response::Err {
                        code: ErrorCode::VersionMismatch,
                        message: "old daemon".into(),
                        candidates: Vec::new(),
                    };
                    let mut out = serde_json::to_string(&resp).unwrap();
                    out.push('\n');
                    w.write_all(out.as_bytes()).await.unwrap();
                }
                Request::Shutdown => {
                    ops.push("shutdown");
                    break;
                }
                _ => panic!("unexpected request"),
            }
        }
        ops
    });
    assert!(Client::shutdown(&paths).await.unwrap());
    assert_eq!(fake.await.unwrap(), vec!["hello", "shutdown"]);
}
