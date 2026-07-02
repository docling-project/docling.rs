//! `fleischwolf-rag` command-line interface.
//!
//! Subcommands: `init-db`, `ingest`, `query`, `eval`, `stats`. Configuration is
//! read from the environment / `.env` (see `RagConfig`); flags override it.

use clap::{Parser, Subcommand};
use fleischwolf_rag::config::{DbBackend, SourceKind};
use fleischwolf_rag::embed;
use fleischwolf_rag::eval::{EvalDataset, Harness};
use fleischwolf_rag::llm;
use fleischwolf_rag::model::RetrievalMode;
use fleischwolf_rag::{Pipeline, RagConfig, Result};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Parser)]
#[command(
    name = "fleischwolf-rag",
    about = "Pluggable RAG over the fleischwolf document converter"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the database schema (documents + chunks tables).
    InitDb,

    /// Ingest documents from the configured source (source → chunk → embed → store).
    Ingest {
        /// Override the source kind (folder|ftp|sftp).
        #[arg(long)]
        source: Option<String>,
        /// Override the source path / directory.
        #[arg(long)]
        path: Option<String>,
    },

    /// Retrieve chunks for a query (and optionally synthesize an answer).
    Query {
        /// The question / search query.
        query: String,
        /// Retrieval mode (vector|bm25|hybrid|multi-query|hyde).
        #[arg(long)]
        mode: Option<String>,
        /// Number of results.
        #[arg(long, short = 'k')]
        top_k: Option<usize>,
        /// Also synthesize an answer with the LLM (needs OPENROUTER_API_KEY).
        #[arg(long)]
        answer: bool,
    },

    /// Sweep chunk sizes / overlaps / modes over a labelled dataset.
    Eval {
        /// Path to a JSON dataset ({documents, queries}).
        #[arg(long)]
        dataset: PathBuf,
        /// Results per query.
        #[arg(long, default_value_t = 5)]
        top_k: usize,
        /// Emit JSON instead of a Markdown table.
        #[arg(long)]
        json: bool,
    },

    /// Print store statistics (document / chunk counts).
    Stats,

    /// Serve the REST API (documents info + search). Requires RAG_API_KEYS.
    Serve {
        /// Bind address (overrides RAG_HTTP_ADDR, default 127.0.0.1:8080).
        #[arg(long)]
        addr: Option<String>,
        /// Ingest the configured source before serving.
        #[arg(long)]
        ingest: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = RagConfig::from_env()?;

    match cli.cmd {
        Cmd::InitDb => {
            let pipeline = Pipeline::from_config(&cfg).await?;
            // from_config already ran migrations; report the target.
            println!(
                "initialized {} store at {}",
                backend_name(cfg.db_backend),
                cfg.database_url
            );
            let _ = pipeline;
        }

        Cmd::Ingest { source, path } => {
            if let Some(s) = source {
                cfg.source = parse_source(&s)?;
            }
            if let Some(p) = path {
                cfg.source_path = p;
            }
            let pipeline = Pipeline::from_config(&cfg).await?;
            let report = pipeline.ingest_all().await?;
            println!(
                "ingested {} document(s), skipped {}, failed {}, added {} chunk(s)",
                report.documents_ingested,
                report.documents_skipped,
                report.documents_failed,
                report.chunks_added
            );
        }

        Cmd::Query {
            query,
            mode,
            top_k,
            answer,
        } => {
            let mode = match mode {
                Some(m) => RetrievalMode::from_str(&m)?,
                None => cfg.retrieval_mode,
            };
            let k = top_k.unwrap_or(cfg.top_k);
            let pipeline = Pipeline::from_config(&cfg).await?;
            if answer {
                let a = pipeline.answer(&query, mode, k).await?;
                println!("{}\n", a.text);
                println!("— grounded in {} passage(s) —", a.sources.len());
            } else {
                let hits = pipeline.query(mode, &query, k).await?;
                if hits.is_empty() {
                    println!("(no results)");
                }
                for (i, h) in hits.iter().enumerate() {
                    let preview = h.chunk.text.replace('\n', " ");
                    let preview: String = preview.chars().take(160).collect();
                    println!("{:>2}. [{:.4}] {}", i + 1, h.score, preview);
                }
            }
        }

        Cmd::Eval {
            dataset,
            top_k,
            json,
        } => {
            let raw = std::fs::read_to_string(&dataset)?;
            let ds: EvalDataset = serde_json::from_str(&raw)?;
            // Use the configured embedder; attach the LLM only if a key is present.
            let embedder = embed::from_config(&cfg)?;
            let chat = match cfg.openrouter_api_key {
                Some(_) => Some(llm::from_config(&cfg)?),
                None => None,
            };
            let modes: Vec<RetrievalMode> = if chat.is_some() {
                RetrievalMode::ALL.to_vec()
            } else {
                RetrievalMode::OFFLINE.to_vec()
            };
            let chunk_configs = [(200usize, 0.0f32), (300, 0.05), (500, 0.1)];
            let harness = Harness::new(embedder, chat);
            let report = harness.run(&ds, &chunk_configs, &modes, top_k).await?;
            if json {
                println!("{}", report.to_json());
            } else {
                println!("{}", report.to_markdown());
            }
        }

        Cmd::Serve { addr, ingest } => {
            let pipeline = Pipeline::from_config(&cfg).await?;
            if ingest {
                let report = pipeline.ingest_all().await?;
                println!(
                    "ingested {} document(s), skipped {}, failed {}, added {} chunk(s)",
                    report.documents_ingested,
                    report.documents_skipped,
                    report.documents_failed,
                    report.chunks_added
                );
            }
            let bind = addr.unwrap_or_else(|| cfg.http_addr.clone());
            println!("serving REST API on http://{bind} (auth: X-Api-Key / Bearer)");
            fleischwolf_rag::api::serve(pipeline, &bind, cfg.api_keys.clone()).await?;
        }

        Cmd::Stats => {
            let pipeline = Pipeline::from_config(&cfg).await?;
            let docs = pipeline.store().count_documents().await?;
            let chunks = pipeline.store().count_chunks().await?;
            println!("documents: {docs}\nchunks:    {chunks}\n");
            let documents = pipeline.store().list_documents().await?;
            if !documents.is_empty() {
                println!(
                    "| title | KiB | pages | words | chunks | parse w/s | parse p/s | chunk w/s | embed w/s |"
                );
                println!(
                    "|-------|----:|------:|------:|-------:|----------:|----------:|----------:|----------:|"
                );
                for d in &documents {
                    let m = &d.metadata["metrics"];
                    let num = |v: &serde_json::Value| {
                        v.as_f64()
                            .map(|x| format!("{x:.1}"))
                            .unwrap_or_else(|| "-".into())
                    };
                    let int = |v: &serde_json::Value| {
                        v.as_u64()
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| "-".into())
                    };
                    let kib = m["file_bytes"]
                        .as_u64()
                        .map(|b| format!("{:.1}", b as f64 / 1024.0))
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                        d.title,
                        kib,
                        int(&m["pages"]),
                        int(&m["words"]),
                        int(&m["chunks"]),
                        num(&m["parsing"]["words_per_sec"]),
                        num(&m["parsing"]["pages_per_sec"]),
                        num(&m["chunking"]["words_per_sec"]),
                        num(&m["embedding"]["words_per_sec"]),
                    );
                }
            }
        }
    }
    Ok(())
}

fn backend_name(b: DbBackend) -> &'static str {
    match b {
        DbBackend::Sqlite => "sqlite",
        DbBackend::Postgres => "postgres",
        DbBackend::Memory => "memory",
    }
}

fn parse_source(s: &str) -> Result<SourceKind> {
    match s.to_ascii_lowercase().as_str() {
        "folder" | "dir" | "local" => Ok(SourceKind::Folder),
        "ftp" => Ok(SourceKind::Ftp),
        "sftp" => Ok(SourceKind::Sftp),
        other => Err(fleischwolf_rag::RagError::config(format!(
            "unknown source '{other}'"
        ))),
    }
}
