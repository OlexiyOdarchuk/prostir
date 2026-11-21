//! `prostir mcp` over its stdin and stdout, on a session bus of the test's own (as in
//! `api.rs`, skipped where `dbus-daemon` is not installed): the protocol around the
//! tools, a monitor added and waited for, a screenshot (the fake desktop takes none:
//! a clean error), and every tool's error while Prostir is not running.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::pc::{Bus, Options, TestDir};
use crate::{START, session};

/// A running `prostir mcp`, and what it wrote to stdout, a line each.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn start(bus: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_prostir"))
            .arg("mcp")
            .env("DBUS_SESSION_BUS_ADDRESS", bus)
            .env("LANGUAGE", "en")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run prostir mcp");
        let out = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// The answer to `method` with `params`; every line on stdout is JSON-RPC.
    fn ask(&mut self, method: &str, params: &Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let line = self
            .lines
            .recv_timeout(START)
            .unwrap_or_else(|_| panic!("no answer to {method}"));
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(reply["jsonrpc"], "2.0", "{line}");
        assert_eq!(reply["id"], id, "{line}");
        reply
    }

    /// What tool `name` gave: whether it failed, and its content.
    fn tool(&mut self, name: &str, args: &Value) -> (bool, Vec<Value>) {
        let reply = self.ask("tools/call", &json!({"name": name, "arguments": args}));
        let result = &reply["result"];
        let content = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply}"))
            .clone();
        (result["isError"] == true, content)
    }

    fn initialize(&mut self) {
        let init = self.ask(
            "initialize",
            &json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "e2e", "version": "1"}}),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// Its stdin closed: it ends, with 0.
    fn finish(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert_eq!(status.code(), Some(0));
        assert!(self.lines.try_recv().is_err(), "nothing more on stdout");
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The JSON of a tool's first text.
fn said(content: &[Value]) -> Value {
    serde_json::from_str(content[0]["text"].as_str().expect("text first")).unwrap()
}

/// An agent adds a monitor, waits until it runs, sees it in the list, asks for its
/// picture (the fake desktop takes none: `Capture`), and removes it.
#[test]
fn an_agent_drives_prostir() {
    let (_dir, pc, _keys, _headset) = session("mcp", &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let mut mcp = Mcp::start(&bus.address);
    mcp.initialize();
    let tools = mcp.ask("tools/list", &json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for wanted in [
        "get_state",
        "add_monitor",
        "wait_for_state",
        "screenshot_monitor",
    ] {
        assert!(names.contains(&wanted), "{names:?}");
    }

    let (failed, content) = mcp.tool("add_monitor", &json!({"w": 640, "h": 360}));
    assert!(!failed, "{content:?}");
    let id = said(&content)["id"].as_u64().expect("its id");
    let (failed, content) = mcp.tool(
        "wait_for_state",
        &json!({"pointer": "/displays", "contains": {"id": id, "state": "running"},
               "timeout": START.as_secs()}),
    );
    assert!(!failed, "{content:?}");
    let (failed, content) = mcp.tool("list_monitors", &json!({}));
    assert!(!failed);
    let displays = said(&content)["displays"].as_array().unwrap().clone();
    assert!(displays.iter().any(|d| d["id"] == id), "{displays:?}");

    let (failed, content) = mcp.tool("screenshot_monitor", &json!({"id": id}));
    assert!(failed, "{content:?}");
    assert_eq!(said(&content)["error"], "Capture");
    let (failed, content) = mcp.tool("screenshot_monitor", &json!({"id": 99_999}));
    assert!(failed);
    assert_eq!(said(&content)["error"], "UnknownDisplay");

    let (failed, content) = mcp.tool("remove_monitor", &json!({"id": id}));
    assert!(!failed, "{content:?}");
    // The answer comes after the state that shows it.
    let (_, content) = mcp.tool("list_monitors", &json!({}));
    let displays = said(&content)["displays"].as_array().unwrap().clone();
    assert!(!displays.iter().any(|d| d["id"] == id), "{displays:?}");
    mcp.finish();
}

/// Without Prostir every tool says `NotRunning` at once, and nothing starts it.
#[test]
fn an_agent_without_prostir() {
    let dir = TestDir::new("mcp-none");
    let Some(bus) = Bus::start(dir.path()) else {
        return;
    };
    let mut mcp = Mcp::start(&bus.address);
    mcp.initialize();
    let started = std::time::Instant::now();
    for (name, args) in [
        ("get_state", json!({})),
        ("add_monitor", json!({"w": 640, "h": 360})),
        ("recenter", json!({})),
        ("wait_for_state", json!({"timeout": 5})),
        ("screenshot_monitor", json!({"id": 1})),
    ] {
        let (failed, content) = mcp.tool(name, &args);
        assert!(failed, "{name}");
        assert_eq!(said(&content)["error"], "NotRunning", "{name}");
    }
    assert!(started.elapsed() < Duration::from_secs(5), "at once");
    let unknown = mcp.ask("tools/call", &json!({"name": "fly"}));
    assert_eq!(unknown["error"]["code"], -32602);
    mcp.finish();
}
