//! `prostir mcp`: the local API as tools of the Model Context Protocol, so an agent
//! (Claude Code, say) drives the running Prostir and looks at what came of it: the
//! state, the monitors, their arrangement, a picture of a monitor, the commands for
//! the headset. A thin wrapper: each tool is one request of the local API (see
//! `crate::api` and `LOCAL-API.md`) through a [`CtlClient`], as `prostir ctl` makes
//! them, with the same answers and errors.
//!
//! MCP runs over stdio: JSON-RPC 2.0, one message a line. Only the protocol's
//! messages go to stdout; the log goes to stderr. Requests are handled one after the
//! other; a tool that waits (a command for the headset, `wait_for_state`) holds the
//! next ones meanwhile. Nothing starts Prostir but the tool `start`.

use std::io::{BufRead, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::api::{Answer, Request};
use crate::ctl::{self, ClientError, CtlClient, Watched};

/// The versions of MCP this server speaks, the newest first.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// JSON-RPC's error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// How long `wait_for_state` waits by default, and at most.
const WAIT_DEFAULT: f64 = 10.0;
const WAIT_MAX: f64 = 120.0;

const INSTRUCTIONS: &str = "\
Prostir streams virtual monitors of this PC (KDE Plasma, Hyprland) to a Meta Quest \
headset. These tools are the local API of the Prostir running for this user: each one \
is one request, answered with the revision of the state that already shows the change \
(LOCAL-API.md describes every field). Look at the result with get_state, \
list_monitors, wait_for_state and screenshot_monitor. Prostir runs only while \
something needs it (a headset, monitors waiting for one, its settings window): the \
tools then answer NotRunning, and only the start tool starts it. Commands for the \
headset need a connected headset (NotConnected otherwise) and wait up to 30 s for it.";

/// Counts the state's changes the bus told of, so a wait wakes on each.
#[derive(Default)]
pub struct Changes {
    seen: Mutex<u64>,
    told: Condvar,
}

impl Changes {
    /// Something changed.
    pub fn bump(&self) {
        *self.seen.lock().unwrap() += 1;
        self.told.notify_all();
    }

    fn count(&self) -> u64 {
        *self.seen.lock().unwrap()
    }

    /// Sleeps until a change after `since`, `timeout` at most.
    fn wait(&self, since: u64, timeout: Duration) {
        let seen = self.seen.lock().unwrap();
        let _ = self.told.wait_timeout_while(seen, timeout, |n| *n == since);
    }
}

/// Starts following the state's changes into the given [`Changes`].
pub type Follow = Box<dyn FnOnce(Arc<Changes>)>;

/// The MCP server: the way to Prostir, and what a session of the protocol keeps.
pub struct Server {
    client: Result<Box<dyn CtlClient>, ClientError>,
    /// Starts Prostir (the tool `start`): what to tell the agent, or why it did not.
    start: Box<dyn Fn() -> Result<String, String>>,
    /// Starts following the state's changes, once a wait needs it.
    follow: Option<Follow>,
    changes: Option<Arc<Changes>>,
    /// How often a wait looks at the state itself, besides waking on each change.
    poll: Duration,
}

impl Server {
    pub fn new(
        client: Result<Box<dyn CtlClient>, ClientError>,
        start: Box<dyn Fn() -> Result<String, String>>,
        follow: Option<Follow>,
    ) -> Self {
        let poll = if follow.is_some() {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(200)
        };
        Self {
            client,
            start,
            follow,
            changes: None,
            poll,
        }
    }

    /// Reads messages from `input` until it ends, answering each to `output`.
    pub fn serve(
        &mut self,
        input: &mut dyn BufRead,
        output: &mut dyn Write,
    ) -> std::io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_line(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
    }

    /// One line of input: the line to answer with, if any (not for notifications).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(e) => {
                return Some(
                    failure(&Value::Null, PARSE_ERROR, &format!("not JSON: {e}")).to_string(),
                );
            }
        };
        match message {
            // A batch (MCP up to 2025-03-26): an answer for each request in it.
            Value::Array(batch) if batch.is_empty() => {
                Some(failure(&Value::Null, INVALID_REQUEST, "an empty batch").to_string())
            }
            Value::Array(batch) => {
                let replies: Vec<Value> =
                    batch.into_iter().filter_map(|m| self.handle(m)).collect();
                (!replies.is_empty()).then(|| Value::Array(replies).to_string())
            }
            message => self.handle(message).map(|reply| reply.to_string()),
        }
    }

    /// One message: its answer, `None` for a notification or a response.
    fn handle(&mut self, message: Value) -> Option<Value> {
        let Value::Object(mut message) = message else {
            return Some(failure(&Value::Null, INVALID_REQUEST, "not an object"));
        };
        let id = message.remove("id");
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(failure(
                &id.unwrap_or_default(),
                INVALID_REQUEST,
                "not JSON-RPC 2.0",
            ));
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let params = message.remove("params").unwrap_or_else(|| json!({}));
        match (method, id) {
            (Some(method), Some(id)) if id.is_string() || id.is_number() => {
                Some(match self.method(&method, &params) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, text)) => failure(&id, code, &text),
                })
            }
            (Some(_), Some(id)) => Some(failure(
                &id,
                INVALID_REQUEST,
                "an id is a string or a number",
            )),
            // Notifications (`notifications/initialized`, `notifications/cancelled`):
            // nothing to answer, nothing to do.
            (Some(method), None) => {
                log::debug!("mcp: {method}");
                None
            }
            // A response to a request of ours: this server sends none.
            (None, Some(_)) if message.contains_key("result") || message.contains_key("error") => {
                None
            }
            (None, id) => Some(failure(
                &id.unwrap_or_default(),
                INVALID_REQUEST,
                "no method",
            )),
        }
    }

    fn method(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str);
                let version = asked
                    .filter(|v| VERSIONS.contains(v))
                    .unwrap_or(VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "prostir",
                        "title": "Prostir",
                        "version": prostir_proto::app_version(env!("CARGO_PKG_VERSION")),
                    },
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                Ok(json!({"tools": TOOLS.iter().map(Tool::listed).collect::<Vec<_>>()}))
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
                    return Err((INVALID_PARAMS, format!("no tool {name:?}")));
                };
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(args)) => args.clone(),
                    Some(_) => return Err((INVALID_PARAMS, "arguments are an object".into())),
                };
                Ok(match self.call(tool, &args) {
                    Ok(content) => json!({"content": content}),
                    Err(e) => json!({"content": [text(&e.to_string())], "isError": true}),
                })
            }
            _ => Err((METHOD_NOT_FOUND, format!("no method {method:?}"))),
        }
    }

    fn client(&self) -> Result<&dyn CtlClient, Value> {
        self.client.as_deref().map_err(|e| refused(e, false))
    }

    /// Carries out `tool` with `args`: what it gives, or the error as JSON.
    fn call(&mut self, tool: &Tool, args: &Map<String, Value>) -> Result<Vec<Value>, Value> {
        match tool.does {
            Does::Ask(request) => {
                let mut asked = args.clone();
                asked.insert("request".into(), request.into());
                let request = Request::from_json(&Value::Object(asked).to_string())
                    .map_err(|f| json!({"error": f.error.name(), "text": f.text}))?;
                let answer = self.ask(&request)?;
                Ok(vec![text(&answer.to_json())])
            }
            Does::Call => {
                let Some(request) = args.get("request").filter(|r| r.is_object()) else {
                    return Err(
                        json!({"error": "BadArgument", "text": "request: a request object"}),
                    );
                };
                let request = Request::from_json(&request.to_string())
                    .map_err(|f| json!({"error": f.error.name(), "text": f.text}))?;
                let answer = self.ask(&request)?;
                Ok(vec![text(&answer.to_json())])
            }
            Does::State => {
                let (revision, state) = self.state()?;
                Ok(vec![text(
                    &json!({"revision": revision, "state": state}).to_string(),
                )])
            }
            Does::Monitors => {
                let (revision, state) = self.state()?;
                let shown = json!({
                    "revision": revision,
                    "primary": state.get("primary"),
                    "displays": state.get("displays"),
                });
                Ok(vec![text(&shown.to_string())])
            }
            Does::Screenshot => self.screenshot(args),
            Does::Wait => self.wait(args),
            Does::Start => match (self.start)() {
                Ok(said) => Ok(vec![text(
                    &json!({"started": true, "text": said}).to_string(),
                )]),
                Err(why) => Err(json!({"error": "NotRunning", "text": why})),
            },
        }
    }

    fn ask(&self, request: &Request) -> Result<Answer, Value> {
        self.client()?
            .call(request)
            .map_err(|e| refused(&e, request.for_headset()))
    }

    /// The state now: its revision and its JSON.
    fn state(&self) -> Result<(u32, Value), Value> {
        let (revision, text) = self.client()?.state().map_err(|e| refused(&e, false))?;
        let state = serde_json::from_str(&text).map_err(
            |e| json!({"error": "Other", "text": format!("the state does not read: {e}")}),
        )?;
        Ok((revision, state))
    }

    /// `screenshot_monitor`: the picture, read from the file Prostir saved it to
    /// (removed afterwards, unless `keep`).
    fn screenshot(&self, args: &Map<String, Value>) -> Result<Vec<Value>, Value> {
        let bad = |text: &str| json!({"error": "BadArgument", "text": text});
        let id = args
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| bad("id: a display's id"))?;
        let keep = match args.get("keep") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(keep)) => *keep,
            Some(_) => return Err(bad("keep: true or false")),
        };
        let answer = self.ask(&Request::Screenshot { id })?;
        let Some(path) = answer.path.clone() else {
            return Err(json!({"error": "Other", "text": "the answer names no file"}));
        };
        let png = std::fs::read(&path)
            .map_err(|e| json!({"error": "Capture", "text": format!("{path}: {e}")}))?;
        if !keep && let Err(e) = std::fs::remove_file(&path) {
            log::warn!("mcp: cannot remove {path}: {e}");
        }
        let said = json!({
            "id": id,
            "whole": answer.whole.unwrap_or(false),
            "path": keep.then_some(path),
            "bytes": png.len(),
            "revision": answer.revision,
        });
        Ok(vec![
            text(&said.to_string()),
            json!({"type": "image", "data": base64(&png), "mimeType": "image/png"}),
        ])
    }

    /// `wait_for_state`: until the condition holds in the state, or the time is up.
    fn wait(&mut self, args: &Map<String, Value>) -> Result<Vec<Value>, Value> {
        let condition =
            Condition::read(args).map_err(|text| json!({"error": "BadArgument", "text": text}))?;
        let timeout = match args.get("timeout") {
            None | Some(Value::Null) => WAIT_DEFAULT,
            Some(t) => t
                .as_f64()
                .filter(|t| (0.0..=WAIT_MAX).contains(t))
                .ok_or_else(
                    || json!({"error": "BadArgument", "text": "timeout: 0 to 120 seconds"}),
                )?,
        };
        let client = self.client()?;
        // Not running now: said at once, as every other tool says it.
        if !client.running() {
            return Err(refused(&ClientError::NotRunning, false));
        }
        if self.changes.is_none()
            && let Some(follow) = self.follow.take()
        {
            let changes = Arc::new(Changes::default());
            follow(changes.clone());
            self.changes = Some(changes);
        }
        let client = self.client()?;
        let started = Instant::now();
        let until = started + Duration::from_secs_f64(timeout);
        let mut first = None;
        let mut last: (Option<u32>, Option<Value>) = (None, None);
        loop {
            let seen = self.changes.as_ref().map(|c| c.count());
            match client.state() {
                Ok((revision, json)) => {
                    let state: Value = serde_json::from_str(&json).unwrap_or_default();
                    let value = state.pointer(&condition.pointer).cloned();
                    let first = *first.get_or_insert(revision);
                    if condition.holds(value.as_ref(), revision != first) {
                        let waited = started.elapsed().as_millis() as u64;
                        let done =
                            json!({"revision": revision, "value": value, "waited_ms": waited});
                        return Ok(vec![text(&done.to_string())]);
                    }
                    last = (Some(revision), value);
                }
                // Gone meanwhile (restarting, say): looked at again until the time is up.
                Err(ClientError::NotRunning) => last.0 = None,
                Err(e) => return Err(refused(&e, false)),
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let why = if last.0.is_none() {
                    "Prostir is not running"
                } else {
                    "the condition did not hold in time"
                };
                return Err(json!({
                    "error": "TimedOut",
                    "text": format!("{why} (waited {timeout} s)"),
                    "revision": last.0,
                    "value": last.1,
                }));
            }
            let pause = left.min(self.poll);
            match (&self.changes, seen) {
                (Some(changes), Some(seen)) => changes.wait(seen, pause),
                _ => std::thread::sleep(pause),
            }
        }
    }
}

/// A JSON-RPC error answer.
fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Text content of a tool's result.
fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// The error of a tool as `ctl --json` says it: `{"error": NAME, "text": ...}`.
fn refused(e: &ClientError, for_headset: bool) -> Value {
    let (_, name, text) = ctl::what_failed(e, for_headset);
    json!({"error": name, "text": text})
}

/// What `wait_for_state` waits for: the value at a JSON Pointer.
#[derive(Debug, PartialEq)]
struct Condition {
    pointer: String,
    test: Test,
}

#[derive(Debug, PartialEq)]
enum Test {
    Equals(Value),
    Contains(Value),
    Exists(bool),
    /// Any change of the state.
    Changed,
}

impl Condition {
    fn read(args: &Map<String, Value>) -> Result<Self, String> {
        let pointer = match args.get("pointer") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(p)) if p.is_empty() || p.starts_with('/') => p.clone(),
            Some(_) => return Err("pointer: a JSON Pointer, \"\" or starting with /".into()),
        };
        let mut tests = Vec::new();
        if let Some(v) = args.get("equals") {
            tests.push(Test::Equals(v.clone()));
        }
        if let Some(v) = args.get("contains") {
            tests.push(Test::Contains(v.clone()));
        }
        match args.get("exists") {
            None => {}
            Some(Value::Bool(b)) => tests.push(Test::Exists(*b)),
            Some(_) => return Err("exists: true or false".into()),
        }
        if tests.len() > 1 {
            return Err("one of equals, contains and exists at most".into());
        }
        Ok(Self {
            pointer,
            test: tests.pop().unwrap_or(Test::Changed),
        })
    }

    /// Whether it holds for `value` (`None`: nothing there); `changed`: the state is
    /// not the one the wait began with.
    fn holds(&self, value: Option<&Value>, changed: bool) -> bool {
        match &self.test {
            Test::Equals(wanted) => value == Some(wanted),
            Test::Contains(wanted) => value.is_some_and(|v| contains(v, wanted)),
            Test::Exists(wanted) => value.is_some() == *wanted,
            Test::Changed => changed,
        }
    }
}

/// Whether `value` has `wanted` in it: an array an element that has it; an object its
/// fields (each having what `wanted`'s has); a string the text; else the same value.
fn contains(value: &Value, wanted: &Value) -> bool {
    match (value, wanted) {
        (Value::Array(items), Value::Array(all)) => all
            .iter()
            .all(|w| items.iter().any(|item| contains(item, w))),
        (Value::Array(items), _) => items.iter().any(|item| contains(item, wanted)),
        (Value::Object(fields), Value::Object(wanted)) => wanted
            .iter()
            .all(|(k, w)| fields.get(k).is_some_and(|v| contains(v, w))),
        (Value::String(text), Value::String(part)) => text.contains(part.as_str()),
        _ => value == wanted,
    }
}

/// Standard base64 with padding, as MCP carries an image.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ABC[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// What a tool does.
#[derive(Debug, Clone, Copy)]
enum Does {
    /// The request of this name, its arguments the tool's.
    Ask(&'static str),
    /// Any request, given whole.
    Call,
    State,
    Monitors,
    Screenshot,
    Wait,
    Start,
}

/// How a tool touches the PC, for the client to decide what to ask the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Touch {
    Reads,
    Changes,
    Destroys,
}

struct Tool {
    name: &'static str,
    description: &'static str,
    /// Its arguments: name, JSON type, whether needed, what it is.
    args: &'static [(&'static str, &'static str, bool, &'static str)],
    does: Does,
    touch: Touch,
}

impl Tool {
    /// As `tools/list` gives it.
    fn listed(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for (name, kind, needed, about) in self.args {
            let mut schema = json!({"description": about});
            if !kind.is_empty() {
                schema["type"] = (*kind).into();
            }
            properties.insert((*name).into(), schema);
            if *needed {
                required.push(*name);
            }
        }
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": {"type": "object", "properties": properties, "required": required},
            "annotations": {
                "readOnlyHint": self.touch == Touch::Reads,
                "destructiveHint": self.touch == Touch::Destroys,
                "idempotentHint": self.touch == Touch::Reads,
                "openWorldHint": false,
            },
        })
    }
}

const ID: (&str, &str, bool, &str) = (
    "id",
    "integer",
    true,
    "the display's id (displays[].id in the state)",
);
const ON: (&str, &str, bool, &str) = ("on", "boolean", true, "on (true) or off");

/// A command for the headset without arguments.
const fn command(name: &'static str, description: &'static str) -> Tool {
    Tool {
        name,
        description,
        args: &[],
        does: Does::Ask(name),
        touch: Touch::Changes,
    }
}

/// A tool that is the request of the same name.
const fn asks(
    name: &'static str,
    description: &'static str,
    args: &'static [(&'static str, &'static str, bool, &'static str)],
) -> Tool {
    Tool {
        name,
        description,
        args,
        does: Does::Ask(name),
        touch: Touch::Changes,
    }
}

static TOOLS: &[Tool] = &[
    Tool {
        name: "get_state",
        description: "The whole state of Prostir as JSON with its revision: the PC, paired and \
                      connected headsets, displays (virtual monitors, the PC's own displays, window \
                      panels) with their state and place on the desktop, limits, encoding, sound, \
                      pairing, problems. LOCAL-API.md describes every field. It includes the \
                      pairing code while one is shown.",
        args: &[],
        does: Does::State,
        touch: Touch::Reads,
    },
    Tool {
        name: "list_monitors",
        description: "The displays (id, kind virtual/physical/window, name, output, mode with \
                      w, h and scale, desk rectangle in logical pixels, state \
                      starting/running/failed/absent/kept, failure) and the primary display's id.",
        args: &[],
        does: Does::Monitors,
        touch: Touch::Reads,
    },
    Tool {
        name: "add_monitor",
        description: "Adds a virtual monitor of w x h pixels at a scale in percent; answers its id. \
                      It stands starting until it runs (running), failed if it could not start. \
                      With no headset connected and no monitors on the desktop it joins the set \
                      kept for the next headset (kept). Wait for it with wait_for_state.",
        args: &[
            ("w", "integer", true, "width in pixels"),
            ("h", "integer", true, "height in pixels"),
            (
                "scale",
                "integer",
                false,
                "scale in percent, 100 by default (limits.scales lists them)",
            ),
        ],
        does: Does::Ask("add_monitor"),
        touch: Touch::Changes,
    },
    Tool {
        name: "remove_monitor",
        description: "Removes a virtual monitor (LastDisplay for the last one while a headset is connected).",
        args: &[ID],
        does: Does::Ask("remove_monitor"),
        touch: Touch::Destroys,
    },
    asks("retry_monitor", "Tries a failed monitor again now.", &[ID]),
    asks(
        "set_primary",
        "Makes a display the desktop's primary one.",
        &[ID],
    ),
    Tool {
        name: "place_monitor",
        description: "Moves a display on the desktop to about x, y (its top-left corner, logical \
                      pixels): into the row it is then in, or a row of its own above or under the \
                      others; the desktop's arrangement lines it up. Only while the monitors are on \
                      the desktop.",
        args: &[
            ID,
            ("x", "integer", true, "left edge, logical pixels"),
            ("y", "integer", true, "top edge, logical pixels"),
        ],
        does: Does::Ask("place"),
        touch: Touch::Changes,
    },
    asks(
        "set_scale",
        "Another scale for a monitor, in percent: its output is made anew with the same id \
         (starting meanwhile).",
        &[ID, ("scale", "integer", true, "scale in percent")],
    ),
    Tool {
        name: "show_pc_display",
        description: "Shows one of the PC's own displays (kind physical) in the headset, or hides it.",
        args: &[ID, ("shown", "boolean", true, "shown in the headset")],
        does: Does::Ask("show_physical"),
        touch: Touch::Changes,
    },
    command(
        "recenter",
        "Headset command: puts the monitors in front of the person again.",
    ),
    command(
        "line_up",
        "Headset command: lines the monitors up in front of the person.",
    ),
    command("show_mouse", "Headset command: shows where the mouse is."),
    command(
        "reset_arrangement",
        "Headset command: the monitors' arrangement as it first was.",
    ),
    asks(
        "profile",
        "Headset command: switches to the headset's profile of this name.",
        &[("name", "string", true, "the profile's name")],
    ),
    asks("cinema", "Headset command: cinema mode on or off.", &[ON]),
    asks("focus", "Headset command: focus mode on or off.", &[ON]),
    asks(
        "follow",
        "Headset command: the monitors follow the head, or stay.",
        &[ON],
    ),
    asks("no_limits", "Headset command: no limits on or off.", &[ON]),
    asks(
        "distance",
        "Headset command: the monitors closer (steps below 0) or further.",
        &[("steps", "integer", true, "-128 to 127")],
    ),
    asks(
        "preset",
        "Headset command: the headset's preset of this number.",
        &[("preset", "integer", true, "0 to 255")],
    ),
    asks(
        "set_quality",
        "The bitrate per monitor.",
        &[("mbit", "integer", true, "Mbit/s, 5 to 100")],
    ),
    asks(
        "set_sharp",
        "The slower encoder preset that keeps small text clearer.",
        &[ON],
    ),
    asks(
        "set_codec",
        "The video codec.",
        &[
            ("codec", "string", true, "h264, h265 or auto"),
            (
                "cpu",
                "boolean",
                false,
                "encode on the CPU even where the graphics card could",
            ),
        ],
    ),
    asks(
        "sound_to_headset",
        "The PC's sound plays in the headset (true) or on the PC.",
        &[ON],
    ),
    asks(
        "set_counting",
        "Counts this PC among Prostir's users, or not.",
        &[ON],
    ),
    asks(
        "autostart",
        "Starts Prostir with the session, or not (kept in settings.txt).",
        &[ON],
    ),
    asks(
        "pair_new_headset",
        "Opens the pairing window for 3 minutes; answers seconds_left. The state shows the pairing \
         code while one is shown.",
        &[],
    ),
    asks("stop_pairing", "Closes the pairing window.", &[]),
    Tool {
        name: "forget_headset",
        description: "Forgets a paired headset (it has to pair again); its session ends.",
        args: &[(
            "headset",
            "string",
            true,
            "its key, its whole key or its name",
        )],
        does: Does::Ask("forget_headset"),
        touch: Touch::Destroys,
    },
    Tool {
        name: "forget_all",
        description: "Forgets every paired headset.",
        args: &[],
        does: Does::Ask("forget_all"),
        touch: Touch::Destroys,
    },
    asks(
        "show_settings",
        "Opens Prostir's settings window on the PC.",
        &[],
    ),
    Tool {
        name: "quit",
        description: "Quits Prostir, as the tray's Quit: nothing waits for the headset afterwards.",
        args: &[],
        does: Does::Ask("quit"),
        touch: Touch::Destroys,
    },
    Tool {
        name: "call",
        description: "Any request of the local API, given whole as LOCAL-API.md writes it, for \
                      what no other tool covers: {\"request\": \"window_look\", \"theme\": \"dark\", \
                      \"palette\": \"ink\"}.",
        args: &[(
            "request",
            "object",
            true,
            "the request, its name in the field request",
        )],
        does: Does::Call,
        touch: Touch::Destroys,
    },
    Tool {
        name: "screenshot_monitor",
        description: "A picture of a virtual monitor or a window panel, as a PNG image. Where the \
                      desktop can only take all of it (KDE Plasma while the monitor does not stream \
                      to a headset) it is the whole desktop, and whole says so. The file Prostir \
                      saves to the Pictures folder is removed afterwards unless keep. The picture \
                      shows what is on that screen.",
        args: &[
            ID,
            (
                "keep",
                "boolean",
                false,
                "keep the file (its path is given)",
            ),
        ],
        does: Does::Screenshot,
        touch: Touch::Reads,
    },
    Tool {
        name: "wait_for_state",
        description: "Waits until the value at a JSON Pointer (RFC 6901) in the state holds a \
                      condition: equals (exactly that), contains (an object with these fields, \
                      an array with such an element, a string with this text) or exists. Without \
                      one, waits for any change of the state. Example: pointer /displays, \
                      contains {\"id\": 103, \"state\": \"running\"}. Answers the revision and the \
                      value; TimedOut with the last value if it did not hold in time.",
        args: &[
            (
                "pointer",
                "string",
                false,
                "a JSON Pointer into the state, \"\" (the whole state) by default",
            ),
            ("equals", "", false, "the value wanted"),
            ("contains", "", false, "what the value has in it"),
            ("exists", "boolean", false, "whether there is a value there"),
            (
                "timeout",
                "number",
                false,
                "seconds, 10 by default, 120 at most",
            ),
        ],
        does: Does::Wait,
        touch: Touch::Reads,
    },
    Tool {
        name: "start",
        description: "Starts Prostir if it is not running, as opening it from the menu does: its \
                      settings window opens too, and keeps it running while open.",
        args: &[],
        does: Does::Start,
        touch: Touch::Changes,
    },
];

/// `prostir mcp`: serves stdin and stdout until stdin ends; its exit code.
pub fn run() -> i32 {
    use crate::platform::local_api::Client;
    let client = Client::connect(ctl::WAIT).map(|c| -> Box<dyn CtlClient> { Box::new(c) });
    let start = Box::new(|| {
        let client = Client::connect(ctl::WAIT).map_err(|e| ctl::what_failed(&e, false).2)?;
        if client.running() {
            return Ok("Prostir is running already".to_owned());
        }
        match ctl::open(&client, &crate::local::wake_for_agent) {
            ctl::DONE => Ok("Prostir is running".to_owned()),
            _ => Err("Prostir did not start (its log says why)".to_owned()),
        }
    });
    let follow: Follow = Box::new(|changes: Arc<Changes>| {
        let spawned = std::thread::Builder::new()
            .name("mcp-watch".into())
            .spawn(move || {
                let Ok(client) = Client::connect(ctl::WAIT) else {
                    return;
                };
                let _ = client.watch(&mut |seen| {
                    if !matches!(seen, Watched::Event(_)) {
                        changes.bump();
                    }
                    true
                });
            });
        if let Err(e) = spawned {
            log::warn!("mcp: the state's changes are not followed: {e}");
        }
    });
    let mut server = Server::new(client, start, Some(follow));
    let (stdin, stdout) = (std::io::stdin(), std::io::stdout());
    let served = server.serve(&mut stdin.lock(), &mut stdout.lock());
    match served {
        Ok(()) => ctl::DONE,
        Err(e) => {
            eprintln!("prostir mcp: {e}");
            ctl::FAILED
        }
    }
}

#[cfg(test)]
mod tests;
