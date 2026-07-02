# fleischwolf-rag

A pluggable **Retrieval-Augmented-Generation** subsystem built on the
[`fleischwolf`](../fleischwolf) document converter.

Pipeline:

```
source → convert to Markdown → chunk → embed → vector store → retrieve → (answer)
```

Every external dependency is a trait with swappable backends, so you can mix and
match a database, embedder, LLM, document source, and message queue without
touching the pipeline.

## Quickstart: index a folder and search it

No services needed — the deterministic `hash` embedder and bundled SQLite let you
try the whole pipeline on any folder of documents (Markdown, PDF¹, DOCX, HTML, …)
in a minute:

```bash
cp .env.example .env         # optional; every key has a default

# 1. point at any documents folder, pick the offline embedder, set an API key
export RAG_SOURCE_PATH=~/Documents/notes
export RAG_EMBED_PROVIDER=hash          # offline; see below for real embeddings
export RAG_API_KEYS=dev-key

# 2. ingest: convert -> chunk -> embed -> store (SQLite at data/rag.db)
cargo run -p fleischwolf-rag -- ingest

# 3. search from the CLI ...
cargo run -p fleischwolf-rag -- query "what did I write about budgets?" --mode hybrid
cargo run -p fleischwolf-rag -- stats

# 4. ... or serve the REST API (ingests first thanks to --ingest)
cargo run -p fleischwolf-rag -- serve --ingest
curl -H 'X-Api-Key: dev-key' 'http://127.0.0.1:8080/api/search?q=budgets&mode=hybrid&k=5'
```

For real semantic quality, install [Ollama](https://ollama.com), run
`ollama pull bge-m3`, and drop the `RAG_EMBED_PROVIDER=hash` override — Ollama +
`bge-m3` (1024-dim) is the default. Re-ingest after switching embedders (vectors
must all come from the same model): `rm -rf data/ && cargo run -p fleischwolf-rag -- ingest`.

¹ PDF/image inputs additionally need the pdfium + ONNX models — see
["Getting the ML models"](../../README.md#getting-the-ml-models) in the root README.

## REST API

`fleischwolf-rag serve` exposes the store over HTTP. Authentication uses a static
API-key list from config (`RAG_API_KEYS`, comma-separated); send the key as
`X-Api-Key: <key>` or `Authorization: Bearer <key>`. The server refuses to start
with an empty key list; `GET /health` is the only public route.

| Method | Path                  | Description                                     |
|--------|-----------------------|-------------------------------------------------|
| GET    | `/health`             | liveness probe (no auth)                        |
| GET    | `/api/stats`          | document / chunk counts                         |
| GET    | `/api/documents`      | all documents with metadata + processing metrics |
| GET    | `/api/documents/{id}` | one document by id                              |
| GET    | `/api/search`         | `?q=…&mode=…&k=…` — mode: `vector`, `bm25`, `hybrid`, `multi-query`, `hyde` |
| POST   | `/api/search`         | `{"query": "…", "mode": "hybrid", "top_k": 5, "answer": false}` |

With `"answer": true` (or `?answer=true`) the LLM synthesizes a grounded answer
from the retrieved chunks (needs `OPENROUTER_API_KEY`; `multi-query`/`hyde` modes
need it too). Responses are JSON: `{query, mode, results: [{score, chunk}], answer?}`.

```bash
curl -s -H 'X-Api-Key: dev-key' \
  -H 'Content-Type: application/json' \
  -d '{"query": "how does hybrid search work?", "mode": "multi-query", "answer": true}' \
  http://127.0.0.1:8080/api/search
```

## Features

| Concern      | Trait            | Backends                                                        |
|--------------|------------------|-----------------------------------------------------------------|
| Chunking     | `Chunker`        | Markdown-aware, configurable size (300) + overlap (5%)          |
| Embeddings   | `Embedder`       | **Ollama** (default, bge-m3, 1024-d), Gemini, local ONNX, hash |
| Vector store | `VectorStore`    | **SQLite + sqlite-vec** (default), PostgreSQL + pgvector, in-memory |
| Retrieval    | `Retriever`      | vector, BM25, **Hybrid** (RRF), Multi-Query fusion, HyDE       |
| LLM          | `ChatModel`      | OpenRouter (default model DeepSeek-V3)                          |
| Sources      | `DocumentSource` | **folder** (default), FTP, SFTP                                 |
| Queues       | `MessageQueue`   | **in-process** (default), RabbitMQ, Redis pub/sub              |

Documents (metadata) and chunks (text + embedding) are stored in **two separate
tables**.

### Advanced retrieval

- **Hybrid** — fuses dense vector search with sparse BM25 via Reciprocal Rank Fusion.
- **Multi-Query (fusion)** — the LLM rewrites the question into several diverse
  queries; each is retrieved and the results are fused with RRF.
- **HyDE** — the LLM writes a hypothetical answer whose embedding drives the search.

## Processing metrics

Every ingested document records per-phase processing metrics in its JSON
`metadata` column (under `"metrics"`), so new metrics can be added later without
a schema migration: source file size, page count (PDF pages, PPTX slides, XLSX
sheets), word count, chunk count, and per-phase `seconds` / `words_per_sec` for
**parsing**, **chunking** and **embedding** — plus `pages_per_sec` for parsing
when the format has pages.

```json
"metrics": {
  "file_bytes": 170934, "pages": 4, "words": 311, "chunks": 2, "embedded_words": 316,
  "parsing":   { "seconds": 0.006, "words_per_sec": 51262.6, "pages_per_sec": 659.3 },
  "chunking":  { "seconds": 0.0,   "words_per_sec": 1119506.4 },
  "embedding": { "seconds": 0.0,   "words_per_sec": 1730381.9 }
}
```

`fleischwolf-rag stats` prints these as a per-document table.

## Configuration

All settings come from the environment (or a `.env` file). See
[`.env.example`](../../.env.example) at the repo root for the full list with
defaults. Nothing is required — an empty environment runs SQLite + Ollama.

## CLI

```bash
# create the schema
fleischwolf-rag init-db

# ingest a folder of documents (any format fleischwolf can convert)
RAG_SOURCE_PATH=./docs fleischwolf-rag ingest

# retrieve
fleischwolf-rag query "how does hybrid search work?" --mode hybrid -k 5

# retrieve + synthesize an answer (needs OPENROUTER_API_KEY)
fleischwolf-rag query "how does hybrid search work?" --answer

# sweep chunk sizes / overlaps / modes over a labelled dataset and rank them
fleischwolf-rag eval --dataset crates/fleischwolf-rag/tests/data/eval_dataset.json

# serve the REST API (optionally ingesting first); see "REST API" above
fleischwolf-rag serve --ingest --addr 0.0.0.0:8080
```

The `eval` sweep prints a ranked table:

```
| chunk | overlap | mode   | embedder | recall | MRR   | nDCG  | ms/query |
|------:|--------:|--------|----------|-------:|------:|------:|---------:|
| 200   | 0.00    | bm25   | hash:512 | 1.000  | 1.000 | 1.000 | 0.30     |
| 300   | 0.05    | hybrid | hash:512 | 1.000  | 0.800 | 0.852 | 0.48     |
| ...   |         |        |          |        |       |       |          |
```

## Library

```rust
use fleischwolf_rag::{RagConfig, Pipeline, RetrievalMode};

# async fn run() -> fleischwolf_rag::Result<()> {
let cfg = RagConfig::from_env()?;
let pipeline = Pipeline::from_config(&cfg).await?;
pipeline.ingest_all().await?;
let hits = pipeline.query(RetrievalMode::Hybrid, "how does chunking work?", 5).await?;
# Ok(()) }
```

## Cargo features

`default = ["sqlite", "ollama"]`. Optional: `postgres`, `onnx-embed`,
`remote-sources` (FTP/SFTP), `rabbitmq`, `redis`, `gemini`, `openrouter`.

The default feature set is fully self-contained and offline-testable
(`cargo test -p fleischwolf-rag`) using the bundled SQLite store and the
deterministic hashing embedder. The SQLite backend statically compiles the
[sqlite-vec](https://github.com/asg017/sqlite-vec) extension: embeddings live in
a `vec0` virtual table (cosine metric) and `vector_search` is a KNN `MATCH`
query, not a full-table scan. The other backends are real client
implementations that require the corresponding service (Postgres, an AMQP broker,
Redis, an FTP/SSH server, an Ollama/Gemini endpoint, or local ONNX model files) to
exercise end-to-end.

## Local embedding model (ONNX)

Build with `--features onnx-embed` and point `RAG_EMBED_ONNX_PATH` /
`RAG_EMBED_TOKENIZER` at a sentence-embedding encoder exported to ONNX (e.g.
`bge-m3`, 1024-d). The mean-pooled, L2-normalized last hidden state is used as the
embedding — the same `ort` runtime the PDF pipeline already depends on.
