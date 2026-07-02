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
