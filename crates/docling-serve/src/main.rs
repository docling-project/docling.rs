//! `docling-serve` — standalone binary for the HTTP conversion API.
//!
//! Usage: docling-serve [--addr HOST:PORT] [--concurrency N] [--max-body-mb N]
//!                      [--warmup] [--no-url-fetch] [--strict]
//!
//!   --addr HOST:PORT  bind address (default: 127.0.0.1:5001). Bind 0.0.0.0
//!                     only behind a trusted proxy — /v1/convert accepts URL
//!                     inputs (outbound fetches) unless --no-url-fetch.
//!   --concurrency N   max conversions in flight; excess requests queue
//!                     (default: 2)
//!   --max-body-mb N   request body cap for uploads, in MiB (default: 256)
//!   --warmup          load the PDF/image models at startup; /ready returns
//!                     503 until they are loaded
//!   --no-url-fetch    reject {"url": …} inputs; uploads only
//!   --strict          default to the cleaner strict Markdown dialect

use std::process::ExitCode;

use docling_serve::{serve, ServeConfig};

fn main() -> ExitCode {
    let mut cfg = ServeConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => match args.next() {
                Some(v) => cfg.addr = v,
                None => return usage("--addr needs HOST:PORT"),
            },
            "--concurrency" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) if v >= 1 => cfg.concurrency = v,
                _ => return usage("--concurrency needs a positive integer"),
            },
            "--max-body-mb" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v >= 1 => cfg.max_body_bytes = v * 1024 * 1024,
                _ => return usage("--max-body-mb needs a positive integer"),
            },
            "--warmup" => cfg.warmup = true,
            "--no-url-fetch" => cfg.allow_url_fetch = false,
            "--strict" => cfg.strict = true,
            "--help" | "-h" => return usage(""),
            other => return usage(&format!("unknown argument '{other}'")),
        }
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(err: &str) -> ExitCode {
    if !err.is_empty() {
        eprintln!("error: {err}");
    }
    eprintln!(
        "usage: docling-serve [--addr HOST:PORT] [--concurrency N] [--max-body-mb N] [--warmup] [--no-url-fetch] [--strict]"
    );
    if err.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}
