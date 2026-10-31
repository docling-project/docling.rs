//! `docling-rs mcp` (#669) end to end: the real binary, spoken to the way an
//! MCP client does — newline-delimited JSON-RPC on stdio, and Streamable HTTP
//! for `--http`. Only declarative fixtures, so no models are needed.
#![cfg(feature = "mcp")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

fn fixtures() -> PathBuf {
    std::fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/data"))
        .unwrap()
}

/// The `file:` URL of a local path: `file:///home/…`, or `file:///C:/…` for
/// a canonical Windows path (its `\\?\` prefix dropped, `\` as `/`) — no
/// character in the fixture paths needs percent-encoding.
fn file_uri(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    let path = path
        .strip_prefix(r"\\?\")
        .unwrap_or(&path)
        .replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

/// A stdio session: the child plus its two pipe ends.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    /// Start `docling-rs mcp ARGS…` and complete the initialize handshake.
    fn start(args: &[&str]) -> (Self, Value) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
            .arg("mcp")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn docling-rs mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "docling-cli-test", "version": "0"}
            }),
        );
        session.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        (session, init)
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a request and return its `result` (notifications in between are
    /// skipped).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "server closed stdout"
            );
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout carried a non-JSON-RPC line ({e}): {line:?}"));
            if message["id"] == json!(id) {
                assert!(message.get("error").is_none(), "{method}: {message}");
                return message["result"].clone();
            }
        }
    }

    /// `tools/call` → (text, isError).
    fn call(&mut self, tool: &str, arguments: Value) -> (String, bool) {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (text, result["isError"] == json!(true))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_lists_the_three_tools() {
    let root = fixtures();
    let (mut s, init) = Session::start(&["--root", root.to_str().unwrap()]);
    assert_eq!(init["serverInfo"]["name"], "docling-rs");
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    let tools = s.request("tools/list", json!({}));
    let mut names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["convert", "convert_to_markdown", "list_input_formats"]
    );
    // MarkItDown's contract: one required string argument named `uri`.
    let tool = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "convert_to_markdown")
        .unwrap();
    assert_eq!(tool["inputSchema"]["required"], json!(["uri"]));
    assert_eq!(tool["inputSchema"]["properties"]["uri"]["type"], "string");
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
}

#[test]
fn stdio_converts_a_docx_fixture() {
    let root = fixtures();
    let (mut s, _) = Session::start(&["--root", root.to_str().unwrap()]);
    // A path relative to the root…
    let (md, is_error) = s.call(
        "convert_to_markdown",
        json!({"uri": "docx/sources/lorem_ipsum.docx"}),
    );
    assert!(!is_error, "{md}");
    assert!(md.contains("Lorem ipsum dolor sit amet"), "{md}");
    // …a file: URI…
    let uri = file_uri(&root.join("docx/sources/lorem_ipsum.docx"));
    let (md2, is_error) = s.call("convert_to_markdown", json!({"uri": uri}));
    assert!(!is_error, "{md2}");
    assert_eq!(md, md2);
    // …a data: URI, and `convert` with another output format.
    let (md, is_error) = s.call(
        "convert_to_markdown",
        json!({"uri": "data:text/html;base64,PGgxPkhlbGxvPC9oMT48cD5Xb3JsZDwvcD4="}),
    );
    assert!(!is_error, "{md}");
    assert_eq!(md.trim(), "# Hello\n\nWorld");
    let (json_out, is_error) = s.call(
        "convert",
        json!({"uri": "docx/sources/lorem_ipsum.docx", "to": "json"}),
    );
    assert!(!is_error, "{json_out}");
    let doc: Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(doc["schema_name"], "DoclingDocument");
}

#[test]
fn stdio_refuses_what_the_operator_did_not_allow() {
    let root = fixtures().join("md");
    let (mut s, _) = Session::start(&["--root", root.to_str().unwrap()]);
    // An absolute path and its `file:` URL outside the root, in the
    // spellings the host resolves: `file:///etc/passwd` names no Windows
    // path at all and is refused earlier, with another message.
    let outside: &[&str] = if cfg!(windows) {
        &[
            "/etc/passwd",
            r"C:\Windows\win.ini",
            "file:///C:/Windows/win.ini",
        ]
    } else {
        &["/etc/passwd", "file:///etc/passwd"]
    };
    let mut uris = vec!["../docx/sources/lorem_ipsum.docx"];
    uris.extend_from_slice(outside);
    for uri in uris {
        let (text, is_error) = s.call("convert_to_markdown", json!({"uri": uri}));
        assert!(is_error, "{uri}: {text}");
        assert!(text.contains("outside the server's root"), "{uri}: {text}");
    }
    let (text, is_error) = s.call(
        "convert_to_markdown",
        json!({"uri": "https://example.com/report.pdf"}),
    );
    assert!(is_error && text.contains("--allow-url-fetch"), "{text}");
    let (text, is_error) = s.call(
        "convert",
        json!({"uri": "x.md", "options": {"not_an_option": 1}}),
    );
    assert!(is_error && text.contains("unknown field"), "{text}");
    // A form's text values read as the option's type (MCP Inspector sends
    // `"true"`); a value no reading accepts names the option.
    let (text, is_error) = s.call(
        "convert",
        json!({"uri": "sources/duck.md", "options": {"strict": "true", "compact_tables": "1"}}),
    );
    assert!(!is_error, "{text}");
    let (text, is_error) = s.call(
        "convert",
        json!({"uri": "sources/duck.md", "options": {"strict": "maybe"}}),
    );
    assert!(is_error && text.contains("strict: cannot read"), "{text}");
    // A tool error leaves the session usable.
    let (formats, is_error) = s.call("list_input_formats", json!({}));
    assert!(
        !is_error && formats.lines().any(|l| l == "docx"),
        "{formats}"
    );
}

#[test]
fn http_refuses_a_public_bind_without_allow_remote() {
    let out = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .args(["mcp", "--http", "--host", "0.0.0.0", "--port", "0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--allow-remote"), "{stderr}");
    // An HTTP flag on a stdio launch is a usage error, not silently ignored.
    let out = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .args(["mcp", "--bearer-token", "x"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("applies to --http only"), "{stderr}");
}

/// One HTTP/1.1 POST; the status code and the body.
fn post(port: u16, headers: &[(&str, &str)], body: &Value) -> (u16, String) {
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, response)
}

#[test]
fn http_serves_with_a_bearer_token() {
    let root = fixtures();
    let mut child = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .args(["mcp", "--http", "--port", "0", "--bearer-token", "t0ken"])
        .args(["--root", root.to_str().unwrap()])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The startup line names the bound address (`--port 0` picks one).
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port: u16 = line
        .split("http://127.0.0.1:")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no address in {line:?}"));
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "t", "version": "0"}}
    });
    let (status, _) = post(port, &[], &init);
    assert_eq!(status, 401);
    let (status, _) = post(port, &[("Authorization", "Bearer wrong")], &init);
    assert_eq!(status, 401);
    // A web page's Origin is refused on a loopback bind (DNS rebinding /
    // drive-by requests from the user's browser).
    let (status, _) = post(
        port,
        &[
            ("Authorization", "Bearer t0ken"),
            ("Origin", "https://evil.example"),
        ],
        &init,
    );
    assert_eq!(status, 403);
    let (status, body) = post(port, &[("Authorization", "Bearer t0ken")], &init);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"name\":\"docling-rs\""), "{body}");
    let _ = child.kill();
    let _ = child.wait();
}

/// Started in `/` (where desktop clients often start servers) without
/// `--root`, the server refuses rather than open the whole file system; an
/// explicit `--root /` is the operator's choice and starts.
#[cfg(unix)]
#[test]
fn a_defaulted_file_system_root_is_refused() {
    let out = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .arg("mcp")
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("file system root"), "{stderr}");
    let out = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .args(["mcp", "--root=/"])
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // stdin closed at once: the server starts, sees no client and exits.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("file system root"), "{stderr}");
    assert!(stderr.contains("root /"), "{stderr}");
}
