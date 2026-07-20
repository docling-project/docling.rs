# docling-rag

A pluggable **Retrieval-Augmented-Generation** subsystem built on the
[`docling.rs`](../docling) document converter.

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
cargo run -p docling-rag -- ingest

# 3. search from the CLI — answers with the LLM when OPENROUTER_API_KEY is set,
#    otherwise lists the retrieved chunks (--chunks forces the list)
cargo run -p docling-rag -- query "what did I write about budgets?" --mode hybrid
cargo run -p docling-rag -- stats

# 4. ... or serve the REST API (ingests first thanks to --ingest)
cargo run -p docling-rag -- serve --ingest
curl -H 'X-Api-Key: dev-key' 'http://127.0.0.1:8080/api/search?q=budgets&mode=hybrid&k=5'
```

For real semantic quality, install [Ollama](https://ollama.com), run
`ollama pull bge-m3`, and drop the `RAG_EMBED_PROVIDER=hash` override — Ollama +
`bge-m3` (1024-dim) is the default. Re-ingest after switching embedders (vectors
must all come from the same model): `rm -rf data/ && cargo run -p docling-rag -- ingest`.

¹ PDF/image inputs additionally need the pdfium + ONNX models — see
["Getting the ML models"](../../README.md#getting-the-ml-models) in the root README.

## REST API

`docling-rag serve` exposes the store over HTTP. Authentication uses a static
API-key list from config (`RAG_API_KEYS`, comma-separated); send the key as
`X-Api-Key: <key>` or `Authorization: Bearer <key>`. The server refuses to start
with an empty key list; `GET /` (the built-in UI) and `GET /health` are the
only public routes.

**Built-in web UI** — `GET /` serves a single self-contained page (embedded in
the binary, no external assets): query box, retrieval-mode and top-k pickers,
an LLM-answer toggle, scored results, a live document/chunk counter from
`/api/stats`, and a documents panel — upload a file (converted, chunked and
embedded through the full ingest pipeline) or delete one with its chunks. The
API key is entered once and kept in the browser's `localStorage`; every
request the page makes carries it as `X-Api-Key`. The page itself holds no
data, which is why it can be public like `/health`.

| Method | Path                  | Description                                     |
|--------|-----------------------|-------------------------------------------------|
| GET    | `/`                   | built-in search UI (no auth; static HTML)       |
| GET    | `/health`             | liveness probe (no auth)                        |
| GET    | `/api/stats`          | document / chunk counts                         |
| GET    | `/api/documents`      | all documents with metadata + processing metrics |
| POST   | `/api/documents`      | `?name=file.pdf`, raw file bytes as the body → full ingest (convert, chunk, embed); dedups identical content |
| GET    | `/api/documents/{id}` | one document by id                              |
| DELETE | `/api/documents/{id}` | remove the document and all its chunks          |
| GET    | `/api/search`         | `?q=…&mode=…&k=…` — mode: `vector`, `bm25`, `hybrid`, `multi-query`, `hyde` |
| POST   | `/api/search`         | `{"query": "…", "mode": "hybrid", "top_k": 5, "answer": false}` |

With `"answer": true` (or `?answer=true`) the LLM synthesizes a grounded answer
from the retrieved chunks (needs `OPENROUTER_API_KEY`; `multi-query`/`hyde` modes
need it too). The LLM client speaks the OpenAI-compatible `/chat/completions`
protocol, so any such endpoint works — e.g. a native DeepSeek key with
`OPENROUTER_BASE_URL=https://api.deepseek.com` and `RAG_LLM_MODEL=deepseek-chat`
(OpenRouter keys start with `sk-or-`; a `sk-…` DeepSeek key sent to openrouter.ai
gets 401). Responses are JSON: `{query, mode, results: [{score, chunk}], answer?}`.

```bash
curl -s -H 'X-Api-Key: dev-key' \
  -H 'Content-Type: application/json' \
  -d '{"query": "how does hybrid search work?", "mode": "multi-query", "answer": true}' \
  http://127.0.0.1:8080/api/search
```

## Features

| Concern      | Trait            | Backends                                                        |
|--------------|------------------|-----------------------------------------------------------------|
| Chunking     | `Chunker`        | **window** (Markdown sliding window, size 300 + 5% overlap, streaming; default), docling's `hierarchical` / `hybrid` (`RAG_CHUNKER`) |
| Embeddings   | `Embedder`       | **Ollama** (default, bge-m3, 1024-d), Gemini, local ONNX, hash |
| Vector store | `VectorStore`    | **SQLite + sqlite-vec** (default), PostgreSQL + pgvector, in-memory |
| Retrieval    | `Retriever`      | vector, BM25, **Hybrid** (RRF), Multi-Query fusion, HyDE       |
| LLM          | `ChatModel`      | OpenRouter (default model DeepSeek-V3)                          |
| Sources      | `DocumentSource` | **folder** (default), FTP, SFTP                                 |
| Queues       | `MessageQueue`   | **in-process** (default), RabbitMQ, Redis pub/sub              |

`RAG_CHUNKER` selects the chunking strategy. The default `window` slides a
fixed-size window (`RAG_CHUNK_SIZE` / `RAG_CHUNK_OVERLAP`) over the converted
Markdown and never crosses a heading; it chunks **streaming**, overlapping
page conversion with embedding. `hierarchical` and `hybrid` run docling's
structure-aware chunkers (`docling::chunker`) over the document tree instead —
one chunk per document item with its heading path, tables triplet-serialized;
`hybrid` additionally splits/merges against a real token budget
(`RAG_CHUNK_SIZE` tokens counted by a HuggingFace `tokenizer.json` —
`RAG_CHUNK_TOKENIZER`, or `models/chunk/tokenizer.json` as fetched by
`scripts/install/download_dependencies.sh`). These two need the complete
document tree, so conversion is whole-document — but their chunks **stream**
into embedding as the chunkers produce them, overlapping chunking with
embedding like the `window` path. They have no overlap concept
(`RAG_CHUNK_OVERLAP` applies to `window` only, matching docling's own
chunkers), and put the heading path and source item refs in each chunk's
metadata.

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

`docling-rag stats` prints these as a per-document table.

## Configuration

All settings come from the environment (or a `.env` file). See
[`.env.example`](../../.env.example) at the repo root for the full list with
defaults. Nothing is required — an empty environment runs SQLite + Ollama.

## CLI

```bash
# create the schema
docling-rag init-db

# ingest a folder of documents (any format docling.rs can convert)
RAG_SOURCE_PATH=./docs docling-rag ingest

# retrieve
docling-rag query "how does hybrid search work?" --mode hybrid -k 5

# retrieve + synthesize an answer (needs OPENROUTER_API_KEY)
docling-rag query "how does hybrid search work?" --answer

# sweep chunk sizes / overlaps / modes over a labelled dataset and rank them
docling-rag eval --dataset crates/docling-rag/tests/data/eval_dataset.json

# run an unlabelled QA benchmark through the full RAG+LLM loop (see "eval" below)
docling-rag answers --questions rag/questions.json

# remove incomplete document records left by interrupted ingests
docling-rag prune

# serve the REST API (optionally ingesting first); see "REST API" above
docling-rag serve --ingest --addr 0.0.0.0:8080
```

## Evaluating configurations (`eval`)

`eval` sweeps a matrix of chunk sizes / overlaps × retrieval modes over a
labelled dataset, builds a fresh in-memory index per chunk config with the
**configured embedder** (`RAG_EMBED_PROVIDER` — run with Ollama up for real
numbers, `hash` for a quick offline smoke), and prints the configurations
ranked best-first:

```
| chunk | overlap | mode   | embedder | recall | MRR   | nDCG  | ms/query |
|------:|--------:|--------|----------|-------:|------:|------:|---------:|
| 200   | 0.00    | bm25   | hash:512 | 1.000  | 1.000 | 1.000 | 0.30     |
| 300   | 0.05    | hybrid | hash:512 | 1.000  | 0.800 | 0.852 | 0.48     |
```

- **recall@k** — fraction of the expected evidence found in the top-k
- **MRR** — how high the first relevant chunk ranks (1.0 = always first)
- **nDCG@k** — ranking quality across all relevant hits

Without `OPENROUTER_API_KEY` the sweep covers the three offline modes
(vector/bm25/hybrid); with a key it also evaluates `multi-query` and `hyde`.

Two ways to provide the dataset:

```bash
# 1. a self-contained file
docling-rag eval --dataset crates/docling-rag/tests/data/eval_dataset.json

# 2. assemble it from the ingested corpus: the .md mirror written by
#    RAG_DOCUMENTS_OUTPUT plus a questions file
docling-rag eval --from-md-dir ./data --questions rag/questions.json \
    --sizes 200,300,500 --overlaps 0,0.05,0.1 --top-k 5   # optional matrix override
```

Dataset / questions format — a retrieved chunk counts as relevant if it
contains any `relevant` substring (case-insensitive; substrings are used
because chunk ids change with every chunking config):

```json
{ "documents": [ { "name": "report", "markdown": "# …" } ],
  "queries":   [ { "query": "default chunk size?", "relevant": ["300 words"] } ] }
```

The questions file is a plain array and also accepts the QA-benchmark shape
`{"text": "…", "kind": "boolean|number|name"}`; entries **without** `relevant`
labels are skipped by `eval` (retrieval scoring needs ground truth) — run those
through `answers` instead:

```bash
# Full RAG + LLM loop against the *ingested store* for unlabelled questions:
# prints each answer with its source count and latency (add --json for machines).
docling-rag answers --questions rag/questions.json --mode hybrid -k 5
```

`answers` needs `OPENROUTER_API_KEY` and an ingested store; it does not score
correctness (the `{text, kind}` format carries no gold answers) — it automates
running the benchmark so you can review answers and compare latency across
modes.

The two commands round-trip: `answers --json` output is itself a valid
questions file (the `question` field is accepted; `answer`/`ms`/… are ignored).
The intended workflow is to review those answers, add a
`"relevant": ["verbatim snippet from the source document", …]` array to each
entry (the answer text and its `[n]` citations point at the right passages),
and feed the annotated file back to `eval` to score retrieval configurations.

## Maintenance

`docling-rag prune` removes incomplete document records — rows left behind
when an ingest was interrupted (killed mid-run), recognizable by their missing
processing metrics. Ingestion is also self-healing: a document's content hash
is only written after a fully successful ingest, and re-ingesting a source
replaces any stale rows for the same URI.

## Library

```rust
use docling_rag::{RagConfig, Pipeline, RetrievalMode};

# async fn run() -> docling_rag::Result<()> {
let cfg = RagConfig::from_env()?;
let pipeline = Pipeline::from_config(&cfg).await?;
pipeline.ingest_all().await?;
let hits = pipeline.query(RetrievalMode::Hybrid, "how does chunking work?", 5).await?;
# Ok(()) }
```

## Cargo features

`default = ["sqlite", "ollama"]`. Optional: `postgres`, `onnx-embed`,
`remote-sources` (FTP/SFTP), `rabbitmq`, `redis`, `gemini`, `openrouter`,
and the GPU execution providers `cuda` / `tensorrt` / `directml` / `coreml`.

### GPU

`--features cuda` puts the whole ingest-and-search path on the GPU where it
counts: document conversion (the PDF/image ML pipeline — 1.5–8.7× measured,
see `docs/PDF_CONFORMANCE.md`) and, when `onnx-embed` is also enabled, the
local embedder's ONNX session — both route through the same `DOCLING_RS_EP`
selection as the rest of the stack, so one switch covers everything and a
build without a usable GPU falls back to CPU per session:

```bash
cargo build --release -p docling-rag --features cuda,onnx-embed
DOCLING_RS_EP=cuda target/release/docling-rag ingest   # pin GPU (or auto/cpu)
```

HTTP embedders (Ollama/Gemini) are unaffected — their GPU usage is the
serving side's business. Runtime requirements match the CLI's CUDA build:
CUDA 12 + cuDNN 9, glibc ≥ 2.38 for the static ONNX Runtime link.

The default feature set is fully self-contained and offline-testable
(`cargo test -p docling-rag`) using the bundled SQLite store and the
deterministic hashing embedder. The SQLite backend statically compiles the
[sqlite-vec](https://github.com/asg017/sqlite-vec) extension: embeddings live in
a `vec0` virtual table (cosine metric) and `vector_search` is a KNN `MATCH`
query, not a full-table scan. The other backends are real client
implementations that require the corresponding service (Postgres, an AMQP broker,
Redis, an FTP/SSH server, an Ollama/Gemini endpoint, or local ONNX model files) to
exercise end-to-end.

## Local embedding model (ONNX)

Build with `--features onnx-embed` and fetch the default model (bge-m3,
1024-d, ~2.3 GB) into the `RAG_EMBED_ONNX_PATH` / `RAG_EMBED_TOKENIZER`
default paths:

```bash
scripts/install/download_dependencies.sh --embed
RAG_EMBED_PROVIDER=onnx docling-rag ingest
```

The embedder adapts to the graph it loads: it feeds only the inputs the model
declares (`token_type_ids` is optional), and uses either an already-pooled
sentence embedding (bge-m3's `dense_vecs`) or a raw encoder's
`last_hidden_state` mean-pooled over the attention mask — L2-normalized either
way. So any sentence-embedding encoder exported to ONNX works; point the two
env vars at it. Same `ort` runtime the PDF pipeline already depends on, and
with `--features cuda` the session runs on the GPU (`DOCLING_RS_EP`).
