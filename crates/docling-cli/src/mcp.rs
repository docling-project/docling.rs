//! `docling-rs mcp` (#669): a [Model Context Protocol](https://modelcontextprotocol.io)
//! server, so agents — Claude Desktop, Claude Code, IDE assistants — call
//! docling.rs directly.
//!
//! Three tools:
//!
//! - `convert_to_markdown(uri)` — MarkItDown's MCP tool (`markitdown-mcp`),
//!   same name and contract, so a config that pointed at MarkItDown keeps
//!   working: the document behind an `http:`/`https:`/`file:`/`data:` URI as
//!   Markdown. A plain path is accepted too.
//! - `convert(uri, to?, images?, options?)` — any output format docling.rs
//!   writes as text, with the conversion options every other surface takes
//!   ([`docling::ConvertOptions`], docs/OPTIONS.md).
//! - `list_input_formats()` — the extensions this binary converts.
//!
//! Transports: stdio (the default — what a desktop client launches) or
//! Streamable HTTP with `--http` (`/mcp` on `--host`/`--port`, 127.0.0.1:3001
//! by default), both through rmcp, the protocol's official Rust SDK.
//!
//! The server reads files and, when allowed, the network on behalf of a
//! model, whose inputs may be steered by the documents it reads — so what a
//! call can reach is the operator's decision, made on the command line:
//!
//! - local reads are confined to `--root DIR` (repeatable; the working
//!   directory by default) — paths are checked lexically first, so a probe
//!   outside the root learns nothing about what exists there, then after
//!   resolving symlinks;
//! - URLs are fetched only with `--allow-url-fetch`, through
//!   [`docling::fetch`]: the SSRF block-list on every redirect hop, a size
//!   cap (`DOCLING_RS_MAX_FETCH_BYTES`, 256 MiB) and timeouts. The options
//!   that reach the network on their own — remote `<img src>`
//!   (`image_sources=remote`), a request-supplied `vlm_endpoint`, the
//!   headless browser — sit behind the same flag, docling-serve's rules;
//!   options that name files (`xbrl_taxonomy`, a copybook path in
//!   `ebcdic_layout`) are held to the root;
//! - `--http` binds loopback unless `--allow-remote`, checks the `Host`
//!   header (DNS rebinding) and the browser `Origin` on a loopback bind, and
//!   takes an optional bearer token (`--bearer-token` /
//!   `DOCLING_RS_MCP_TOKEN`).
//!
//! Conversions run on the blocking pool, at most `--concurrency` (2) at a
//! time; a call that carries a progress token gets a heartbeat
//! `notifications/progress` every 10 s, so clients that reset their request
//! timeout on progress wait out a long PDF. stdout is the stdio transport's
//! wire — everything else the process says goes to stderr.
//!
//! The same server runs from the `docling-rs` binary ([`run`]) and from
//! embedders such as the Python package's `docling-rs-mcp` launcher
//! ([`run_with`] and a [`Launcher`] naming the command and where its PDF
//! models come from).

use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use docling::{ConvertOptions, DocumentConverter, ImageMode, InputFormat, SourceDocument};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProgressNotificationParam, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler, ServiceExt};
use serde::Deserialize;

/// How the server presents itself: the command that starts it (in `--help`,
/// usage errors and the startup line), the client configuration `--help`
/// shows, and how to get the PDF models when they are missing. The binary
/// uses [`Launcher::CLI`].
#[derive(Clone, Copy, Debug)]
pub struct Launcher<'a> {
    /// The command line that starts the server, e.g. `docling-rs mcp`.
    pub command: &'a str,
    /// A Claude Desktop `mcpServers` entry that launches it.
    pub client_config: &'a str,
    /// The sentence that tells the operator how to fetch the layout models.
    pub models_hint: &'a str,
    /// Flags the launcher handles itself, listed in `--help` (empty for none).
    pub extra_flags: &'a str,
}

impl Launcher<'static> {
    /// The `docling-rs` binary's `mcp` subcommand.
    pub const CLI: Self = Self {
        command: "docling-rs mcp",
        client_config: "{\"mcpServers\": {\"docling\": {\"command\": \"docling-rs\",\n     \"args\": [\"mcp\", \"--root\", \"/Users/me/Documents\"]}}}",
        models_hint: "Fetch them with scripts/install/download_dependencies.sh (--cli \
                      beside a cargo-installed binary), or point DOCLING_RS_MODELS_DIR at a \
                      .models directory.",
        extra_flags: "",
    };
}

fn help(launcher: &Launcher) -> String {
    let cmd = launcher.command;
    let pad = " ".repeat(cmd.len());
    format!(
        "\
{cmd} — a Model Context Protocol server over the docling.rs converter

usage: {cmd} [--root DIR]... [--allow-url-fetch] [--concurrency N]
       {cmd} --http [--host HOST] [--port PORT] [--allow-remote]
       {pad} [--bearer-token TOKEN] [...]

Tools: convert_to_markdown(uri) (MarkItDown's MCP tool, same contract),
convert(uri, to, images, options), list_input_formats().

  --root DIR          directory local paths and file: URIs may read from;
                      repeatable (default: the working directory)
  --allow-url-fetch   convert http(s) URLs, remote <img src> and a
                      request-supplied vlm_endpoint (outbound requests from
                      this machine; private and loopback targets stay refused
                      unless DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1)
  --concurrency N     conversions running at once (default 2)
  --http              serve Streamable HTTP at /mcp instead of stdio
  --host HOST         bind address for --http (default 127.0.0.1)
  --port PORT         port for --http (default 3001)
  --allow-remote      let --http bind a non-loopback address
  --bearer-token T    require `Authorization: Bearer T` on --http
                      (default: DOCLING_RS_MCP_TOKEN when set)
{}
Claude Desktop (claude_desktop_config.json):
  {}",
        if launcher.extra_flags.is_empty() {
            String::new()
        } else {
            format!("{}\n", launcher.extra_flags.trim_end())
        },
        launcher.client_config
    )
}

/// A `--root` directory in both spellings a path may arrive in: as the
/// operator gave it (absolute, `..` folded — what an agent quotes back) and
/// canonical (symlinks resolved: macOS's `/tmp` is `/private/tmp`, Windows
/// adds a `\\?\` prefix).
struct Root {
    given: PathBuf,
    canonical: PathBuf,
}

impl Root {
    fn new(dir: &str) -> Result<Self, String> {
        let canonical = std::fs::canonicalize(dir).map_err(|e| format!("--root {dir}: {e}"))?;
        if !canonical.is_dir() {
            return Err(format!("--root {dir}: not a directory"));
        }
        let given = std::path::absolute(dir).map_err(|e| format!("--root {dir}: {e}"))?;
        Ok(Self {
            given: normalize(&given),
            canonical,
        })
    }
}

/// What the operator allowed, shared by every session.
struct State {
    /// The directories local reads are confined to; the first one resolves
    /// relative paths.
    roots: Vec<Root>,
    allow_url_fetch: bool,
    /// How `http(s)` inputs are fetched (only with `--allow-url-fetch`).
    fetch_policy: docling::FetchPolicy,
    permits: Arc<tokio::sync::Semaphore>,
    /// The missing-models note is printed once per process.
    warned_text_layer: AtomicBool,
    /// The command that started the server, for the stderr notes.
    command: String,
    /// How to get the layout models when they are missing.
    models_hint: String,
    /// The PDF/image models, loaded on the first call that needs them and
    /// kept for the next — docling-serve's warm pipeline, shared by every
    /// session of this process.
    warm: docling::WarmPipeline,
}

struct HttpConfig {
    host: String,
    port: u16,
    allow_remote: bool,
    token: Option<String>,
}

/// `docling-rs mcp …` — parse the flags, then serve until the client hangs
/// up (stdio) or Ctrl-C (HTTP).
pub fn run(args: Vec<String>) -> ExitCode {
    ExitCode::from(run_with(args, &Launcher::CLI))
}

/// [`run`] for an embedder: the flags after the command, how the server
/// should name itself; returns the process exit status (0, 1 on a serving
/// error, 2 on a usage error). Call it before other threads read the
/// environment — it defaults `DOCLING_RS_NO_ARENA`.
pub fn run_with(args: Vec<String>, launcher: &Launcher) -> u8 {
    // A long-lived server holding warm ONNX sessions, like `serve` (#263):
    // the CPU arena off unless the operator says otherwise. Measured on the
    // warm MCP server (4 vCPU, a 9-page paper and two short PDFs in turn):
    // 2.1 GB live with the arena, 0.55 GB — the models — without, at the
    // same latency.
    if std::env::var_os("DOCLING_RS_NO_ARENA").is_none() {
        std::env::set_var("DOCLING_RS_NO_ARENA", "1");
    }
    let usage = |err: &str| usage(launcher, err);
    let mut roots: Vec<String> = Vec::new();
    let mut allow_url_fetch = false;
    let mut concurrency: usize = 2;
    let mut http = false;
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 3001;
    let mut allow_remote = false;
    let mut token = docling_core::env::nonempty("DOCLING_RS_MCP_TOKEN");
    // The `--http`-only flags, so a stdio launch that carries one fails
    // loudly instead of ignoring it.
    let mut http_flag: Option<&'static str> = None;
    let args = match split_flag_values(args) {
        Ok(args) => args,
        Err(e) => return usage(&e),
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", help(launcher));
                return 0;
            }
            "--root" => match it.next() {
                Some(v) if !v.is_empty() => roots.push(v),
                _ => return usage("--root needs a directory"),
            },
            "--allow-url-fetch" => allow_url_fetch = true,
            "--concurrency" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) if n >= 1 => concurrency = n,
                _ => return usage("--concurrency needs a positive integer"),
            },
            "--http" => http = true,
            "--host" => match it.next() {
                Some(v) if !v.is_empty() => {
                    host = v;
                    http_flag = Some("--host");
                }
                _ => return usage("--host needs an address"),
            },
            "--port" => match it.next().and_then(|v| v.parse().ok()) {
                Some(p) => {
                    port = p;
                    http_flag = Some("--port");
                }
                None => return usage("--port needs a port number"),
            },
            "--allow-remote" => {
                allow_remote = true;
                http_flag = Some("--allow-remote");
            }
            "--bearer-token" => match it.next() {
                Some(v) if !v.is_empty() => {
                    token = Some(v);
                    http_flag = Some("--bearer-token");
                }
                _ => return usage("--bearer-token needs a token"),
            },
            other => return usage(&format!("unknown argument '{other}'")),
        }
    }
    if let (false, Some(flag)) = (http, http_flag) {
        return usage(&format!("{flag} applies to --http only"));
    }
    let defaulted = roots.is_empty();
    if defaulted {
        roots.push(".".to_string());
    }
    let roots = match roots
        .iter()
        .map(|r| Root::new(r))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(roots) => roots,
        Err(e) => return usage(&e),
    };
    // Desktop clients often start servers in `/`: the working-directory
    // default would then open the whole file system. That takes an explicit
    // `--root`.
    if defaulted && roots[0].canonical.parent().is_none() {
        return usage(&format!(
            "the working directory is the file system root ({}); pass --root DIR \
             to choose what the server may read",
            roots[0].canonical.display()
        ));
    }
    let state = Arc::new(State {
        roots,
        allow_url_fetch,
        fetch_policy: fetch_policy(),
        permits: Arc::new(tokio::sync::Semaphore::new(concurrency)),
        warned_text_layer: AtomicBool::new(false),
        command: launcher.command.to_string(),
        models_hint: launcher.models_hint.to_string(),
        warm: docling::WarmPipeline::new(),
    });
    // A long-lived process: large buffers are mapped and returned, not
    // carved from the per-thread arenas where they stay resident.
    docling::memory::pin_mmap_threshold();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: tokio runtime: {e}");
            return 1;
        }
    };
    let result = if http {
        let cfg = HttpConfig {
            host,
            port,
            allow_remote,
            token,
        };
        runtime.block_on(serve_http(state, cfg))
    } else {
        runtime.block_on(serve_stdio(state))
    };
    // A conversion still running for a client that hung up has no one to
    // answer: exit now rather than wait for it.
    runtime.shutdown_background();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// The `--flag=value` spelling next to `--flag value`, so the argument
/// lists MCP catalogs generate work (the registry's `server.json` describes
/// a named argument as `--flag={value}`): value flags split in two, the
/// switches take `=true` / `=false`.
fn split_flag_values(args: Vec<String>) -> Result<Vec<String>, String> {
    const SWITCHES: [&str; 3] = ["--allow-url-fetch", "--http", "--allow-remote"];
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        let Some((flag, value)) = arg.split_once('=').filter(|(f, _)| f.starts_with("--")) else {
            out.push(arg);
            continue;
        };
        if SWITCHES.contains(&flag) {
            match value.to_ascii_lowercase().as_str() {
                "" | "true" | "1" | "yes" | "on" => out.push(flag.to_string()),
                "false" | "0" | "no" | "off" => {}
                _ => return Err(format!("{flag} takes true or false, not '{value}'")),
            }
        } else {
            out.push(flag.to_string());
            out.push(value.to_string());
        }
    }
    Ok(out)
}

fn usage(launcher: &Launcher, err: &str) -> u8 {
    eprintln!("error: {err}");
    eprintln!(
        "usage: {} [--root DIR]... [--allow-url-fetch] [--concurrency N] [--http [--host HOST] [--port PORT] [--allow-remote] [--bearer-token TOKEN]]",
        launcher.command
    );
    2
}

/// One line on stderr saying what this server may reach — the operator's
/// check that the flags did what they meant.
fn describe(state: &State, transport: &str) {
    eprintln!(
        "{} {} · {transport} · root {} · URL fetch {}",
        state.command,
        env!("CARGO_PKG_VERSION"),
        state.roots_display(),
        if state.allow_url_fetch { "on" } else { "off" }
    );
}

impl State {
    /// The roots as the operator gave them, comma-separated.
    fn roots_display(&self) -> String {
        let roots: Vec<String> = self
            .roots
            .iter()
            .map(|r| r.given.display().to_string())
            .collect();
        roots.join(", ")
    }
}

async fn serve_stdio(state: Arc<State>) -> Result<(), String> {
    describe(&state, "stdio");
    let service = DoclingMcp::new(state)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("mcp: {e}"))?;
    service
        .waiting()
        .await
        .map(|_| ())
        .map_err(|e| format!("mcp: {e}"))
}

async fn serve_http(state: Arc<State>, cfg: HttpConfig) -> Result<(), String> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let addr = tokio::net::lookup_host((cfg.host.as_str(), cfg.port))
        .await
        .map_err(|e| format!("--host {}: {e}", cfg.host))?
        .next()
        .ok_or_else(|| format!("--host {}: no address", cfg.host))?;
    let loopback = addr.ip().is_loopback();
    if !loopback && !cfg.allow_remote {
        return Err(format!(
            "refusing to bind {addr}: not a loopback address — the server reads files under \
             its root for whoever can reach it; pass --allow-remote (and --bearer-token) to \
             expose it"
        ));
    }
    if !loopback && cfg.token.is_none() {
        eprintln!(
            "warning: serving {addr} without a bearer token — anyone who can reach it can \
             read the files under the root"
        );
    }
    let mut config = StreamableHttpServerConfig::default();
    // Ctrl-C closes the open sessions as well as the listener.
    let ct = config.cancellation_token.clone();
    config = if loopback {
        // The default allowed `Host`s (localhost, 127.0.0.1, ::1) stop DNS
        // rebinding; the `Origin` list stops a web page in the user's own
        // browser from driving the server — requests without an `Origin`
        // (every non-browser client) pass.
        config.with_allowed_origins(["http://localhost:*", "http://127.0.0.1:*", "http://[::1]:*"])
    } else {
        // Remote clients name the server by its own host name; the bearer
        // token is the control there.
        config.disable_allowed_hosts()
    };
    let service: StreamableHttpService<DoclingMcp, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                move || Ok(DoclingMcp::new(state.clone()))
            },
            Default::default(),
            config,
        );
    let mut router = axum::Router::new().nest_service("/mcp", service);
    if let Some(token) = cfg.token.clone() {
        let expected: Arc<[u8]> = format!("Bearer {token}").into_bytes().into();
        router = router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let expected = expected.clone();
                async move {
                    let given = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .map(|v| v.as_bytes())
                        .unwrap_or_default();
                    if constant_time_eq(given, &expected) {
                        next.run(req).await
                    } else {
                        axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::UNAUTHORIZED,
                            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                            "missing or wrong bearer token\n",
                        ))
                    }
                }
            },
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("binding {addr}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    describe(&state, &format!("Streamable HTTP on http://{bound}/mcp"));
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            ct.cancel();
        })
        .await
        .map_err(|e| format!("serving {bound}: {e}"))
}

/// Ctrl-C, or SIGTERM on Unix — what `docker stop` sends, and a process
/// that is a container's PID 1 ignores a signal it installed no handler for.
async fn shutdown_signal() {
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate => {}
    }
}

/// Byte comparison whose time does not depend on where the inputs differ —
/// a token check must not leak its prefix through response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// --- the tools ----------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct ConvertToMarkdownArgs {
    /// The document to read: an http: or https: URL (only when the server
    /// allows URL fetching — see list_input_formats), a file: URI, a data:
    /// URI, or a file path. Relative paths resolve against the server's
    /// root; files outside it are refused.
    uri: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ConvertArgs {
    /// The document to read: an http: or https: URL (only when the server
    /// allows URL fetching — see list_input_formats), a file: URI, a data:
    /// URI, or a file path. Relative paths resolve against the server's
    /// root; files outside it are refused.
    uri: String,
    /// Output format: md (Markdown, the default), json (docling's
    /// DoclingDocument: structure, tables and page provenance), html, text,
    /// latex, chunks (RAG-ready chunk records as JSON), vtt (WebVTT, for
    /// audio/video transcripts) or pandoc (Pandoc's JSON AST).
    // Optional parameters are single-typed in the schema and left out of
    // `required` — a `["string", "null"]` type union is what some clients'
    // function-calling dialects reject (MCP Inspector's portability check);
    // an explicit `null` still deserializes as "not given".
    #[serde(default)]
    #[schemars(transform = drop_null)]
    to: Option<String>,
    /// Pictures in md, html or pandoc output: placeholder (the default, an
    /// `<!-- image -->` marker) or embedded (the image as a data: URI).
    #[serde(default)]
    #[schemars(transform = drop_null)]
    images: Option<String>,
    /// Conversion options, docling.rs's shared set (docs/OPTIONS.md), e.g.
    /// {"pages": "1-5"}, {"text_layer_only": true} (digital PDFs, fastest;
    /// "auto" = the text layer when it has text, OCR when it has none),
    /// {"ocr_lang": "ch"}, {"force_full_page_ocr": true},
    /// {"do_picture_ocr": true} (OCR pictures inside Office files),
    /// {"password": "…"}, {"compact_tables": false}. A value may also be
    /// given as text ("true", "1.5"), as a form sends it.
    #[serde(default)]
    #[schemars(transform = drop_null)]
    options: Option<serde_json::Map<String, serde_json::Value>>,
}

/// An `Option<T>` field's schema as `T`'s: `"type": ["T", "null"]` → `"T"`,
/// and no `"default": null` — absence already says "not given".
fn drop_null(schema: &mut schemars::Schema) {
    if let Some(serde_json::Value::Array(types)) = schema.get("type").cloned() {
        let kept: Vec<_> = types.into_iter().filter(|t| t != "null").collect();
        if let [single] = kept.as_slice() {
            schema.insert("type".into(), single.clone());
        }
    }
    if schema.get("default") == Some(&serde_json::Value::Null) {
        schema.remove("default");
    }
}

/// The MCP service: one per stdio connection or HTTP session, sharing the
/// operator's [`State`].
#[derive(Clone)]
struct DoclingMcp {
    state: Arc<State>,
    tool_router: ToolRouter<Self>,
}

impl DoclingMcp {
    fn new(state: Arc<State>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    /// Convert on the blocking pool under a concurrency permit, sending a
    /// progress heartbeat while it runs when the caller asked for progress.
    async fn run_conversion(
        &self,
        request: Request,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let permit = self
            .state
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| e.to_string())?;
        let state = self.state.clone();
        // The permit lives as long as the work: a call the client cancels
        // drops this future, not the conversion already running, and must
        // not free a slot that conversion still occupies.
        let mut job = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = convert(&state, &request);
            // What the conversion freed goes back to the OS, not into the
            // allocator's arenas (`docling::memory`).
            docling::memory::release_free_memory();
            result
        });
        let token = ctx.meta.get_progress_token();
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.tick().await; // the first tick is immediate
        let mut beats = 0u32;
        loop {
            tokio::select! {
                done = &mut job => {
                    return done.map_err(|e| format!("conversion worker failed: {e}"))?;
                }
                _ = tick.tick(), if token.is_some() => {
                    beats += 1;
                    let param = ProgressNotificationParam::new(
                        token.clone().expect("guarded"),
                        f64::from(beats),
                    )
                    .with_message(format!("converting ({}s)", beats * 10));
                    let _ = ctx.peer.notify_progress(param).await;
                }
            }
        }
    }
}

#[tool_router]
impl DoclingMcp {
    /// Read a document as Markdown, with default settings, and return the
    /// Markdown text. Handles PDF (layout analysis, tables, OCR for scans),
    /// Word, PowerPoint, Excel, HTML, EPUB, e-mail, images, audio and more.
    /// Use this whenever Markdown is all you need. Use `convert` instead for
    /// another output format (docling JSON, HTML, RAG chunks, …), embedded
    /// images, or options such as a page range, OCR language or password.
    /// Fails with a message when the document is missing, outside the
    /// server's root, of an unsupported type (see `list_input_formats`), or
    /// a URL while URL fetching is off. Read-only. Same name and contract as
    /// MarkItDown's MCP tool.
    #[tool(annotations(
        title = "Convert to Markdown",
        read_only_hint = true,
        open_world_hint = true
    ))]
    async fn convert_to_markdown(
        &self,
        Parameters(args): Parameters<ConvertToMarkdownArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let request = Request {
            uri: args.uri,
            to: "md".into(),
            images: ImageMode::Placeholder,
            options: ConvertOptions::default(),
        };
        self.run_conversion(request, &ctx).await
    }

    /// Convert a document to a chosen output format, with conversion
    /// options, and return the result as text. `to` picks the format
    /// (Markdown, docling JSON with structure and page provenance, HTML,
    /// plain text, LaTeX, RAG chunks, WebVTT, Pandoc JSON); `images` embeds
    /// pictures instead of placeholders; `options` sets page ranges, OCR
    /// language and mode, text-layer-only PDFs, passwords and the rest.
    /// For plain Markdown with default settings, `convert_to_markdown` is
    /// the simpler call. Inputs and errors as for `convert_to_markdown`;
    /// an unknown `to`, `images` value or option key is an error that says
    /// what is accepted. Read-only.
    #[tool(annotations(
        title = "Convert to a chosen format",
        read_only_hint = true,
        open_world_hint = true
    ))]
    async fn convert(
        &self,
        Parameters(args): Parameters<ConvertArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let request = Request::from_args(args)?;
        self.run_conversion(request, &ctx).await
    }

    /// List the file extensions this server can convert, one per line, and
    /// whether it accepts http(s) URLs. Takes no arguments and converts
    /// nothing. Call it before `convert_to_markdown` or `convert` when
    /// unsure whether a file type is supported, or to learn why a URL was
    /// refused.
    #[tool(annotations(title = "List input formats", read_only_hint = true))]
    async fn list_input_formats(&self) -> String {
        let mut out = InputFormat::supported_extensions().join("\n");
        out.push_str(if self.state.allow_url_fetch {
            "\n\nURL inputs (http/https): enabled"
        } else {
            "\n\nURL inputs (http/https): disabled (the server was started without --allow-url-fetch)"
        });
        out
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DoclingMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = Implementation::new("docling-rs", env!("CARGO_PKG_VERSION"));
        info.title = Some("docling.rs".into());
        info.website_url = Some("https://github.com/docling-project/docling.rs".into());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(info)
            .with_instructions(format!(
                "Converts documents to Markdown or docling JSON for reading: PDF (layout, \
                 tables, OCR), DOCX, PPTX, XLSX, HTML, EPUB, e-mail, images, audio and more. \
                 Tools: convert_to_markdown for Markdown with default settings; convert for \
                 any other output format or for options (e.g. {{\"pages\": \"1-5\"}}); \
                 list_input_formats to check what this server accepts. Local files: paths under {}. URLs: {}.",
                self.state.roots_display(),
                if self.state.allow_url_fetch {
                    "enabled"
                } else {
                    "disabled on this server"
                }
            ))
    }
}

// --- the conversion -----------------------------------------------------------

/// One validated tool call.
struct Request {
    uri: String,
    to: String,
    images: ImageMode,
    options: ConvertOptions,
}

/// The `to` values the tools return — every text output of the CLI's
/// `--to` (`dclx` is a ZIP and `images` page PNGs, neither of which fits a
/// text result).
const OUTPUTS: &[&str] = &[
    "md", "json", "html", "text", "latex", "chunks", "vtt", "pandoc",
];

impl Request {
    fn from_args(args: ConvertArgs) -> Result<Self, String> {
        let to = match args.to.as_deref().map(str::trim) {
            None | Some("") | Some("md") | Some("markdown") => "md".to_string(),
            Some(t) if OUTPUTS.contains(&t) => t.to_string(),
            Some(t) => {
                return Err(format!(
                    "unknown to={t:?} (expected one of: {})",
                    OUTPUTS.join(", ")
                ))
            }
        };
        let images = match args.images.as_deref().map(str::trim) {
            None | Some("") | Some("placeholder") => ImageMode::Placeholder,
            Some("embedded") => ImageMode::Embedded,
            Some(other) => {
                return Err(format!(
                    "unknown images={other:?} (expected: placeholder, embedded)"
                ))
            }
        };
        let options = match args.options {
            None => ConvertOptions::default(),
            Some(map) => {
                let unknown = ConvertOptions::unknown_keys(map.keys().map(String::as_str), &[]);
                if let Some(key) = unknown.first() {
                    return Err(format!(
                        "options: unknown field `{key}` (see docs/OPTIONS.md for the list)"
                    ));
                }
                // Values as text too — MCP Inspector's form sends every
                // value as a string: `{"force_full_page_ocr": "true"}`.
                ConvertOptions::from_loose_json(map).map_err(|e| format!("options: {e}"))?
            }
        };
        Ok(Self {
            uri: args.uri,
            to,
            images,
            options,
        })
    }
}

/// Resolve, convert and render one request — the blocking half of a tool
/// call.
fn convert(state: &State, request: &Request) -> Result<String, String> {
    let source = resolve_source(state, &request.uri)?;
    let mut options = guard_options(state, &request.options)?;
    // Markdown for a model to read: unpadded `| a | b |` tables carry the
    // same cells in far fewer tokens (the MarkItDown facade's choice too);
    // the caller's explicit `compact_tables` wins.
    if request.to == "md" && options.compact_tables.is_none() {
        options.compact_tables = Some(true);
    }
    let document = convert_document(state, &source, &options)?;
    render(
        &document,
        &request.to,
        request.images,
        request.options.track_changes().ok().flatten(),
    )
}

/// The tools answer with the document alone, so the problems a partial
/// result survived (docling's `PARTIAL_SUCCESS`: a spent document budget, a
/// failed or cut-off VLM page, #762/#763) go to the server's stderr — the
/// client's log — instead of disappearing.
fn warn_partial(state: &State, source: &SourceDocument, errors: &[docling::ErrorItem]) {
    for e in errors {
        eprintln!(
            "{}: {}: partial document: {}",
            state.command,
            source.name,
            e.describe()
        );
    }
}

fn convert_document(
    state: &State,
    source: &SourceDocument,
    options: &ConvertOptions,
) -> Result<docling::DoclingDocument, String> {
    let converter = options
        .apply(DocumentConverter::new().warm_pipeline(&state.warm))
        .map_err(|e| e.to_string())?;
    // The remote VLM pipeline (#304) is a sibling path, as in docling-serve.
    if let Some(vlm) = options.vlm_options().map_err(|e| e.to_string())? {
        let result = docling::vlm::convert_vlm_result(source, &vlm).map_err(|e| e.to_string())?;
        warn_partial(state, source, &result.errors);
        let mut document = result.document;
        converter.finish_document(&mut document);
        return Ok(document);
    }
    match converter.convert(source.clone()) {
        Ok(result) => {
            warn_partial(state, source, &result.errors);
            Ok(result.document)
        }
        // No layout model on this machine (the release binaries ship none):
        // a digital PDF still has its text layer, which is what MarkItDown
        // reads — convert that rather than fail, and say once how to get the
        // full pipeline. An explicit `text_layer_only` is the caller's call.
        Err(e)
            if source.format == InputFormat::Pdf
                && options.text_layer_only.is_none()
                && e.to_string().contains("model not found") =>
        {
            if !state.warned_text_layer.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "{}: the layout models are missing, so PDFs convert from their text \
                     layer (no layout analysis, table structure or OCR). {}",
                    state.command, state.models_hint
                );
            }
            // A converter of its own: the text-layer pipeline loads no
            // models, and putting it in the warm slot would only evict them.
            let retry = DocumentConverter::new().text_layer_only(true);
            let converter = options.apply(retry).map_err(|e| e.to_string())?;
            converter
                .convert(source.clone())
                .map(|r| r.document)
                .map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn render(
    document: &docling::DoclingDocument,
    to: &str,
    images: ImageMode,
    track_changes: Option<docling::TrackChanges>,
) -> Result<String, String> {
    Ok(match to {
        "md" => match images {
            ImageMode::Placeholder => document.export_to_markdown(),
            mode => document.export_to_markdown_with_images(mode, "artifacts").0,
        },
        "json" => document.export_to_json(),
        "html" => document.export_to_html(),
        "text" => document.export_to_text(),
        "latex" => document.export_to_latex(),
        "vtt" => document.export_to_vtt(),
        "chunks" => {
            let mut warn = |msg: String| eprintln!("warning: {msg}");
            let records = docling::chunks::chunk_records(document, &mut warn);
            serde_json::to_string_pretty(&records).map_err(|e| e.to_string())?
        }
        "pandoc" => {
            document
                .export_to_pandoc_json_with(&docling::pandoc::PandocExportOptions {
                    image_mode: images,
                    track_changes,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?
                .0
        }
        other => return Err(format!("unknown to={other:?}")),
    })
}

/// The caller's options under the operator's policy — docling-serve's rules
/// for everything that leaves the machine, the root for everything that
/// names a file.
fn guard_options(state: &State, options: &ConvertOptions) -> Result<ConvertOptions, String> {
    let mut o = options.clone();
    o.validate().map_err(|e| e.to_string())?;
    // Remote `<img src>` is an outbound fetch like a URL input: without the
    // flag the call gets the safe tier (`data:` URIs and container parts)
    // instead of a surprise request — docling-serve's behavior.
    let requested = o.image_sources().map_err(|e| e.to_string())?.or_else(|| {
        o.fetch_images
            .filter(|&f| f)
            .map(|_| docling::ImageSources::Remote)
    });
    if requested == Some(docling::ImageSources::Remote) && !state.allow_url_fetch {
        o.image_sources = Some(docling::ImageSources::Embedded.to_string());
        o.fetch_images = Some(false);
    }
    if let Some(endpoint) = o.vlm_endpoint.as_deref().filter(|v| !v.trim().is_empty()) {
        if !state.allow_url_fetch {
            return Err(
                "a request-supplied vlm_endpoint needs the server started with \
                 --allow-url-fetch (or pin it with DOCLING_RS_VLM_ENDPOINT)"
                    .into(),
            );
        }
        docling::fetch::check_outbound_url(endpoint, false)?;
    }
    if o.use_web_browser == Some(true) && !state.allow_url_fetch {
        return Err(
            "use_web_browser loads the page's remote resources; it needs the server \
             started with --allow-url-fetch"
                .into(),
        );
    }
    if let Some(dir) = o.xbrl_taxonomy.as_deref() {
        o.xbrl_taxonomy = Some(local_path(state, Path::new(dir))?.display().to_string());
    }
    // Inline copybook JSON is data; anything else names a file.
    if let Some(layout) = o.ebcdic_layout.as_deref() {
        if !layout.trim_start().starts_with('{') {
            o.ebcdic_layout = Some(local_path(state, Path::new(layout))?.display().to_string());
        }
    }
    // A preset is a directory name under `.models/asr/`, never a path.
    if let Some(model) = o.asr_model.as_deref() {
        if model.contains(['/', '\\']) || model.contains("..") {
            return Err(format!("asr_model must be a preset name, got {model:?}"));
        }
    }
    Ok(o)
}

/// The document a `uri` names: a `data:` URI, an `http(s)` URL (with
/// `--allow-url-fetch`), a `file:` URI or a path under the root.
fn resolve_source(state: &State, uri: &str) -> Result<SourceDocument, String> {
    let uri = uri.trim();
    if uri.is_empty() {
        return Err("uri is empty".into());
    }
    let lower = uri.get(..8).unwrap_or(uri).to_ascii_lowercase();
    if lower.starts_with("data:") {
        return docling::fetch::from_data_url(uri).map_err(|e| e.to_string());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        if !state.allow_url_fetch {
            return Err(format!(
                "fetching URLs is disabled on this server (it was started without \
                 --allow-url-fetch); pass a local path or file: URI under {} instead",
                state.roots_display()
            ));
        }
        return SourceDocument::from_url(uri, &state.fetch_policy).map_err(|e| e.to_string());
    }
    let path = if lower.starts_with("file:") {
        docling::fetch::file_url_path(uri)?
    } else if let Some(scheme) = uri_scheme(uri) {
        return Err(format!(
            "unsupported URI scheme {scheme:?} (expected http, https, file, data, or a path)"
        ));
    } else {
        PathBuf::from(uri)
    };
    let path = local_path(state, &path)?;
    if path.is_dir() {
        return Err(format!("{} is a directory, not a document", path.display()));
    }
    match SourceDocument::from_file(&path) {
        Ok(source) => Ok(source),
        // No (known) extension: read it and let the content decide.
        Err(_) => {
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("document");
            let mut source = SourceDocument::from_named_bytes(name, bytes, None)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            source.path = Some(path);
            Ok(source)
        }
    }
}

/// The server's URL-fetch policy on top of the library's bounds
/// (`DOCLING_RS_MAX_FETCH_BYTES`, 10 s connect / 300 s total): up to five
/// redirects, each hop re-checked against the private-address block —
/// links agents are handed (`http:` → `https:`, an arXiv abstract → its
/// PDF) redirect routinely, and the per-hop check keeps a public URL from
/// bouncing onto an internal one. docling-serve refuses redirects instead.
fn fetch_policy() -> docling::FetchPolicy {
    docling::FetchPolicy {
        max_redirects: 5,
        ..docling::FetchPolicy::default()
    }
}

/// `scheme` of `scheme:rest` when `uri` has one — a URI scheme is two or
/// more characters, so a Windows drive (`C:\docs\a.pdf`) is a path.
fn uri_scheme(uri: &str) -> Option<&str> {
    let (scheme, _) = uri.split_once(':')?;
    let valid = scheme.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

/// `path` resolved against the first root and confined to the roots: the
/// lexically normalized path must lie under one (so nothing outside is
/// touched — not even to learn whether it exists), and so must the
/// canonical one (a symlink inside a root cannot lead out of it). The
/// lexical check takes a root in either spelling, so `/tmp/docs/a.pdf`
/// passes under a `/tmp/docs` root whose canonical form is
/// `/private/tmp/docs`.
fn local_path(state: &State, path: &Path) -> Result<PathBuf, String> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        state.roots[0].canonical.join(path)
    };
    let outside = || {
        format!(
            "{} is outside the server's root ({}); start the server with --root to allow it",
            path.display(),
            state.roots_display()
        )
    };
    let lexical = normalize(&joined);
    let under = |p: &Path| {
        let p = plain(p);
        state
            .roots
            .iter()
            .any(|r| p.starts_with(plain(&r.given)) || p.starts_with(plain(&r.canonical)))
    };
    if !under(&lexical) {
        return Err(outside());
    }
    let canonical = std::fs::canonicalize(&lexical)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if !state
        .roots
        .iter()
        .any(|r| canonical.starts_with(&r.canonical))
    {
        return Err(outside());
    }
    Ok(canonical)
}

/// The path without Windows' `\\?\` verbatim prefix: `canonicalize` spells
/// every root that way there, and the plain `C:\…` a client sends names
/// the same place without starting with it — so the lexical check compares
/// both sides in the plain spelling (a UNC `\\?\UNC\host\share` as
/// `\\host\share`). Elsewhere the path is returned as it is.
fn plain(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return match rest.strip_prefix(r"UNC\") {
                Some(unc) => PathBuf::from(format!(r"\\{unc}")),
                None => PathBuf::from(rest),
            };
        }
    }
    path.to_path_buf()
}

/// `.` and `..` folded away without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A `data:[<mime>][;charset=…][;base64],<payload>` URI as a document: the
/// format from the media type, else the content's signature; a `charset`
/// decodes text formats.
#[cfg(test)]
mod tests {
    use super::*;

    fn state(root: &Path, allow_url_fetch: bool) -> State {
        State {
            roots: vec![Root::new(root.to_str().unwrap()).unwrap()],
            allow_url_fetch,
            fetch_policy: fetch_policy(),
            permits: Arc::new(tokio::sync::Semaphore::new(1)),
            warned_text_layer: AtomicBool::new(false),
            command: Launcher::CLI.command.to_string(),
            models_hint: Launcher::CLI.models_hint.to_string(),
            warm: docling::WarmPipeline::new(),
        }
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data")
    }

    #[test]
    fn paths_stay_under_the_root() {
        let root = fixtures().join("md");
        let s = state(&root, false);
        // Relative paths resolve against the root.
        let ok = local_path(&s, Path::new("sources")).unwrap();
        assert!(ok.ends_with("md/sources"));
        // `..` out of the root is refused before the file system is asked.
        for escape in [
            "../docx/sources",
            "sources/../../docx",
            "/etc/passwd",
            "/nonexistent/x.pdf",
        ] {
            let err = local_path(&s, Path::new(escape)).unwrap_err();
            assert!(err.contains("outside the server's root"), "{escape}: {err}");
        }
    }

    /// A root reached through a symlink (macOS's `/tmp` → `/private/tmp`)
    /// accepts absolute paths in the spelling the operator gave.
    #[cfg(unix)]
    #[test]
    fn a_root_behind_a_symlink_takes_both_spellings() {
        let dir = std::env::temp_dir().join(format!("docling-mcp-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("docs");
        std::os::unix::fs::symlink(fixtures().join("md"), &link).unwrap();
        let s = state(&link, false);
        let via_link = link.join("sources");
        assert!(local_path(&s, &via_link).is_ok());
        let via_target = fixtures().join("md/sources");
        assert!(local_path(&s, &std::fs::canonicalize(via_target).unwrap()).is_ok());
        assert!(local_path(&s, &dir.join("docs/../elsewhere")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The canonical root is spelled `\\?\C:\…`, an absolute path the
    /// client sends `C:\…` — one place, so the latter is under the root.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_root_takes_a_plain_absolute_path() {
        let root = std::fs::canonicalize(fixtures().join("md")).unwrap();
        let text = root.to_string_lossy();
        let Some(plain_root) = text.strip_prefix(r"\\?\") else {
            panic!("not a verbatim path: {text}");
        };
        let s = state(&root, false);
        assert!(local_path(&s, &Path::new(plain_root).join("sources")).is_ok());
        assert!(local_path(&s, Path::new(r"C:\Windows\win.ini")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_lead_out_of_the_root() {
        let dir = std::env::temp_dir().join(format!("docling-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(fixtures().join("docx"), dir.join("out")).unwrap();
        let s = state(&dir, false);
        let err = local_path(&s, Path::new("out/sources")).unwrap_err();
        assert!(err.contains("outside the server's root"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn uri_kinds() {
        assert_eq!(uri_scheme("s3://bucket/x.pdf"), Some("s3"));
        assert_eq!(uri_scheme("ftp://h/x"), Some("ftp"));
        assert_eq!(uri_scheme("C:\\docs\\a.pdf"), None);
        assert_eq!(uri_scheme("report.pdf"), None);
        let s = state(&fixtures(), false);
        let err = resolve_source(&s, "ftp://example.com/a.pdf").unwrap_err();
        assert!(err.contains("unsupported URI scheme"), "{err}");
        let err = resolve_source(&s, "https://example.com/a.pdf").unwrap_err();
        assert!(err.contains("--allow-url-fetch"), "{err}");
        let err = resolve_source(&s, "docx").unwrap_err();
        assert!(err.contains("is a directory"), "{err}");
    }

    /// `data:` URIs decode in the library (`docling::fetch::from_data_url`)
    /// whether or not URL fetching is on — nothing leaves the machine.
    #[test]
    fn data_uris_decode() {
        let s = state(&fixtures(), false);
        let src = resolve_source(&s, "data:text/markdown;base64,IyBUaXRsZQoKSGVsbG8=").unwrap();
        assert_eq!(src.format, InputFormat::Md);
        assert_eq!(src.bytes, b"# Title\n\nHello");
        let src = resolve_source(&s, "DATA:text/html,%3Ch1%3EHi%3C/h1%3E").unwrap();
        assert_eq!(src.format, InputFormat::Html);
        assert_eq!(src.bytes, b"<h1>Hi</h1>");
        // No media type: the signature decides.
        let src = resolve_source(&s, "data:;base64,JVBERi0xLjQK").unwrap();
        assert_eq!(src.format, InputFormat::Pdf);
        let src = resolve_source(&s, "data:text/csv;charset=shift_jis,a,b").unwrap();
        assert_eq!(src.encoding.as_deref(), Some("shift_jis"));
        assert!(resolve_source(&s, "data:text/plain").is_err());
        // Untyped text is plain text since #671; untyped binary is refused.
        let src = resolve_source(&s, "data:application/octet-stream,xyz").unwrap();
        assert_eq!(src.format, InputFormat::Text);
        assert!(resolve_source(&s, "data:application/octet-stream;base64,AAECAwQF").is_err());
    }

    #[test]
    fn options_follow_the_operator_policy() {
        let s = state(&fixtures(), false);
        let opts: ConvertOptions =
            serde_json::from_value(serde_json::json!({"image_sources": "remote"})).unwrap();
        let guarded = guard_options(&s, &opts).unwrap();
        assert_eq!(guarded.image_sources.as_deref(), Some("embedded"));
        let opts: ConvertOptions =
            serde_json::from_value(serde_json::json!({"vlm_endpoint": "http://h/v1"})).unwrap();
        assert!(guard_options(&s, &opts)
            .unwrap_err()
            .contains("--allow-url-fetch"));
        let opts: ConvertOptions =
            serde_json::from_value(serde_json::json!({"xbrl_taxonomy": "/etc"})).unwrap();
        assert!(guard_options(&s, &opts).unwrap_err().contains("outside"));
        let opts: ConvertOptions =
            serde_json::from_value(serde_json::json!({"asr_model": "../../x"})).unwrap();
        assert!(guard_options(&s, &opts)
            .unwrap_err()
            .contains("preset name"));
        // With the flag, remote images stay remote.
        let s = state(&fixtures(), true);
        let opts: ConvertOptions =
            serde_json::from_value(serde_json::json!({"fetch_images": true})).unwrap();
        let guarded = guard_options(&s, &opts).unwrap();
        assert_ne!(guarded.image_sources.as_deref(), Some("embedded"));
    }

    #[test]
    fn requests_validate_their_arguments() {
        let args = |v: serde_json::Value| serde_json::from_value::<ConvertArgs>(v).unwrap();
        let r = Request::from_args(args(serde_json::json!({"uri": "a.docx"}))).unwrap();
        assert_eq!(r.to, "md");
        let err = Request::from_args(args(serde_json::json!({"uri": "a", "to": "dclx"})))
            .map(|_| ())
            .unwrap_err();
        assert!(err.contains("unknown to"), "{err}");
        let err = Request::from_args(args(
            serde_json::json!({"uri": "a", "options": {"text_layer_onyl": true}}),
        ))
        .map(|_| ())
        .unwrap_err();
        assert!(err.contains("unknown field `text_layer_onyl`"), "{err}");
        let r = Request::from_args(args(
            serde_json::json!({"uri": "a", "to": "json", "options": {"pages": "1-2"}}),
        ))
        .unwrap();
        assert_eq!(r.options.pages.as_deref(), Some("1-2"));
        // A form's text values read as the option's type.
        let r = Request::from_args(args(serde_json::json!({
            "uri": "a",
            "options": {"force_full_page_ocr": "true", "ocr_scale": "1.5", "pages": "3", "video_frames": 2}
        })))
        .unwrap();
        assert_eq!(r.options.force_full_page_ocr, Some(true));
        assert_eq!(r.options.ocr_scale, Some(1.5));
        assert_eq!(r.options.pages.as_deref(), Some("3"));
        assert_eq!(r.options.video_frames, Some(2));
        let err = Request::from_args(args(
            serde_json::json!({"uri": "a", "options": {"force_full_page_ocr": "maybe"}}),
        ))
        .map(|_| ())
        .unwrap_err();
        assert!(
            err.contains("force_full_page_ocr: cannot read \"maybe\""),
            "{err}"
        );
    }

    #[test]
    fn converts_a_docx_to_compact_markdown() {
        let s = state(&fixtures(), false);
        let request = Request {
            uri: "docx/sources/lorem_ipsum.docx".into(),
            to: "md".into(),
            images: ImageMode::Placeholder,
            options: ConvertOptions::default(),
        };
        let md = convert(&s, &request).unwrap();
        assert!(md.contains("Lorem ipsum"), "{md}");
    }

    /// Calls share the server's warm pipeline (#669) without sharing their
    /// options: a page window on one call does not cut the next.
    #[test]
    fn calls_on_the_warm_pipeline_keep_their_own_options() {
        let warm = state(&fixtures(), false);
        let request = |pages: Option<&str>| Request {
            uri: "pdf/sources/2206.01062.pdf".into(),
            to: "json".into(),
            images: ImageMode::Placeholder,
            options: ConvertOptions {
                text_layer_only: Some(docling::TextLayerOnly::On),
                pages: pages.map(str::to_string),
                ..Default::default()
            },
        };
        for pages in [Some("2-3"), None, Some("1-1"), None] {
            let cold = convert(&state(&fixtures(), false), &request(pages)).unwrap();
            assert_eq!(
                convert(&warm, &request(pages)).unwrap(),
                cold,
                "pages {pages:?}"
            );
        }
    }

    /// `--flag=value` reads like `--flag value`; switches take true/false.
    #[test]
    fn flags_accept_the_equals_spelling() {
        let v = |a: &[&str]| split_flag_values(a.iter().map(|s| s.to_string()).collect());
        assert_eq!(
            v(&[
                "--root=/a",
                "--allow-url-fetch=true",
                "--http=false",
                "--port=9"
            ])
            .unwrap(),
            ["--root", "/a", "--allow-url-fetch", "--port", "9"]
        );
        assert_eq!(v(&["--root", "/a=b"]).unwrap(), ["--root", "/a=b"]);
        assert!(v(&["--allow-url-fetch=maybe"]).is_err());
    }

    /// Optional parameters are single-typed and not required (MCP
    /// Inspector's portability check: no `["string", "null"]` unions).
    #[test]
    fn tool_schemas_are_portable() {
        let schema = serde_json::to_value(schemars::schema_for!(ConvertArgs)).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["uri"]));
        for (name, ty) in [
            ("to", "string"),
            ("images", "string"),
            ("options", "object"),
        ] {
            let prop = &schema["properties"][name];
            assert_eq!(prop["type"], ty, "{name}: {prop}");
            assert!(prop.get("default").is_none(), "{name}: {prop}");
        }
        // An explicit null is still "not given".
        let args: ConvertArgs = serde_json::from_value(
            serde_json::json!({"uri": "a.md", "to": null, "images": null, "options": null}),
        )
        .unwrap();
        assert_eq!(Request::from_args(args).unwrap().to, "md");
    }

    #[test]
    fn bearer_comparison() {
        assert!(constant_time_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer abd", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer ab", b"Bearer abc"));
        assert!(!constant_time_eq(b"", b"Bearer abc"));
    }
}
