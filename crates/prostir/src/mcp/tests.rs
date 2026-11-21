use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// A Prostir that answers as a test says, keeps what it was asked, and shows the
/// states it is given one after the other (the last one stays).
#[derive(Clone)]
struct Fake {
    answer: Result<Answer, ClientError>,
    states: Rc<RefCell<VecDeque<(u32, String)>>>,
    asked: Rc<RefCell<Vec<Request>>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            answer: Ok(Answer {
                revision: 7,
                ..Answer::default()
            }),
            states: Rc::new(RefCell::new(VecDeque::from([(1, state("starting"))]))),
            asked: Rc::default(),
        }
    }
}

impl CtlClient for Fake {
    fn call(&self, request: &Request) -> Result<Answer, ClientError> {
        self.asked.borrow_mut().push(request.clone());
        self.answer.clone()
    }

    fn state(&self) -> Result<(u32, String), ClientError> {
        let mut states = self.states.borrow_mut();
        let state = if states.len() > 1 {
            states.pop_front()
        } else {
            states.front().cloned()
        };
        state.ok_or(ClientError::NotRunning)
    }

    fn running(&self) -> bool {
        !self.states.borrow().is_empty()
    }

    fn watch(&self, _each: &mut dyn FnMut(Watched) -> bool) -> Result<(), ClientError> {
        Ok(())
    }
}

/// A state with monitor 103 standing so.
fn state(display: &str) -> String {
    json!({"v": 1, "primary": 103, "displays": [
        {"id": 7, "kind": "physical", "state": "running"},
        {"id": 103, "kind": "virtual", "name": "Prostir-1", "state": display},
    ]})
    .to_string()
}

fn server(fake: &Fake) -> Server {
    let start = Box::new(|| Ok("Prostir is running".to_owned()));
    let mut server = Server::new(Ok(Box::new(fake.clone())), start, None);
    server.poll = Duration::from_millis(5);
    server
}

/// The answer to one request, as JSON.
fn ask(server: &mut Server, method: &str, params: &Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let reply = server.handle_line(&line.to_string()).expect("an answer");
    serde_json::from_str(&reply).unwrap()
}

/// What tool `name` gave with `args`: whether it failed, and its content.
fn tool(server: &mut Server, name: &str, args: &Value) -> (bool, Vec<Value>) {
    let reply = ask(
        server,
        "tools/call",
        &json!({"name": name, "arguments": args}),
    );
    let result = &reply["result"];
    assert!(result.is_object(), "{reply}");
    let failed = result["isError"].as_bool().unwrap_or(false);
    (failed, result["content"].as_array().unwrap().clone())
}

/// The JSON of the first text of a tool's content.
fn said(content: &[Value]) -> Value {
    assert_eq!(content[0]["type"], "text");
    serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap()
}

/// `initialize` takes the client's version where it is one of ours, else gives the
/// newest; it says there are tools.
#[test]
fn initialize_agrees_on_a_version() {
    let mut server = server(&Fake::new());
    let init = |s: &mut Server, version: &str| {
        ask(
            s,
            "initialize",
            &json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
        )
    };
    let reply = init(&mut server, "2025-06-18");
    assert_eq!(reply["jsonrpc"], "2.0");
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(reply["result"]["serverInfo"]["name"], "prostir");
    assert!(reply["result"]["capabilities"]["tools"].is_object());
    let reply = init(&mut server, "2099-01-01");
    assert_eq!(reply["result"]["protocolVersion"], VERSIONS[0]);
    assert_eq!(ask(&mut server, "ping", &json!({}))["result"], json!({}));
}

/// Every tool is listed with a description and a schema of its arguments.
#[test]
fn every_tool_is_listed() {
    let mut server = server(&Fake::new());
    let reply = ask(&mut server, "tools/list", &json!({}));
    let tools = reply["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), TOOLS.len());
    let mut names = std::collections::BTreeSet::new();
    for t in tools {
        let name = t["name"].as_str().unwrap();
        assert!(names.insert(name.to_owned()), "{name} twice");
        assert!(!t["description"].as_str().unwrap().is_empty(), "{name}");
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}");
        for needed in schema["required"].as_array().unwrap() {
            let needed = needed.as_str().unwrap();
            assert!(
                schema["properties"].get(needed).is_some(),
                "{name}: {needed}"
            );
        }
    }
    for wanted in [
        "get_state",
        "list_monitors",
        "add_monitor",
        "remove_monitor",
        "place_monitor",
        "set_scale",
        "show_pc_display",
        "recenter",
        "no_limits",
        "set_quality",
        "set_codec",
        "pair_new_headset",
        "stop_pairing",
        "screenshot_monitor",
        "wait_for_state",
        "start",
    ] {
        assert!(names.contains(wanted), "{wanted}");
    }
    let quit = tools.iter().find(|t| t["name"] == "quit").unwrap();
    assert_eq!(quit["annotations"]["destructiveHint"], true);
}

/// Each tool that is a request asks for it with the tool's arguments: every request
/// it names is one the local API knows.
#[test]
fn tools_ask_for_their_requests() {
    for t in TOOLS {
        if let Does::Ask(request) = t.does {
            assert!(
                crate::api::REQUESTS.contains(&request),
                "{}: {request}",
                t.name
            );
        }
    }
    let fake = Fake::new();
    let mut server = server(&fake);
    let (failed, content) = tool(&mut server, "add_monitor", &json!({"w": 1920, "h": 1080}));
    assert!(!failed);
    assert_eq!(said(&content), json!({"revision": 7}));
    let (failed, _) = tool(
        &mut server,
        "place_monitor",
        &json!({"id": 103, "x": -10, "y": 0}),
    );
    assert!(!failed);
    tool(
        &mut server,
        "show_pc_display",
        &json!({"id": 7, "shown": false}),
    );
    tool(&mut server, "recenter", &json!({}));
    tool(
        &mut server,
        "call",
        &json!({"request": {"request": "set_sharp", "on": true}}),
    );
    assert_eq!(
        *fake.asked.borrow(),
        [
            Request::AddMonitor {
                w: 1920,
                h: 1080,
                scale: 100
            },
            Request::Place {
                id: 103,
                x: -10,
                y: 0
            },
            Request::ShowPhysical {
                id: 7,
                shown: false
            },
            Request::Recenter,
            Request::SetSharp { on: true },
        ]
    );
}

/// Arguments that do not read, a refusal, and Prostir not running are the tool's
/// errors, named as `ctl --json` names them; nothing is asked for wrong arguments.
#[test]
fn errors_of_tools() {
    let mut fake = Fake::new();
    let mut s = server(&fake);
    let (failed, content) = tool(&mut s, "cinema", &json!({}));
    assert!(failed);
    assert_eq!(said(&content)["error"], "BadArgument");
    let (failed, content) = tool(&mut s, "call", &json!({"request": {"request": "fly"}}));
    assert!(failed);
    assert_eq!(said(&content)["error"], "Unsupported");
    assert!(fake.asked.borrow().is_empty());

    fake.answer = Err(ClientError::Refused(crate::api::Failure::new(
        crate::api::ApiError::NotConnected,
        "No headset is connected",
    )));
    let mut s = server(&fake);
    let (failed, content) = tool(&mut s, "recenter", &json!({}));
    assert!(failed);
    assert_eq!(
        said(&content),
        json!({"error": "NotConnected", "text": "No headset is connected"})
    );

    fake.answer = Err(ClientError::NotRunning);
    let mut s = server(&fake);
    let (failed, content) = tool(&mut s, "add_monitor", &json!({"w": 640, "h": 360}));
    assert!(failed);
    assert_eq!(said(&content)["error"], "NotRunning");

    let start = Box::new(|| Ok(String::new()));
    let mut no_bus = Server::new(Err(ClientError::NoBus("none".into())), start, None);
    let (failed, content) = tool(&mut no_bus, "get_state", &json!({}));
    assert!(failed);
    assert_eq!(said(&content)["error"], "NoBus");
    let (failed, content) = tool(&mut no_bus, "start", &json!({}));
    assert!(!failed);
    assert_eq!(said(&content)["started"], true);
}

/// What is not MCP gets JSON-RPC's errors; notifications and responses get nothing;
/// a batch an answer for each of its requests.
#[test]
fn the_protocol_around_the_tools() {
    let mut s = server(&Fake::new());
    let code = |reply: Option<String>| -> i64 {
        let reply: Value = serde_json::from_str(&reply.expect("an answer")).unwrap();
        reply["error"]["code"].as_i64().unwrap()
    };
    assert_eq!(code(s.handle_line("{not json")), PARSE_ERROR);
    assert_eq!(code(s.handle_line("[]")), INVALID_REQUEST);
    assert_eq!(code(s.handle_line("7")), INVALID_REQUEST);
    assert_eq!(
        code(s.handle_line(r#"{"id":1,"method":"ping"}"#)),
        INVALID_REQUEST
    );
    assert_eq!(
        code(s.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#)),
        METHOD_NOT_FOUND
    );
    assert_eq!(
        code(s.handle_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fly"}}"#
        )),
        INVALID_PARAMS
    );
    assert_eq!(
        s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        None
    );
    assert_eq!(
        s.handle_line(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#),
        None
    );
    let batch = s
        .handle_line(
            r#"[{"jsonrpc":"2.0","id":"a","method":"ping"},
                {"jsonrpc":"2.0","method":"notifications/initialized"},
                {"jsonrpc":"2.0","id":"b","method":"ping"}]"#,
        )
        .unwrap();
    let batch: Value = serde_json::from_str(&batch).unwrap();
    let ids: Vec<&Value> = batch.as_array().unwrap().iter().map(|r| &r["id"]).collect();
    assert_eq!(ids, [&json!("a"), &json!("b")]);
}

/// `serve` answers a line for each request and nothing else on stdout.
#[test]
fn serve_answers_line_by_line() {
    let mut s = server(&Fake::new());
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_state"}}"#,
        "\n",
    );
    let mut out = Vec::new();
    s.serve(&mut input.as_bytes(), &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-03-26");
    let content = lines[1]["result"]["content"].as_array().unwrap();
    let got = said(content);
    assert_eq!(got["revision"], 1);
    assert_eq!(got["state"]["primary"], 103);
}

/// The conditions `wait_for_state` reads, and when each holds.
#[test]
fn conditions() {
    let read = |v: Value| Condition::read(v.as_object().unwrap());
    let c =
        read(json!({"pointer": "/displays", "contains": {"id": 103, "state": "running"}})).unwrap();
    let displays: Value =
        serde_json::from_str::<Value>(&state("running")).unwrap()["displays"].clone();
    assert!(c.holds(Some(&displays), false));
    let starting: Value =
        serde_json::from_str::<Value>(&state("starting")).unwrap()["displays"].clone();
    assert!(!c.holds(Some(&starting), true));
    assert!(!c.holds(None, true));
    let equals = read(json!({"pointer": "/primary", "equals": 103})).unwrap();
    assert!(equals.holds(Some(&json!(103)), false));
    assert!(!equals.holds(Some(&json!(104)), false));
    let gone = read(json!({"pointer": "/pairing", "exists": false})).unwrap();
    assert!(gone.holds(None, false));
    assert!(!gone.holds(Some(&json!({})), false));
    let any = read(json!({})).unwrap();
    assert_eq!(any.test, Test::Changed);
    assert!(!any.holds(None, false) && any.holds(None, true));
    assert!(read(json!({"pointer": "displays"})).is_err());
    assert!(read(json!({"equals": 1, "exists": true})).is_err());
    assert!(read(json!({"exists": "yes"})).is_err());
    assert!(contains(
        &json!("The desktop does not respond"),
        &json!("respond")
    ));
    assert!(contains(&json!([[1, 2], [3]]), &json!([3])));
    assert!(!contains(&json!({"a": 1}), &json!({"a": 1, "b": 2})));
}

/// `wait_for_state` looks until the condition holds, answering the value; in time
/// or not, and not at all while Prostir is not running.
#[test]
fn waiting_for_the_state() {
    let fake = Fake::new();
    fake.states
        .borrow_mut()
        .extend([(2, state("starting")), (3, state("running"))]);
    let mut s = server(&fake);
    let running = json!({"pointer": "/displays/1/state", "equals": "running", "timeout": 5});
    let (failed, content) = tool(&mut s, "wait_for_state", &running);
    assert!(!failed, "{content:?}");
    let got = said(&content);
    assert_eq!(
        (got["revision"].as_u64(), &got["value"]),
        (Some(3), &json!("running"))
    );

    let failed_state = json!({"pointer": "/displays/1/state", "equals": "failed", "timeout": 0.05});
    let (failed, content) = tool(&mut s, "wait_for_state", &failed_state);
    assert!(failed);
    let got = said(&content);
    assert_eq!(got["error"], "TimedOut");
    assert_eq!(got["value"], "running");

    let (failed, content) = tool(&mut s, "wait_for_state", &json!({"timeout": 500}));
    assert!(failed);
    assert_eq!(said(&content)["error"], "BadArgument");

    fake.states.borrow_mut().clear();
    let (failed, content) = tool(&mut s, "wait_for_state", &running);
    assert!(failed);
    assert_eq!(said(&content)["error"], "NotRunning");
}

/// A screenshot comes as a PNG image read from the file Prostir saved, which goes
/// unless kept.
#[test]
fn a_screenshot_is_an_image() {
    let path = std::env::temp_dir().join(format!("prostir-mcp-test-{}.png", std::process::id()));
    let png = b"\x89PNG\r\n\x1a\nnot really";
    let mut fake = Fake::new();
    fake.answer = Ok(Answer {
        path: Some(path.to_string_lossy().into_owned()),
        whole: Some(true),
        revision: 8,
        ..Answer::default()
    });
    let mut s = server(&fake);
    for keep in [true, false] {
        std::fs::write(&path, png).unwrap();
        let (failed, content) = tool(
            &mut s,
            "screenshot_monitor",
            &json!({"id": 103, "keep": keep}),
        );
        assert!(!failed, "{content:?}");
        let got = said(&content);
        assert_eq!(got["whole"], true);
        assert_eq!(got["bytes"], png.len());
        assert_eq!(got["path"].is_string(), keep);
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["mimeType"], "image/png");
        assert_eq!(content[1]["data"], base64(png));
        assert_eq!(path.exists(), keep);
    }
    assert_eq!(fake.asked.borrow()[0], Request::Screenshot { id: 103 });
    let (failed, content) = tool(&mut s, "screenshot_monitor", &json!({"id": 103}));
    assert!(failed, "no file any more");
    assert_eq!(said(&content)["error"], "Capture");
    let (failed, content) = tool(&mut s, "screenshot_monitor", &json!({"id": "x"}));
    assert!(failed);
    assert_eq!(said(&content)["error"], "BadArgument");
}

#[test]
fn base64_as_the_rfc_has_it() {
    for (bytes, encoded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(bytes.as_bytes()), encoded, "{bytes}");
    }
    assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
}
