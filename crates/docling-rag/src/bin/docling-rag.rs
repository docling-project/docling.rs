//! `docling-rag` command-line interface.
//!
//! Subcommands: `init-db`, `ingest`, `query`, `eval`, `answers`, `prune`,
//! `stats`, `serve`. Configuration is read from the environment / `.env`
//! (see `RagConfig`); flags override it.

use clap::{Parser, Subcommand};
use docling_rag::config::{DbBackend, SourceKind};
use docling_rag::embed;
use docling_rag::eval::{EvalDataset, Harness};
use docling_rag::llm;
use docling_rag::model::RetrievalMode;
use docling_rag::{Pipeline, RagConfig, Result};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Parser)]
#[command(
    name = "docling-rag",
    about = "Pluggable RAG over the docling.rs document converter"
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

    /// Retrieve for a query and answer with the LLM (falls back to listing the
    /// retrieved chunks when no OPENROUTER_API_KEY is configured).
    Query {
        /// The question / search query.
        query: String,
        /// Retrieval mode (vector|bm25|hybrid|multi-query|hyde).
        #[arg(long)]
        mode: Option<String>,
        /// Number of results.
        #[arg(long, short = 'k')]
        top_k: Option<usize>,
        /// List only the retrieved chunks; skip LLM answer synthesis.
        #[arg(long)]
        chunks: bool,
    },

    /// Sweep chunk sizes / overlaps / modes over a labelled dataset and rank
    /// the configurations by retrieval quality (recall@k / MRR / nDCG@k).
    Eval {
        /// Path to a self-contained JSON dataset ({documents, queries}).
        #[arg(long, conflicts_with_all = ["from_md_dir", "questions"])]
        dataset: Option<PathBuf>,
        /// Build the documents from a directory of .md files (e.g. the
        /// RAG_DOCUMENTS_OUTPUT mirror). Requires --questions.
        #[arg(long, requires = "questions")]
        from_md_dir: Option<PathBuf>,
        /// Questions JSON: [{"query"|"text", "relevant": [..], "kind"?}, …].
        /// Entries without `relevant` labels are skipped (retrieval eval needs
        /// ground truth; see `answers` for unlabelled QA benchmarks).
        #[arg(long, requires = "from_md_dir")]
        questions: Option<PathBuf>,
        /// Chunk sizes to sweep, comma-separated (e.g. 200,300,500).
        #[arg(long, value_delimiter = ',')]
        sizes: Vec<usize>,
        /// Overlap fractions to sweep, comma-separated (e.g. 0,0.05,0.1).
        #[arg(long, value_delimiter = ',')]
        overlaps: Vec<f32>,
        /// Results per query.
        #[arg(long, default_value_t = 5)]
        top_k: usize,
        /// Emit JSON instead of a Markdown table.
        #[arg(long)]
        json: bool,
    },

    /// Run an unlabelled QA benchmark ({"text", "kind"} questions) through the
    /// full RAG + LLM loop against the configured store; prints each answer
    /// with its latency. Needs an ingested store and OPENROUTER_API_KEY.
    Answers {
        /// Questions JSON: [{"text"|"query": "...", "kind"?: "..."}, …].
        #[arg(long)]
        questions: PathBuf,
        /// Retrieval mode (vector|bm25|hybrid|multi-query|hyde).
        #[arg(long)]
        mode: Option<String>,
        /// Passages retrieved per question.
        #[arg(long, short = 'k')]
        top_k: Option<usize>,
        /// Emit JSON instead of Markdown.
        #[arg(long)]
        json: bool,
    },

    /// Remove incomplete document records (interrupted ingests: rows without
    /// processing metrics or with a pending hash).
    Prune,

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
    // Default filter: our own logs at info, but silence lopdf's per-stream
    // warnings ("corrupt deflate stream", …) — lopdf skips such streams and
    // continues, and real-world PDFs trigger them constantly. RUST_LOG overrides.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,lopdf=error")),
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
            chunks,
        } => {
            let mode = match mode {
                Some(m) => RetrievalMode::from_str(&m)?,
                None => cfg.retrieval_mode,
            };
            let k = top_k.unwrap_or(cfg.top_k);
            let have_llm = cfg.openrouter_api_key.is_some();
            let pipeline = Pipeline::from_config(&cfg).await?;
            if !chunks && have_llm {
                // Default: synthesize a grounded answer, then show the sources.
                let a = pipeline.answer(&query, mode, k).await?;
                println!("{}\n", a.text.trim());
                println!("— sources ({} passage(s), mode: {mode}) —", a.sources.len());
                for (i, h) in a.sources.iter().enumerate() {
                    let preview = h.chunk.text.replace('\n', " ");
                    let preview: String = preview.chars().take(120).collect();
                    println!("{:>2}. [{:.4}] {}", i + 1, h.score, preview);
                }
            } else {
                if !chunks {
                    eprintln!(
                        "note: no OPENROUTER_API_KEY configured — listing retrieved \
                         chunks only (set the key to get an LLM answer)"
                    );
                }
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
            from_md_dir,
            questions,
            sizes,
            overlaps,
            top_k,
            json,
        } => {
            // Assemble the dataset from either a self-contained file or an
            // .md directory + questions file.
            let ds: EvalDataset = match (dataset, from_md_dir, questions) {
                (Some(path), _, _) => serde_json::from_str(&std::fs::read_to_string(&path)?)?,
                (None, Some(md_dir), Some(qpath)) => {
                    let documents = docling_rag::eval::documents_from_md_dir(&md_dir)?;
                    let all = docling_rag::eval::load_questions(&qpath)?;
                    let total = all.len();
                    let queries: Vec<_> = all
                        .into_iter()
                        .filter(|q| !q.relevant.is_empty())
                        .map(|q| docling_rag::eval::QueryCase {
                            query: q.query,
                            relevant: q.relevant,
                        })
                        .collect();
                    if queries.len() < total {
                        eprintln!(
                            "note: skipped {} question(s) without `relevant` labels \
                             (retrieval eval needs ground truth; use `answers` for those)",
                            total - queries.len()
                        );
                    }
                    if queries.is_empty() {
                        return Err(docling_rag::RagError::config(
                            "no questions carry `relevant` labels — retrieval eval needs \
                             ground truth. Add `\"relevant\": [\"verbatim snippet from the \
                             source document\", …]` to each question (the `answers` output \
                             and its citations help find the right passages), or use \
                             `answers` to just run the benchmark",
                        ));
                    }
                    EvalDataset { documents, queries }
                }
                _ => {
                    return Err(docling_rag::RagError::config(
                        "pass --dataset FILE, or --from-md-dir DIR --questions FILE",
                    ))
                }
            };

            // Chunk-config matrix: default pairs, or the cross product of the
            // provided --sizes / --overlaps lists.
            let chunk_configs: Vec<(usize, f32)> = if sizes.is_empty() && overlaps.is_empty() {
                vec![(200, 0.0), (300, 0.05), (500, 0.1)]
            } else {
                let sizes = if sizes.is_empty() { vec![300] } else { sizes };
                let overlaps = if overlaps.is_empty() {
                    vec![0.05]
                } else {
                    overlaps
                };
                sizes
                    .iter()
                    .flat_map(|&s| overlaps.iter().map(move |&o| (s, o)))
                    .collect()
            };

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
            let harness = Harness::new(embedder, chat);
            let report = harness.run(&ds, &chunk_configs, &modes, top_k).await?;
            if json {
                println!("{}", report.to_json());
            } else {
                println!("{}", report.to_markdown());
            }
        }

        Cmd::Answers {
            questions,
            mode,
            top_k,
            json,
        } => {
            let mode = match mode {
                Some(m) => RetrievalMode::from_str(&m)?,
                None => cfg.retrieval_mode,
            };
            let k = top_k.unwrap_or(cfg.top_k);
            if cfg.openrouter_api_key.is_none() {
                return Err(docling_rag::RagError::config(
                    "`answers` needs an LLM: set OPENROUTER_API_KEY (see .env.example)",
                ));
            }
            let pipeline = Pipeline::from_config(&cfg).await?;
            if pipeline.store().count_chunks().await? == 0 {
                return Err(docling_rag::RagError::config(
                    "the store is empty — run `ingest` first",
                ));
            }
            let qs = docling_rag::eval::load_questions(&questions)?;
            let mut rows = Vec::new();
            for (i, q) in qs.iter().enumerate() {
                let start = std::time::Instant::now();
                let result = pipeline.answer(&q.query, mode, k).await;
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                match result {
                    Ok(a) => {
                        if !json {
                            println!(
                                "## Q{} [{}] {}\n\n{}\n\n({} source(s), {:.0} ms, mode: {mode})\n",
                                i + 1,
                                q.kind.as_deref().unwrap_or("-"),
                                q.query,
                                a.text.trim(),
                                a.sources.len(),
                                ms
                            );
                        }
                        rows.push(serde_json::json!({
                            "question": q.query,
                            "kind": q.kind,
                            "answer": a.text,
                            "sources": a.sources.len(),
                            "ms": ms,
                            "mode": mode.to_string(),
                        }));
                    }
                    Err(e) => {
                        if !json {
                            eprintln!("## Q{} {}\n\nERROR: {e}\n", i + 1, q.query);
                        }
                        rows.push(serde_json::json!({
                            "question": q.query,
                            "kind": q.kind,
                            "error": e.to_string(),
                            "ms": ms,
                        }));
                    }
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            }
        }

        Cmd::Prune => {
            let pipeline = Pipeline::from_config(&cfg).await?;
            let docs = pipeline.store().list_documents().await?;
            let mut removed = 0;
            for d in &docs {
                let incomplete = d.hash.starts_with("pending:")
                    || d.metadata.get("metrics").is_none_or(|m| m.is_null());
                if incomplete {
                    pipeline.store().delete_document(&d.id).await?;
                    println!(
                        "removed incomplete document: {} ({})",
                        d.title, d.source_uri
                    );
                    removed += 1;
                }
            }
            println!("pruned {removed} incomplete document(s) of {}", docs.len());
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
            docling_rag::api::serve(pipeline, &bind, cfg.api_keys.clone()).await?;
        }

        Cmd::Stats => {
            let pipeline = Pipeline::from_config(&cfg).await?;
            let docs = pipeline.store().count_documents().await?;
            let chunks = pipeline.store().count_chunks().await?;
            println!("documents: {docs}\nchunks:    {chunks}\n");
            let documents = pipeline.store().list_documents().await?;
            if !documents.is_empty() {
                // Fixed column widths so rows line up in the terminal.
                const TITLE_W: usize = 65;
                println!(
                    "| {:<TITLE_W$} | {:>9} | {:>5} | {:>7} | {:>6} | {:>9} | {:>9} | {:>10} | {:>9} |",
                    "title", "KiB", "pages", "words", "chunks", "parse w/s", "parse p/s", "chunk w/s", "embed w/s"
                );
                println!(
                    "|{:-<w$}|{:->11}|{:->7}|{:->9}|{:->8}|{:->11}|{:->11}|{:->12}|{:->11}|",
                    "",
                    ":",
                    ":",
                    ":",
                    ":",
                    ":",
                    ":",
                    ":",
                    ":",
                    w = TITLE_W + 2
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
                        "| {:<TITLE_W$} | {:>9} | {:>5} | {:>7} | {:>6} | {:>9} | {:>9} | {:>10} | {:>9} |",
                        ellipsize(&d.title, TITLE_W),
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

/// Truncate to `width` chars with a `...` tail (char-safe, no mid-UTF-8 cuts).
fn ellipsize(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let cut: String = s.chars().take(width.saturating_sub(3)).collect();
        format!("{cut}...")
    }
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
        other => Err(docling_rag::RagError::config(format!(
            "unknown source '{other}'"
        ))),
    }
}
