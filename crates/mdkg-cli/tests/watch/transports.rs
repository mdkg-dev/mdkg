//! The envelope cases' transports (conformance case 22): the CLI, MCP's
//! stdio server and serve-local's socket, each on its own copy of the
//! fixture. An operation is the registry's (`mdkg_ops::OPS`), with its
//! arguments by their HTTP names; each transport puts them on its own wire:
//! argv, a `tools/call`, or a request on the socket.

use std::io::Write;
use std::os::unix::net::UnixStream;

use mdkg_ops::registry::{Method, OpSpec, find};
use mdkg_wire::client_conn::ClientConnection;
use mdkg_wire::sign::Outgoing;

use super::*;

/// How long one call may take on any transport.
const CALL: Duration = Duration::from_secs(30);

fn spec(op: &str) -> &'static OpSpec {
    find(op).unwrap_or_else(|| panic!("no operation {op}"))
}

/// A scalar argument as one word.
fn word(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The envelope with the fixture's folder, as given and canonical, written
/// `<fixture>`, so copies of one fixture can be compared.
fn hide_folder(fixture: &Fixture, envelope: &Value) -> Value {
    let mut text = envelope.to_string();
    let canonical = fs::canonicalize(&fixture.dir).expect("fixture folder");
    for dir in [canonical.as_path(), fixture.dir.as_path()] {
        text = text.replace(dir.to_str().expect("UTF-8 fixture folder"), "<fixture>");
    }
    serde_json::from_str(&text).expect("envelope")
}

/// The CLI, run as `mdkg <words> --json`.
pub(crate) struct CliLocal(pub(crate) Fixture);

/// Each case operation's positionals, in the CLI's order; every other
/// argument is its `--flag` (the long name, `_` for `-`), a list repeated
/// and `true` the bare flag.
const POSITIONALS: &[(&str, &[&str])] = &[
    ("node.show", &["id"]),
    ("lease.claim", &["target"]),
    ("lease.release", &["target"]),
];

impl shared_cases::Transport for CliLocal {
    fn name(&self) -> &'static str {
        "cli-local"
    }
    fn call(&self, op: &str, actor: &str, args: &Value) -> (Value, Value) {
        let spec = spec(op);
        let positional = POSITIONALS
            .iter()
            .find(|(name, _)| *name == op)
            .map_or(&[][..], |(_, names)| names);
        let mut words: Vec<String> = spec.cli.iter().map(|w| (*w).to_string()).collect();
        for (key, value) in args.as_object().expect("arguments") {
            if positional.contains(&key.as_str()) {
                continue;
            }
            let flag = format!("--{}", key.replace('_', "-"));
            match value {
                Value::Bool(true) => words.push(flag),
                Value::Bool(false) => {}
                Value::Array(values) => {
                    for value in values {
                        words.extend([flag.clone(), word(value)]);
                    }
                }
                value => words.extend([flag, word(value)]),
            }
        }
        words.extend(
            positional
                .iter()
                .filter_map(|name| args.get(*name))
                .map(word),
        );
        let (exit, envelope) = self.0.cli(actor, &words);
        (hide_folder(&self.0, &envelope), exit.into())
    }
}

/// MCP: one `mdkg mcp` server per call, pinned to the call's actor as an
/// agent's harness pins it, called statelessly with the operation's tool.
pub(crate) struct Mcp(pub(crate) Fixture);

impl shared_cases::Transport for Mcp {
    fn name(&self) -> &'static str {
        "mcp"
    }
    fn call(&self, op: &str, actor: &str, args: &Value) -> (Value, Value) {
        let tool = spec(op)
            .mcp
            .first()
            .unwrap_or_else(|| panic!("{op} has no MCP tool"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.0
            .isolated(&mut command)
            .arg("--root")
            .arg(&self.0.repo)
            .args(["--as", actor, "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("mdkg mcp");
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        // A call in flight is answered after stdin closes.
        {
            let mut stdin = child.stdin.take().expect("stdin");
            writeln!(stdin, "{request}").expect("write the call");
        }
        let stdout = child.stdout.take().expect("stdout");
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let reply = loop {
            let line = lines.recv_timeout(CALL).expect("MCP answered the call");
            let reply: Value = serde_json::from_str(&line).expect("one JSON-RPC message");
            if reply["id"] == 1 {
                break reply;
            }
        };
        child.kill().ok();
        child.wait().ok();
        let result = &reply["result"];
        // The envelope is the last text item; a mail result may put a
        // notice about others' text before it.
        let text = result["content"]
            .as_array()
            .and_then(|c| c.last())
            .and_then(|c| c["text"].as_str())
            .unwrap_or_else(|| panic!("{tool}: no text content: {reply}"));
        let envelope: Value = serde_json::from_str(text).expect("the text is the envelope");
        (hide_folder(&self.0, &envelope), result["isError"].clone())
    }
}

/// serve-local: `mdkg serve` on the fixture, and each call one request
/// on its socket, as the request's `Mdkg-Actor`.
pub(crate) struct HttpSocket {
    pub(crate) fixture: Fixture,
    serve: Child,
    socket: PathBuf,
    graph_id: String,
}

impl HttpSocket {
    pub(crate) fn new(spec: &Value) -> Self {
        let fixture = Fixture::new(spec);
        // serve's log lines go to a file, never an unread pipe.
        let log = fs::File::create(fixture.dir.join("serve.log")).expect("serve log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        fixture
            .isolated(&mut command)
            .arg("--root")
            .arg(&fixture.repo)
            .args(["--as", "serve-test", "serve", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
        let mut serve = command.spawn().expect("mdkg serve");
        // The start envelope, printed whole once the socket listens.
        let mut stdout = BufReader::new(serve.stdout.take().expect("stdout"));
        let mut text = String::new();
        loop {
            let mut line = String::new();
            if stdout.read_line(&mut line).expect("serve's start") == 0 || line == "}\n" {
                text.push_str(&line);
                break;
            }
            text.push_str(&line);
        }
        let started: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            let log = fs::read_to_string(fixture.dir.join("serve.log")).unwrap_or_default();
            panic!("serve didn't start: {text}\n{log}")
        });
        assert_eq!(started["ok"], true, "{started}");
        Self {
            socket: PathBuf::from(started["data"]["socket"].as_str().expect("socket")),
            graph_id: spec["graph_id"].as_str().expect("graph id").to_string(),
            fixture,
            serve,
        }
    }

    /// One request: its status and JSON body.
    fn request(
        &self,
        method: &str,
        path: &str,
        query: Option<String>,
        actor: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let io = UnixStream::connect(&self.socket).expect("connect to serve");
        let mut connection = ClientConnection::new(Box::new(io));
        let mut request = Outgoing::new(method, path, query);
        request.header("Mdkg-Actor", actor).expect("actor header");
        if let Some(body) = body {
            request
                .header("Content-Type", "application/json")
                .expect("content type");
            request
                .body(body.to_string().into_bytes())
                .expect("request body");
        }
        connection.send(&request, CALL).expect("send");
        let head = connection.read_head(CALL).expect("response head");
        let body = connection
            .read_body(&head, 1 << 22, CALL)
            .expect("response body");
        let value = serde_json::from_slice(&body)
            .unwrap_or_else(|_| panic!("{path}: not JSON: {}", String::from_utf8_lossy(&body)));
        (head.status, value)
    }
}

/// `value` percent-encoded for a path segment or a query value.
fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl shared_cases::Transport for HttpSocket {
    fn name(&self) -> &'static str {
        "http-socket"
    }
    fn call(&self, op: &str, actor: &str, args: &Value) -> (Value, Value) {
        let route = spec(op).http.unwrap_or_else(|| panic!("{op} isn't served"));
        let mut given = args.as_object().expect("arguments").clone();
        let mut path = route.path.to_string();
        for name in route.names() {
            let value = match name {
                "graph_id" => self.graph_id.clone(),
                // An inbox's mailbox is the caller's own unless named, as
                // the CLI's and MCP's are.
                "mailbox" if !given.contains_key(name) => actor.to_string(),
                _ => word(
                    &given
                        .remove(name)
                        .unwrap_or_else(|| panic!("{op}: no `{name}` for the path")),
                ),
            };
            path = path.replace(&format!("{{{name}}}"), &encode(&value));
        }
        let (status, envelope) = match route.method {
            Method::Get => {
                let mut pairs = Vec::new();
                for (key, value) in &given {
                    let values = match value {
                        Value::Array(values) => values.clone(),
                        value => vec![value.clone()],
                    };
                    for value in values {
                        pairs.push(format!("{}={}", encode(key), encode(&word(&value))));
                    }
                }
                let query = (!pairs.is_empty()).then(|| pairs.join("&"));
                self.request("GET", &path, query, actor, None)
            }
            Method::Post => self.request("POST", &path, None, actor, Some(&Value::Object(given))),
        };
        (hide_folder(&self.fixture, &envelope), status.into())
    }
}

impl Drop for HttpSocket {
    fn drop(&mut self) {
        // SIGTERM drains serve and removes its socket; the fixture's
        // folder goes after it.
        if let Some(pid) = rustix::process::Pid::from_raw(self.serve.id() as i32) {
            rustix::process::kill_process(pid, rustix::process::Signal::TERM).ok();
        }
        let until = std::time::Instant::now() + Duration::from_secs(30);
        while self.serve.try_wait().ok().flatten().is_none() {
            if std::time::Instant::now() >= until {
                self.serve.kill().ok();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.serve.wait().ok();
    }
}
