//! Single-instance IPC (WP-08): handoff to a running instance, stale sockets, socket privacy,
//! malformed requests, and the real binary exiting 0 after handing over its request.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use tuner::cli::ViewArg;
use tuner::ipc::{Handoff, IpcRequest, IpcServer, handoff};
use tuner::msg::{AppEvent, EventBus};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap()
}

/// `<tmp>/run/tuner.sock`, like `AppPaths::rooted(<tmp>).ipc_socket()`.
fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("run").join("tuner.sock")
}

fn next_ipc(rx: &Receiver<AppEvent>) -> IpcRequest {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.checked_duration_since(Instant::now()).expect("no IPC event within 5 s");
        match rx.recv_timeout(left) {
            Ok(AppEvent::Ipc(req)) => return req,
            Ok(_) => continue,
            Err(e) => panic!("no IPC event: {e}"),
        }
    }
}

/// A handoff as the primary-to-be does it (removing a stale socket): delivered or not.
fn delivered(sock: &Path, req: &IpcRequest) -> bool {
    handoff(sock, req, true).unwrap() == Handoff::Delivered
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// Leave a socket file nobody listens on, like a crashed instance does. Other tests spawn
/// processes concurrently, and a child forked before its `exec` briefly inherits the listener,
/// so wait until connecting is really refused.
fn make_stale(sock: &Path) {
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    drop(UnixListener::bind(sock).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(sock).err().map(|e| e.kind()) != Some(std::io::ErrorKind::ConnectionRefused) {
        assert!(Instant::now() < deadline, "socket never became stale");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(sock.exists());
}

#[test]
fn handoff_delivers_the_request_to_the_running_instance() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let (bus, rx) = EventBus::new(None);
    let _server = IpcServer::start(rt.handle(), &sock, bus).unwrap();

    let req = IpcRequest { view: Some(ViewArg::Guide), ..Default::default() };
    assert!(delivered(&sock, &req));
    assert_eq!(next_ipc(&rx), req);

    let req = IpcRequest {
        view: None,
        play: Some("2:xt:live:1234".into()),
        inputs: vec!["/tmp/a b.m3u".into(), "http://h.tv/x.m3u8".into()],
        ..Default::default()
    };
    assert!(delivered(&sock, &req));
    assert_eq!(next_ipc(&rx), req);
}

#[test]
fn socket_and_directory_are_private() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let (bus, _rx) = EventBus::new(None);
    let _server = IpcServer::start(rt.handle(), &sock, bus).unwrap();
    assert_eq!(mode(&sock), 0o600);
    assert_eq!(mode(sock.parent().unwrap()), 0o700);
}

#[test]
fn no_socket_means_become_primary() {
    let dir = tempfile::tempdir().unwrap();
    // neither the runtime dir nor the socket exist yet (handoff runs before `paths.ensure`)
    assert!(!delivered(&socket_in(&dir), &IpcRequest::default()));
}

#[test]
fn stale_socket_is_removed_and_replaced() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    make_stale(&sock);
    assert!(!delivered(&sock, &IpcRequest::default()), "nobody listens: become primary");
    assert!(!sock.exists(), "stale socket removed");

    // a stale file left at server start is replaced too
    make_stale(&sock);
    let (bus, rx) = EventBus::new(None);
    let _server = IpcServer::start(rt.handle(), &sock, bus).unwrap();
    let req = IpcRequest { view: Some(ViewArg::Live), ..Default::default() };
    assert!(delivered(&sock, &req));
    assert_eq!(next_ipc(&rx), req);
}

#[test]
fn a_live_server_is_never_replaced_and_cleans_up_on_drop() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let (bus, rx) = EventBus::new(None);
    let first = IpcServer::start(rt.handle(), &sock, bus.clone()).unwrap();
    assert!(IpcServer::start(rt.handle(), &sock, bus.clone()).is_err(), "second server must not steal the socket");
    let req = IpcRequest { view: Some(ViewArg::Settings), ..Default::default() };
    assert!(delivered(&sock, &req), "first server still answers");
    assert_eq!(next_ipc(&rx), req);

    drop(first);
    assert!(!sock.exists(), "socket removed on shutdown");
    assert!(!delivered(&sock, &req));
    let _again = IpcServer::start(rt.handle(), &sock, bus).unwrap();
    assert!(delivered(&sock, &req));
}

#[test]
fn a_regular_file_in_the_way_is_left_alone() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    std::fs::write(&sock, "not a socket").unwrap();
    assert!(!delivered(&sock, &IpcRequest::default()));
    let (bus, _rx) = EventBus::new(None);
    assert!(IpcServer::start(rt.handle(), &sock, bus).is_err());
    assert_eq!(std::fs::read_to_string(&sock).unwrap(), "not a socket");
}

#[test]
fn malformed_requests_are_rejected_and_the_server_keeps_serving() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let (bus, rx) = EventBus::new(None);
    let _server = IpcServer::start(rt.handle(), &sock, bus).unwrap();

    let s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    (&s).write_all(b"{\"view\":\"nonsense\"}\n").unwrap();
    let mut reply = String::new();
    BufReader::new(&s).read_line(&mut reply).unwrap();
    let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(v["ok"], false, "{reply}");
    assert!(v["error"].as_str().unwrap().contains("bad request"), "{reply}");

    // a client that connects and says nothing doesn't block others
    let _idle = UnixStream::connect(&sock).unwrap();
    let req = IpcRequest { view: Some(ViewArg::Movies), ..Default::default() };
    assert!(delivered(&sock, &req));
    assert_eq!(next_ipc(&rx), req);
    assert!(rx.try_recv().is_err(), "the malformed request produced no event");
}

/// The real binary: a second `tuner --view guide file.m3u` hands over and exits 0 without
/// opening a window (no Wayland/X display is reachable from it, as a safety net).
#[test]
fn second_instance_binary_hands_over_and_exits_zero() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let (bus, rx) = EventBus::new(None);
    let _server = IpcServer::start(rt.handle(), &sock, bus).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tuner"))
        .arg("--profile-dir")
        .arg(dir.path())
        .args(["--view", "guide", "--play", "1:xt:live:42", "/tmp/list.m3u"])
        .env("WAYLAND_DISPLAY", "tuner-test-no-such-display")
        .env_remove("DISPLAY")
        .env("RUST_LOG", "off")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("second instance did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "exit status {status}");
    assert_eq!(
        next_ipc(&rx),
        IpcRequest { view: Some(ViewArg::Guide), play: Some("1:xt:live:42".into()), inputs: vec!["/tmp/list.m3u".into()], ..Default::default() }
    );
}
