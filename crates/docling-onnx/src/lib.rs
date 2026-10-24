//! Execution-provider selection for the ONNX sessions (#74, #288).
//!
//! Shared by every crate that owns `ort` sessions (docling-pdf's layout/
//! TableFormer/OCR/enrichment models, docling-asr's Whisper encoder/decoder,
//! docling-rag's embedder) so a single env var switches the whole process.
//!
//! CPU is the default and the only provider in a default build — the GPU
//! providers exist behind cargo features (`cuda`, `tensorrt`, `directml`,
//! `coreml`; plus the CPU-class `xnnpack`, #324) so the standard build keeps
//! zero GPU dependencies. A feature only
//! *compiles* a provider in (it makes `ort` link/download an ONNX Runtime
//! binary that contains that EP); which provider actually runs is chosen at
//! startup from `DOCLING_RS_EP`:
//!
//! * unset — `auto` in a build that compiled a CUDA-class provider (or
//!   XNNPACK) in (you chose a GPU build or installed the GPU wheel: use the
//!   GPU when one is usable, fall back to CPU when not); plain CPU in a
//!   default build. CoreML is opt-in (#602): a `coreml` build stays on CPU,
//!   and that `auto` leaves CoreML out, until `DOCLING_RS_EP=coreml` (or
//!   `auto`, by name) asks for it
//! * `cpu` — force CPU, exactly the pre-#74 behavior
//! * `cuda` / `tensorrt` (`trt`) / `directml` (`dml`) / `coreml` /
//!   `xnnpack` — that provider, registered with error-on-failure: an *explicitly requested*
//!   accelerator that can't initialize (missing driver, no device) fails the
//!   session load loudly instead of silently degrading to a CPU run that
//!   looks fine but is 10× slower than expected. Requesting a provider the
//!   binary wasn't compiled with warns once and stays on CPU (there is
//!   nothing to register at all in that case).
//! * `auto` — every compiled-in provider is registered in performance order
//!   (TensorRT, CUDA, CoreML, DirectML, then XNNPACK) and ONNX Runtime falls
//!   back down the list — ultimately to CPU — at session creation. The "try GPU if there is
//!   one" mode for images built once and deployed on mixed fleets. CoreML
//!   joins the list only when `auto` was set by name, not as the default.
//!
//! CoreML registers with the `NeuralNetwork` model format by default (#602):
//! measured on Apple silicon it reproduces the CPU provider's output byte
//! for byte, while `MLProgram` (the #324 default, ~1.8× faster on the GPU)
//! changes the layout detections — `DOCLING_RS_COREML_FORMAT=mlprogram`
//! opts into it, with a notice. Two safety defaults ride along (issue-#324
//! testing, M4 Max): only *static-shaped* partitions are handed to CoreML
//! (`DOCLING_RS_COREML_STATIC_SHAPES=0` opts out) — dynamic partitions under
//! MLProgram fail an MPSGraph assertion as an uncatchable SIGABRT — and
//! compute units default to `cpu_and_gpu` rather than `all`
//! (`DOCLING_RS_COREML_UNITS`: `all`|`cpu_and_gpu`|`cpu_and_ne`|`cpu_only`),
//! since the fp16 Neural Engine silently corrupts this model's logits.
//! Static partitions need static graphs, so CoreML also keeps docling-pdf's
//! layout per-page ([`prefers_batching`]): a batched session leaves the batch
//! axis free and CoreML is handed nothing.
//! `DOCLING_RS_XNNPACK_THREADS` sizes XNNPACK's own thread pool.
//!
//! The int8 model defaults in docling-pdf are skipped whenever a GPU provider
//! is selected ([`prefers_fp32`]): the int8 exports are QDQ graphs calibrated
//! for CPU kernels — on GPU they only add de-quantize traffic and were never
//! conformance-validated there, while fp32 is (see docs/PDF_CONFORMANCE.md).

// glibc 2.32/2.38 symbols the prebuilt static ONNX Runtime references,
// defined here so the manylinux_2_28 wheels import on glibc < 2.38 (#745).
#[cfg(all(target_os = "linux", target_env = "gnu", not(feature = "load-dynamic")))]
mod glibc_compat;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use ort::ep::ExecutionProviderDispatch;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::session::Session;

/// The file name ONNX Runtime's shared library carries on this platform —
/// what `ort` itself opens when `ORT_DYLIB_PATH` is unset.
#[cfg(feature = "load-dynamic")]
const RUNTIME_SONAME: &str = if cfg!(target_os = "windows") {
    "onnxruntime.dll"
} else if cfg!(any(target_os = "macos", target_os = "ios")) {
    "libonnxruntime.dylib"
} else {
    "libonnxruntime.so"
};

/// The ONNX Runtime shared library this process loaded (the `load-dynamic`
/// feature, #504), resolved and opened once at the first session:
///
/// 1. `ORT_DYLIB_PATH` — `ort`'s own override, a path to the library;
/// 2. `.models/onnxruntime/libonnxruntime.so` (`.dylib` / `onnxruntime.dll`)
///    through the shared asset chain (`docling_core::assets::resolve`: CWD,
///    `DOCLING_RS_MODELS_DIR`, the executable's directory) — where a
///    self-built runtime for a target without prebuilt binaries is dropped
///    next to the models;
/// 3. the bare soname, i.e. the dynamic linker's search path (a distro
///    package, `LD_LIBRARY_PATH`).
///
/// `Err` names what was tried: a library that is missing, unloadable or
/// older than the API this build was compiled against (ONNX Runtime 1.17+).
/// Callers treat it like a missing model — the stage warns and degrades, a
/// conversion that cannot proceed without it fails with this message, and
/// nothing outside the ML stages ever asks.
#[cfg(feature = "load-dynamic")]
pub fn runtime_library() -> Result<&'static Path, String> {
    static LIB: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        let candidate = if let Some(p) = docling_core::env::nonempty("ORT_DYLIB_PATH") {
            PathBuf::from(p)
        } else {
            let bundled =
                docling_core::assets::resolve(&format!(".models/onnxruntime/{RUNTIME_SONAME}"));
            if Path::new(&bundled).is_file() {
                PathBuf::from(bundled)
            } else {
                PathBuf::from(RUNTIME_SONAME)
            }
        };
        match ort::init_from(&candidate) {
            Ok(env) => {
                // The library is loaded by `init_from`; the environment
                // itself is built once by [`environment`].
                drop(env);
                docling_core::debug_log!(
                    "docling-onnx: ONNX Runtime loaded from {}",
                    candidate.display()
                );
                Ok(candidate)
            }
            // `ort`'s error already names the path it tried.
            Err(e) => Err(format!(
                "ONNX Runtime library unavailable ({e}) — set ORT_DYLIB_PATH to a libonnxruntime \
                 (1.17 or newer) built for this platform, or place it under .models/onnxruntime/; \
                 the ML stages (PDF layout/tables/OCR, ASR, the embedder) need it, every other \
                 format converts without"
            )),
        }
    })
    .as_deref()
    .map_err(Clone::clone)
}

/// A fresh [`SessionBuilder`] — the one entry point every session in the
/// workspace starts from. In a `load-dynamic` build it first loads the ONNX
/// Runtime library ([`runtime_library`]) and returns its error as a plain
/// message instead of the panic `ort` raises when the library is missing at
/// the first API call; in a linked build it is `Session::builder()`.
///
/// Either way the process environment is committed with ONNX Runtime's
/// telemetry off. `ort` defaults it *on* (`EnableTelemetryEvents`): a no-op
/// for pyke's linked builds, which carry no telemetry provider, but
/// Microsoft's own libraries do — ETW on Windows and, from 1.29, the 1DS SDK
/// on Linux and macOS, uploading model metadata and run statistics — and a
/// `load-dynamic` build runs whatever library the machine provides. A
/// document converter does not report its users' models to anyone.
pub fn session_builder() -> Result<SessionBuilder, String> {
    Session::builder(environment()?).map_err(|e| e.to_string())
}

/// The process's one ONNX Runtime [`Environment`] (ort 2.0.0-rc.14 made
/// it explicit: every session is created from one), built on first use
/// with telemetry off — and, in a `load-dynamic` build, after the runtime
/// library is loaded ([`runtime_library`]).
pub fn environment() -> Result<&'static ort::environment::Environment, String> {
    static ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        // Before the first tensor: without the arena (`DOCLING_RS_NO_ARENA`,
        // serve's default) every ONNX Runtime allocation is a `malloc`, and
        // the TableFormer decoder's ever-longer KV caches left glibc's heap
        // too fragmented to finish a long table on linux/arm64 (#871). The
        // fixed threshold maps the large ones, so freeing them returns them.
        docling_core::memory::pin_mmap_threshold();
        #[cfg(feature = "load-dynamic")]
        runtime_library()?;
        ort::init()
            .with_name("docling-rs")
            .with_telemetry(false)
            .build()
            .map_err(|e| format!("ONNX Runtime environment: {e}"))
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The parsed `DOCLING_RS_EP` choice. Named GPU variants are only ever
/// *selected* (returned by [`choice`]) when their cargo feature is compiled
/// in; [`parse`] itself is feature-blind so it can be unit-tested everywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ep {
    Cpu,
    Cuda,
    TensorRt,
    DirectMl,
    CoreMl,
    /// CPU-class EP with ARM NEON / x86 SIMD kernels (#324) — an accelerator
    /// for machines without a usable GPU provider, Apple silicon included.
    Xnnpack,
    /// Register everything compiled in, let ONNX Runtime fall back.
    Auto,
}

/// Parse a `DOCLING_RS_EP` value. `None` for values that name no known
/// provider (the caller warns and stays on CPU).
pub fn parse(v: &str) -> Option<Ep> {
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "cpu" => Some(Ep::Cpu),
        "cuda" => Some(Ep::Cuda),
        "tensorrt" | "trt" => Some(Ep::TensorRt),
        "directml" | "dml" => Some(Ep::DirectMl),
        "coreml" => Some(Ep::CoreMl),
        "xnnpack" => Some(Ep::Xnnpack),
        "auto" => Some(Ep::Auto),
        _ => None,
    }
}

/// The providers compiled into this binary, by their `DOCLING_RS_EP` names
/// (`cpu` first). For the bindings that map a device option onto
/// `DOCLING_RS_EP` (Python's `AcceleratorDevice.MPS` → `coreml`, #602) and
/// must only name a provider the build has. Reads no environment and leaves
/// [`choice`] unresolved, so the caller may still set `DOCLING_RS_EP` after
/// asking.
pub fn compiled_providers() -> Vec<&'static str> {
    [
        (Ep::Cpu, "cpu"),
        (Ep::Cuda, "cuda"),
        (Ep::TensorRt, "tensorrt"),
        (Ep::DirectMl, "directml"),
        (Ep::CoreMl, "coreml"),
        (Ep::Xnnpack, "xnnpack"),
    ]
    .into_iter()
    .filter(|&(ep, _)| compiled(ep))
    .map(|(_, name)| name)
    .collect()
}

/// Is this provider compiled into the binary (cargo feature enabled)?
fn compiled(ep: Ep) -> bool {
    match ep {
        Ep::Cpu => true,
        Ep::Cuda => cfg!(feature = "cuda"),
        Ep::TensorRt => cfg!(feature = "tensorrt"),
        Ep::DirectMl => cfg!(feature = "directml"),
        Ep::CoreMl => cfg!(feature = "coreml"),
        Ep::Xnnpack => cfg!(feature = "xnnpack"),
        Ep::Auto => true,
    }
}

/// Any GPU provider among `members` — the providers `auto` registers
/// ([`in_auto`]) in production, a stand-in set in the tests (the features
/// differ per build).
fn any_gpu(members: impl Fn(Ep) -> bool) -> bool {
    [Ep::Cuda, Ep::TensorRt, Ep::DirectMl, Ep::CoreMl]
        .into_iter()
        .any(members)
}

/// The choice when `DOCLING_RS_EP` is unset (or empty): a build that
/// compiled a CUDA-class provider in defaults to `auto` — whoever built with
/// `--features cuda` (or installed the `docling-rs-cuda` wheel) wants the
/// GPU used when one is usable, and `auto`'s per-session registration
/// falls back to CPU when not. A default build has nothing to register and
/// stays on the exact pre-#74 CPU path.
///
/// CoreML does not count (#602): on Apple silicon it is no clear win — with
/// its CPU-identical defaults it measured ~6% faster than the CPU provider on
/// a 25-page manual, while session setup costs ~2 s per worker and does not
/// parallelize, so a short one-shot run is a net loss — and a `coreml` build
/// that quietly registered it surprised the issue's reporter. A `coreml`
/// build therefore converts on CPU until `DOCLING_RS_EP=coreml` (or `auto`)
/// asks for CoreML by name.
fn default_choice_for(compiled: impl Fn(Ep) -> bool) -> Ep {
    // XNNPACK counts here (whoever built with it wants it used) but not in
    // [`prefers_fp32`] — it runs the same CPU-calibrated graphs as the CPU EP.
    if [Ep::Cuda, Ep::TensorRt, Ep::DirectMl, Ep::Xnnpack]
        .into_iter()
        .any(compiled)
    {
        Ep::Auto
    } else {
        Ep::Cpu
    }
}

/// `DOCLING_RS_EP` resolved once per process: the provider choice, and
/// whether `auto` may register CoreML — only when `auto` was asked for by
/// name, never as the unset default ([`default_choice_for`]).
#[derive(Clone, Copy)]
struct Resolved {
    ep: Ep,
    coreml_in_auto: bool,
}

/// Is `ep` among the providers `auto` registers? Compiled in, and for
/// CoreML also requested (`coreml_in_auto`). Pure for the tests;
/// [`in_auto`] is the production form.
fn auto_member(ep: Ep, compiled: impl Fn(Ep) -> bool, coreml_in_auto: bool) -> bool {
    !matches!(ep, Ep::Cpu | Ep::Auto) && compiled(ep) && (ep != Ep::CoreMl || coreml_in_auto)
}

/// The providers this process's `auto` registers.
fn in_auto(ep: Ep) -> bool {
    auto_member(ep, compiled, resolved().coreml_in_auto)
}

/// The effective provider choice for this process, resolved once. Invalid or
/// not-compiled-in requests degrade to CPU with a single stderr warning —
/// same convention as a missing model file.
pub fn choice() -> Ep {
    resolved().ep
}

fn resolved() -> Resolved {
    static RESOLVED: OnceLock<Resolved> = OnceLock::new();
    *RESOLVED.get_or_init(|| {
        let cpu = Resolved {
            ep: Ep::Cpu,
            coreml_in_auto: false,
        };
        let Some(raw) = docling_core::env::nonempty("DOCLING_RS_EP") else {
            if compiled(Ep::CoreMl) {
                docling_core::debug_log!(
                    "docling-rs: CoreML is compiled in but opt-in — set \
                     DOCLING_RS_EP=coreml to use it"
                );
            }
            return Resolved {
                ep: default_choice_for(compiled),
                coreml_in_auto: false,
            };
        };
        let Some(ep) = parse(&raw) else {
            eprintln!(
                "docling-rs: DOCLING_RS_EP={raw:?} names no known execution provider \
                 (cpu|cuda|tensorrt|directml|coreml|xnnpack|auto); using CPU"
            );
            return cpu;
        };
        if !compiled(ep) {
            eprintln!(
                "docling-rs: DOCLING_RS_EP={raw:?} requested, but this binary was built \
                 without that provider — rebuild with `--features {}`; using CPU",
                match ep {
                    Ep::Cuda => "cuda",
                    Ep::TensorRt => "tensorrt",
                    Ep::DirectMl => "directml",
                    Ep::CoreMl => "coreml",
                    Ep::Xnnpack => "xnnpack",
                    Ep::Cpu | Ep::Auto => unreachable!("always compiled"),
                }
            );
            return cpu;
        }
        Resolved {
            ep,
            coreml_in_auto: ep == Ep::Auto,
        }
    })
}

/// True when the int8 model defaults should be skipped in favor of fp32
/// because inference is (or may be) leaving the CPU. `Auto` counts as GPU as
/// soon as it registers any GPU provider: whether registration succeeds is
/// only known per-session, and a CPU fall-back running fp32 is merely the
/// pre-int8 speed, while a GPU running the CPU-calibrated int8 graph is a
/// conformance risk.
///
/// A no-op for ASR: the Whisper exports ship without int8 variants, so there
/// is no model selection for this to influence on that path.
pub fn prefers_fp32() -> bool {
    prefers_fp32_for(choice(), in_auto)
}

/// [`prefers_fp32`] for a given choice; `members` = what `auto` registers.
fn prefers_fp32_for(ep: Ep, members: impl Fn(Ep) -> bool) -> bool {
    match ep {
        // XNNPACK is CPU-class: the int8 QDQ graphs were calibrated for CPU
        // kernels and stay valid on it.
        Ep::Cpu | Ep::Xnnpack => false,
        Ep::Cuda | Ep::TensorRt | Ep::DirectMl | Ep::CoreMl => true,
        Ep::Auto => any_gpu(members),
    }
}

/// True when a multi-image batch is the better default for a model that
/// accepts one (docling-pdf's layout batch, #338): the CUDA-class providers
/// pay a real per-call dispatch cost that batching amortizes. Not CoreML
/// (#602): its static-partitions default (#324) only places a graph whose
/// shapes are all fixed, and a batched session keeps the batch axis free —
/// so under the old batch-of-4 default CoreML was handed nothing it was
/// allowed to run (0% GPU; fp32 heron 8.4 s vs 3.7 s on the CPU provider for
/// the issue's 25-page manual, M4 Max), while per-page mode pins the axis to
/// 1 and the same model runs on the GPU. Nor the CPU-class providers, where
/// per-page measured faster (#338). `Auto` batches only when it registers
/// a CUDA-class provider — a CoreML-only `auto` registers CoreML first and
/// wants the static graph.
pub fn prefers_batching() -> bool {
    // A Windows build with both `cuda` and `directml` whose CUDA provider does
    // not initialize runs on DirectML, which wants per-page too.
    prefers_batching_for(choice(), in_auto) && !is_directml()
}

/// [`prefers_batching`] for a given choice; `members` = what `auto` registers.
fn prefers_batching_for(ep: Ep, members: impl Fn(Ep) -> bool) -> bool {
    match ep {
        // DirectML (#840): batch 4 over the dynamic batch axis measured 2-5x
        // slower than per-page on both an NVIDIA T600 and an Intel iGPU.
        Ep::Cpu | Ep::Xnnpack | Ep::CoreMl | Ep::DirectMl => false,
        Ep::Cuda | Ep::TensorRt => true,
        Ep::Auto => [Ep::Cuda, Ep::TensorRt].into_iter().any(members),
    }
}

/// CoreML's model format as `DOCLING_RS_COREML_FORMAT` selects it — kept
/// apart from `ort`'s `ModelFormat` (compiled only with the `coreml`
/// feature) so the default is testable on every host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "coreml"), allow(dead_code))]
enum CoreMlFormat {
    NeuralNetwork,
    MlProgram,
}

/// Parse `DOCLING_RS_COREML_FORMAT`; `Err` carries an unrecognized value
/// (the caller warns and uses the default). The default is `NeuralNetwork`
/// (#602): measured on the issue's 25-page manual (M4 Max, per-page
/// layout), NeuralNetwork reproduces the CPU provider's Markdown byte for
/// byte, while MLProgram — ~1.8× faster on the GPU — changes the layout
/// detections on 14 of 25 pages (neighbouring boxes merge, reading order
/// shifts; 448 texts vs 489), under `cpu_only` units too, so it is
/// MLProgram's op lowering, not just GPU fp16. Correct output is the
/// default; MLProgram stays an opt-in with a notice. (#324 had moved to
/// MLProgram because NeuralNetwork aborted on the dynamic-shaped layout
/// graph; the static-partitions default since then keeps such partitions
/// off CoreML, and per-page mode makes the layout graph static.)
#[cfg_attr(not(feature = "coreml"), allow(dead_code))]
fn coreml_format(raw: Option<&str>) -> Result<CoreMlFormat, String> {
    match raw.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "neuralnetwork" | "nn") => Ok(CoreMlFormat::NeuralNetwork),
        Some("mlprogram") => Ok(CoreMlFormat::MlProgram),
        Some(other) => Err(other.to_string()),
    }
}

/// The CoreML provider, configured from the environment (#324, #602). The
/// model format defaults to `NeuralNetwork` — see [`coreml_format`] for why;
/// `DOCLING_RS_COREML_FORMAT=mlprogram` trades CPU-identical layout output
/// for speed (macOS 12+).
#[cfg(feature = "coreml")]
fn coreml_ep() -> ort::ep::CoreML {
    use ort::ep::coreml::{ComputeUnits, ModelFormat};
    static WARNED: OnceLock<()> = OnceLock::new();
    static FORMAT_NOTICE: OnceLock<()> = OnceLock::new();
    let format =
        match coreml_format(docling_core::env::nonempty("DOCLING_RS_COREML_FORMAT").as_deref()) {
            Ok(CoreMlFormat::NeuralNetwork) => ModelFormat::NeuralNetwork,
            Ok(CoreMlFormat::MlProgram) => {
                FORMAT_NOTICE.get_or_init(|| {
                    eprintln!(
                        "docling-rs: DOCLING_RS_COREML_FORMAT=mlprogram is faster on the Apple \
                     GPU, but its layout detections differ from the CPU provider's \
                     (neighbouring boxes merge, reading order shifts); the default \
                     neuralnetwork format matches CPU output"
                    );
                });
                ModelFormat::MLProgram
            }
            Err(other) => {
                WARNED.get_or_init(|| {
                    eprintln!(
                        "docling-rs: DOCLING_RS_COREML_FORMAT={other:?} is not \
                     neuralnetwork|mlprogram; using neuralnetwork"
                    );
                });
                ModelFormat::NeuralNetwork
            }
        };
    let mut ep = ort::ep::CoreML::default().with_model_format(format);
    // Static-shaped partitions only, ON by default (#324 follow-up): with the
    // stock dynamic-batch layout model, MLProgram otherwise fails an MPSGraph
    // assertion *inside* CoreML — a SIGABRT the process cannot catch, worse
    // than the NeuralNetwork error it replaced (M4 Max, macOS 26 report).
    // Keeping dynamic partitions off CoreML avoids it at the root;
    // `DOCLING_RS_COREML_STATIC_SHAPES=0` opts back into dynamic placement
    // for models known to be safe under it.
    let static_shapes =
        docling_core::env::nonempty("DOCLING_RS_COREML_STATIC_SHAPES").is_none_or(|v| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        });
    if static_shapes {
        ep = ep.with_static_input_shapes(true);
    }
    // Compute units default to CPU+GPU, not ONNX Runtime's `ALL` (#324
    // follow-up): `ALL` may place partitions on the fp16 Neural Engine, which
    // silently corrupts this model's output (max|Δlogits| = 6.5 vs CPU, no
    // error raised) and measured slower than the GPU path. `all`/`cpu_and_ne`
    // remain available for models validated on the ANE.
    let units = match docling_core::env::nonempty("DOCLING_RS_COREML_UNITS")
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("cpu_and_gpu" | "cpu_gpu" | "gpu") => ComputeUnits::CPUAndGPU,
        Some("all") => ComputeUnits::All,
        Some("cpu_and_ne" | "cpu_ane" | "ane") => ComputeUnits::CPUAndNeuralEngine,
        Some("cpu_only" | "cpu") => ComputeUnits::CPUOnly,
        Some(other) => {
            let other = other.to_string();
            WARNED.get_or_init(|| {
                eprintln!(
                    "docling-rs: DOCLING_RS_COREML_UNITS={other:?} is not \
                     all|cpu_and_gpu|cpu_and_ne|cpu_only; using cpu_and_gpu"
                );
            });
            ComputeUnits::CPUAndGPU
        }
    };
    ep.with_compute_units(units)
}

/// The XNNPACK provider (#324). It runs its own thread pool;
/// `DOCLING_RS_XNNPACK_THREADS` sizes it, unset keeps ONNX Runtime's default.
#[cfg(feature = "xnnpack")]
fn xnnpack_ep() -> ort::ep::XNNPACK {
    let mut ep = ort::ep::XNNPACK::default();
    if let Some(n) = docling_core::env::parse::<usize>("DOCLING_RS_XNNPACK_THREADS")
        .and_then(core::num::NonZeroUsize::new)
    {
        ep = ep.with_intra_op_num_threads(n);
    }
    ep
}

/// How the CUDA provider's BFC arena grows (#842): ONNX Runtime's default
/// doubles the extension each time (`kNextPowerOfTwo`), so a server that has
/// seen one large page keeps a power-of-two-sized chunk around for every
/// session; `same_as_requested` extends by exactly the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArenaExtend {
    NextPowerOfTwo,
    SameAsRequested,
}

/// cuDNN's convolution algorithm search (#842). `exhaustive` (ONNX Runtime's
/// default) benchmarks every algorithm with its maximal workspace on the
/// first run of each conv shape — fast kernels, but a workspace-sized
/// allocation per new page shape; `heuristic` asks cuDNN for its pick,
/// `default` takes cuDNN's default algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvAlgo {
    Exhaustive,
    Heuristic,
    Default,
}

/// The CUDA provider's configuration (#842), read from the environment once
/// — see [`cuda_options`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CudaOptions {
    /// `DOCLING_RS_CUDA_DEVICE` (0): the device every session runs on, also
    /// the device whose arena [`run_options`] shrinks.
    device: i32,
    /// `DOCLING_RS_CUDA_MEM_LIMIT_MB`: the arena's ceiling per session, in
    /// MiB; unset = unbounded (ONNX Runtime's default).
    mem_limit_mb: Option<usize>,
    /// `DOCLING_RS_CUDA_ARENA_EXTEND`: `next_power_of_two` (default) |
    /// `same_as_requested`.
    arena_extend: ArenaExtend,
    /// `DOCLING_RS_CUDA_CONV_ALGO_SEARCH`: `exhaustive` (default) |
    /// `heuristic` | `default`.
    conv_algo: ConvAlgo,
    /// `DOCLING_RS_CUDA_CONV_MAX_WORKSPACE` (on): let cuDNN use its maximal
    /// workspace for convolutions.
    conv_max_workspace: bool,
}

impl Default for CudaOptions {
    fn default() -> Self {
        Self {
            device: 0,
            mem_limit_mb: None,
            arena_extend: ArenaExtend::NextPowerOfTwo,
            conv_algo: ConvAlgo::Exhaustive,
            conv_max_workspace: true,
        }
    }
}

/// Parse the CUDA knobs from `get` (an environment lookup), reporting each
/// unusable value through `warn` and keeping that knob's default. Pure, so
/// the defaults and the vocabulary are testable on every host.
fn cuda_options_from(
    get: impl Fn(&str) -> Option<String>,
    mut warn: impl FnMut(String),
) -> CudaOptions {
    let mut o = CudaOptions::default();
    let lower = |v: String| v.trim().to_ascii_lowercase();
    if let Some(v) = get("DOCLING_RS_CUDA_DEVICE") {
        match v.trim().parse::<i32>() {
            Ok(n) if n >= 0 => o.device = n,
            _ => warn(format!(
                "DOCLING_RS_CUDA_DEVICE={v:?} is not a device index; using 0"
            )),
        }
    }
    if let Some(v) = get("DOCLING_RS_CUDA_MEM_LIMIT_MB") {
        match v.trim().parse::<usize>() {
            Ok(0) => {}
            Ok(mb) => o.mem_limit_mb = Some(mb),
            Err(_) => warn(format!(
                "DOCLING_RS_CUDA_MEM_LIMIT_MB={v:?} is not a number of MiB; leaving the arena unbounded"
            )),
        }
    }
    if let Some(v) = get("DOCLING_RS_CUDA_ARENA_EXTEND") {
        match lower(v.clone()).as_str() {
            "next_power_of_two" | "power_of_two" | "pow2" => {
                o.arena_extend = ArenaExtend::NextPowerOfTwo
            }
            "same_as_requested" | "requested" | "exact" => {
                o.arena_extend = ArenaExtend::SameAsRequested
            }
            _ => warn(format!(
                "DOCLING_RS_CUDA_ARENA_EXTEND={v:?} is not next_power_of_two|same_as_requested; \
                 using next_power_of_two"
            )),
        }
    }
    if let Some(v) = get("DOCLING_RS_CUDA_CONV_ALGO_SEARCH") {
        match lower(v.clone()).as_str() {
            "exhaustive" => o.conv_algo = ConvAlgo::Exhaustive,
            "heuristic" => o.conv_algo = ConvAlgo::Heuristic,
            "default" => o.conv_algo = ConvAlgo::Default,
            _ => warn(format!(
                "DOCLING_RS_CUDA_CONV_ALGO_SEARCH={v:?} is not exhaustive|heuristic|default; \
                 using exhaustive"
            )),
        }
    }
    if let Some(v) = get("DOCLING_RS_CUDA_CONV_MAX_WORKSPACE") {
        o.conv_max_workspace = !matches!(lower(v).as_str(), "0" | "false" | "no" | "off");
    }
    o
}

/// The CUDA knobs (#842), resolved once per process.
#[cfg_attr(not(any(feature = "cuda", feature = "tensorrt")), allow(dead_code))]
fn cuda_options() -> CudaOptions {
    static OPTS: OnceLock<CudaOptions> = OnceLock::new();
    *OPTS.get_or_init(|| {
        cuda_options_from(docling_core::env::nonempty, |msg| {
            eprintln!("docling-rs: {msg}")
        })
    })
}

/// The CUDA provider, configured from the environment (#842). ONNX Runtime's
/// defaults — an unbounded arena per session that grows by powers of two,
/// an exhaustive cuDNN search with the maximal workspace — are tuned for one
/// session on one GPU; a docling pipeline holds ~10 sessions per process
/// (layout, three TableFormer graphs, the OCR lanes, the enrichment models)
/// and a long-running server saw their arenas ratchet up to the device's
/// capacity (16 GB) until every conversion failed with
/// `BFCArena::AllocateRawInternal … Failed to allocate memory`.
/// `DOCLING_RS_CUDA_MEM_LIMIT_MB` bounds each arena,
/// `DOCLING_RS_CUDA_ARENA_EXTEND=same_as_requested` stops the power-of-two
/// over-allocation, `DOCLING_RS_CUDA_CONV_ALGO_SEARCH` /
/// `DOCLING_RS_CUDA_CONV_MAX_WORKSPACE` trim cuDNN's workspace; the arena
/// shrinkage after each run ([`run_options`]) returns what a run used.
#[cfg(feature = "cuda")]
fn cuda_ep() -> ort::ep::CUDA {
    use ort::ep::{cuda::ConvAlgorithmSearch, ArenaExtendStrategy};
    let o = cuda_options();
    let mut ep = ort::ep::CUDA::default().with_device_id(o.device);
    if let Some(mb) = o.mem_limit_mb {
        ep = ep.with_memory_limit(mb << 20);
    }
    ep.with_arena_extend_strategy(match o.arena_extend {
        ArenaExtend::NextPowerOfTwo => ArenaExtendStrategy::NextPowerOfTwo,
        ArenaExtend::SameAsRequested => ArenaExtendStrategy::SameAsRequested,
    })
    .with_conv_algorithm_search(match o.conv_algo {
        ConvAlgo::Exhaustive => ConvAlgorithmSearch::Exhaustive,
        ConvAlgo::Heuristic => ConvAlgorithmSearch::Heuristic,
        ConvAlgo::Default => ConvAlgorithmSearch::Default,
    })
    .with_conv_max_workspace(o.conv_max_workspace)
}

/// `DOCLING_RS_DML_DEVICE` (#840): the DXGI adapter index DirectML runs on.
/// `Ok(None)` when unset — the provider then asks for the high-performance
/// adapter, which on a laptop with an integrated and a discrete GPU is the
/// discrete one (DirectML's own default is adapter 0, often the iGPU).
#[cfg_attr(not(feature = "directml"), allow(dead_code))]
fn parse_dml_device(v: Option<String>) -> Result<Option<i32>, String> {
    let Some(v) = v else { return Ok(None) };
    match v.trim().parse::<i32>() {
        Ok(n) if n >= 0 => Ok(Some(n)),
        _ => Err(format!(
            "DOCLING_RS_DML_DEVICE={v:?} is not an adapter index; using the high-performance adapter"
        )),
    }
}

/// The DirectML provider as `DOCLING_RS_DML_DEVICE` configures it.
#[cfg(feature = "directml")]
fn directml_ep() -> ort::ep::DirectML {
    let ep = ort::ep::DirectML::default();
    let high_performance = |ep: ort::ep::DirectML| {
        ep.with_performance_preference(ort::ep::directml::PerformancePreference::HighPerformance)
    };
    match parse_dml_device(docling_core::env::nonempty("DOCLING_RS_DML_DEVICE")) {
        Ok(Some(id)) => ep.with_device_id(id),
        Ok(None) => high_performance(ep),
        Err(warning) => {
            static WARNED: OnceLock<()> = OnceLock::new();
            WARNED.get_or_init(|| eprintln!("docling-rs: {warning}"));
            high_performance(ep)
        }
    }
}

/// The dispatch list for the current choice. `None` means "register nothing"
/// (CPU — leave the builder untouched, the pre-#74 code path).
// The cfg-gated pushes read as sequential to clippy in single-feature builds.
#[allow(clippy::vec_init_then_push)]
fn dispatches() -> Option<Vec<ExecutionProviderDispatch>> {
    // Since ort 2.0.0-rc.13 the `ort::ep::*` structs are themselves gated
    // behind their cargo features (rc.12 exposed them unconditionally), so
    // every construction site needs a cfg gate. `choice()` still guarantees a
    // named provider is only ever *selected* when compiled in — the
    // fallthrough arm below is unreachable at runtime and exists to keep the
    // match exhaustive in builds without that feature.
    let d = match choice() {
        Ep::Cpu => return None,
        #[cfg(feature = "cuda")]
        Ep::Cuda => vec![cuda_ep().build().error_on_failure()],
        #[cfg(feature = "tensorrt")]
        Ep::TensorRt => vec![ort::ep::TensorRT::default().build().error_on_failure()],
        #[cfg(feature = "directml")]
        Ep::DirectMl => vec![directml_ep().build().error_on_failure()],
        #[cfg(feature = "coreml")]
        Ep::CoreMl => vec![coreml_ep().build().error_on_failure()],
        #[cfg(feature = "xnnpack")]
        Ep::Xnnpack => vec![xnnpack_ep().build().error_on_failure()],
        Ep::Auto => {
            #[allow(unused_mut)]
            let mut v: Vec<ExecutionProviderDispatch> = Vec::new();
            #[cfg(feature = "tensorrt")]
            v.push(ort::ep::TensorRT::default().build());
            #[cfg(feature = "cuda")]
            v.push(cuda_ep().build());
            // Only when `auto` was set by name (#602 — CoreML is opt-in).
            #[cfg(feature = "coreml")]
            if in_auto(Ep::CoreMl) {
                v.push(coreml_ep().build());
            }
            #[cfg(feature = "directml")]
            v.push(directml_ep().build());
            // Last, above only the implicit CPU fallback: a CPU-class
            // accelerator must not shadow a usable GPU.
            #[cfg(feature = "xnnpack")]
            v.push(xnnpack_ep().build());
            if v.is_empty() {
                return None; // CPU-only build: auto ≡ cpu
            }
            v
        }
        #[allow(unreachable_patterns)]
        ep => unreachable!("choice() returned {ep:?} without its cargo feature"),
    };
    Some(d)
}

/// Re-execute the process under an absolute `argv[0]` when a provider-library
/// execution provider (CUDA, TensorRT) is compiled in (#664).
///
/// ONNX Runtime opens `libonnxruntime_providers_shared.so` and the provider
/// (`…_cuda.so`, `…_tensorrt.so`) by absolute path from its runtime directory:
/// the directory of the object `dladdr()` finds ORT's own code in, made
/// absolute against the working directory. Linked statically into an
/// executable, that object is the main program, which glibc names by
/// `argv[0]` — so a binary started by bare name from `PATH`
/// (`docling-serve`, as Kubernetes manifests and the CUDA images' old `CMD`
/// do) looks for its providers in the working directory, never next to the
/// binary, and `LD_LIBRARY_PATH` is not consulted. Re-executing through
/// `/proc/self/exe` with the absolute path as `argv[0]` (same PID, arguments
/// and environment) makes the lookup land next to the binary, where
/// `ort`'s `copy-dylibs` and the CUDA images put the providers.
///
/// Call first thing in `main`, before any thread starts. A no-op unless on
/// Linux with ONNX Runtime linked in (not `load-dynamic`, whose library
/// `dladdr()` names by its own path) and a CUDA/TensorRT feature, and only
/// when `argv[0]` has no `/`. On failure it warns and returns: the CPU
/// provider still works.
pub fn reexec_for_provider_libraries() {
    #[cfg(all(
        target_os = "linux",
        any(feature = "cuda", feature = "tensorrt"),
        not(feature = "load-dynamic")
    ))]
    {
        use std::os::unix::process::CommandExt;
        let mut args = std::env::args_os();
        let Some(arg0) = args.next() else { return };
        if !needs_absolute_argv0(&arg0) {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let err = std::process::Command::new(&exe)
            .arg0(&exe)
            .args(args)
            .exec();
        eprintln!(
            "docling-rs: warning: re-executing as {} failed ({err}); ONNX Runtime will look \
             for its GPU provider libraries in the working directory",
            exe.display()
        );
    }
}

/// Whether ONNX Runtime would resolve its runtime directory against the
/// working directory: `argv[0]` without a path (#664).
#[cfg_attr(
    not(all(
        target_os = "linux",
        any(feature = "cuda", feature = "tensorrt"),
        not(feature = "load-dynamic")
    )),
    allow(dead_code)
)]
fn needs_absolute_argv0(arg0: &std::ffi::OsStr) -> bool {
    !arg0.as_encoded_bytes().contains(&b'/')
}

/// Register the selected execution providers on a session builder. Called by
/// every ONNX session in the workspace; a no-op (and infallible) in the
/// default CPU configuration.
pub fn apply(builder: SessionBuilder) -> Result<SessionBuilder, String> {
    let Some(eps) = dispatches() else {
        // No GPU EP selected: the CPU fallback still honors the arena knob.
        return memory_opts(builder);
    };
    static LOGGED: OnceLock<()> = OnceLock::new();
    LOGGED.get_or_init(|| {
        if docling_core::env::flag("DOCLING_RS_TIMING") {
            let names: Vec<&str> = eps.iter().map(|e| e.name()).collect();
            eprintln!("docling-rs: execution providers: {names:?}");
        }
        // CoreML's cost profile is invisible otherwise (#324 follow-up
        // testing): session creation runs ~2 s per worker, does not
        // parallelize, and only amortizes over long-lived processes or large
        // batches — a one-shot CLI conversion is typically *slower* than CPU,
        // with correct output and nothing else to hint at why. Say so once at
        // the moment the cost is incurred.
        #[cfg(feature = "coreml")]
        if choice() == Ep::CoreMl || (choice() == Ep::Auto && in_auto(Ep::CoreMl)) {
            eprintln!(
                "docling-rs: CoreML registered; its session setup costs roughly 2 s per \
                 worker and pays off for long-lived processes and large batches — for \
                 short one-shot runs DOCLING_RS_EP=cpu is usually faster"
            );
        }
    });
    let builder = builder
        .with_execution_providers(eps)
        .map_err(|e| format!("execution provider registration: {e}"))?;
    // DirectML supports neither memory patterns nor parallel execution (#840);
    // ONNX Runtime turns both off itself with a warning per session — say it
    // up front instead. Only when DirectML is what runs: an `auto` session
    // that fell back to CPU keeps the CPU defaults.
    #[cfg(feature = "directml")]
    let builder = if is_directml() {
        builder
            .with_memory_pattern(false)
            .map_err(|e| format!("directml memory pattern: {e}"))?
            .with_parallel_execution(false)
            .map_err(|e| format!("directml sequential execution: {e}"))?
    } else {
        builder
    };
    // After the GPU EPs, so they keep registration priority.
    memory_opts(builder)
}

/// Session memory options (#263): with `DOCLING_RS_NO_ARENA` set, the CPU
/// execution provider registers with its memory arena disabled
/// (`DisableCpuMemArena`) and initializers stay out of arena allocations.
/// ONNX Runtime's CPU arena grows a slab for every new tensor shape it sees
/// — a PDF's pages all differ, so a warm server's arena ratchets up with the
/// largest documents it ever served and never returns a byte; without it,
/// activations free after each run (and `malloc_trim` hands them back to the
/// OS) at a few percent inference cost. Off by default — batch CLI runs
/// prefer the arena's speed. Called at the *end* of [`apply`], so an explicit
/// GPU execution provider (registered first) keeps priority.
fn memory_opts(builder: SessionBuilder) -> Result<SessionBuilder, String> {
    if !docling_core::env::flag("DOCLING_RS_NO_ARENA") {
        return Ok(builder);
    }
    use ort::ep::{cpu::CPU, ExecutionProvider as _};
    let cpu = CPU::default().with_arena_allocator(false);
    let mut builder = builder
        .with_config_entry("session.use_device_allocator_for_initializers", "1")
        .map_err(|e| format!("allocator config: {e}"))?;
    cpu.register(&mut builder)
        .map_err(|e| format!("cpu ep: {e}"))?;
    Ok(builder)
}

/// A one-node `Identity` ONNX graph (ir 8, opset 13; `x: float[1] -> y`),
/// hand-encoded: the smallest model a session can be created from, used to
/// find out whether the CUDA or DirectML provider initializes on this
/// machine ([`cuda_usable`], [`is_directml`]).
#[cfg_attr(
    not(any(feature = "cuda", feature = "tensorrt", feature = "directml")),
    allow(dead_code)
)]
const PROBE_MODEL: &[u8] = &[
    0x08, 0x08, 0x12, 0x0c, 0x64, 0x6f, 0x63, 0x6c, 0x69, 0x6e, 0x67, 0x2d, 0x6f, 0x6e, 0x6e, 0x78,
    0x3a, 0x52, 0x0a, 0x1a, 0x0a, 0x01, 0x78, 0x12, 0x01, 0x79, 0x1a, 0x08, 0x69, 0x64, 0x65, 0x6e,
    0x74, 0x69, 0x74, 0x79, 0x22, 0x08, 0x49, 0x64, 0x65, 0x6e, 0x74, 0x69, 0x74, 0x79, 0x12, 0x12,
    0x64, 0x6f, 0x63, 0x6c, 0x69, 0x6e, 0x67, 0x2d, 0x6f, 0x6e, 0x6e, 0x78, 0x2d, 0x70, 0x72, 0x6f,
    0x62, 0x65, 0x5a, 0x0f, 0x0a, 0x01, 0x78, 0x12, 0x0a, 0x0a, 0x08, 0x08, 0x01, 0x12, 0x04, 0x0a,
    0x02, 0x08, 0x01, 0x62, 0x0f, 0x0a, 0x01, 0x79, 0x12, 0x0a, 0x0a, 0x08, 0x08, 0x01, 0x12, 0x04,
    0x0a, 0x02, 0x08, 0x01, 0x42, 0x04, 0x0a, 0x00, 0x10, 0x0d,
];

/// Whether a CUDA-class provider (CUDA, TensorRT) is the one the sessions
/// run on, as far as the *choice* goes: named outright, or in `auto` with
/// one compiled in. Under `auto` the device may still be absent — that is
/// [`cuda_usable`]'s question.
fn cuda_class_chosen() -> bool {
    match choice() {
        Ep::Cuda | Ep::TensorRt => true,
        Ep::Auto => in_auto(Ep::Cuda) || in_auto(Ep::TensorRt),
        Ep::Cpu | Ep::DirectMl | Ep::CoreMl | Ep::Xnnpack => false,
    }
}

/// Whether the CUDA provider actually initializes on this machine (#842):
/// `auto` registers it without `error_on_failure`, so a CUDA build on a host
/// without a device (or its driver) silently converts on CPU — and a run
/// option that names the GPU arena then fails every run ("Did not find an
/// arena based allocator registered for device-id combination"). Decided
/// once, by creating a session from [`PROBE_MODEL`] with the provider set to
/// fail loudly; the CUDA context it initializes is the one the real
/// sessions reuse, so the probe's cost is paid once per process either way.
/// A provider named outright is already loud, so no probe is needed there.
fn cuda_usable() -> bool {
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        if !cuda_class_chosen() {
            return false;
        }
        if choice() != Ep::Auto {
            return true;
        }
        #[cfg(any(feature = "cuda", feature = "tensorrt"))]
        {
            let probe = || -> Result<(), String> {
                let builder = session_builder()?;
                #[cfg(feature = "cuda")]
                let ep = cuda_ep().build().error_on_failure();
                #[cfg(not(feature = "cuda"))]
                let ep = ort::ep::TensorRT::default().build().error_on_failure();
                let mut builder = builder
                    .with_execution_providers([ep])
                    .map_err(|e| e.to_string())?;
                let _guard = CREATION.lock().unwrap_or_else(|p| p.into_inner());
                builder
                    .commit_from_memory(PROBE_MODEL)
                    .map_err(|e| e.to_string())?;
                Ok(())
            };
            match probe() {
                Ok(()) => true,
                Err(e) => {
                    docling_core::debug_log!(
                        "docling-onnx: the CUDA provider did not initialize ({e}); sessions run \
                         on CPU, the GPU arena options stay off"
                    );
                    false
                }
            }
        }
        #[cfg(not(any(feature = "cuda", feature = "tensorrt")))]
        false
    })
}

/// Whether DirectML is the provider the sessions run on (#840): named
/// outright (`DOCLING_RS_EP=directml`, which fails loudly without a device),
/// or `auto` with it compiled in, no CUDA-class provider ahead of it that
/// initializes, and a DirectML device that does — probed once on
/// [`PROBE_MODEL`] like [`cuda_usable`], because `auto` registers DirectML
/// without `error_on_failure` and a host without a usable adapter silently
/// converts on CPU, where the DirectML policies (one page worker, no
/// batching, no memory pattern) would only cost throughput.
pub fn is_directml() -> bool {
    #[cfg(feature = "directml")]
    {
        static ACTIVE: OnceLock<bool> = OnceLock::new();
        *ACTIVE.get_or_init(|| match choice() {
            Ep::DirectMl => true,
            Ep::Auto if in_auto(Ep::DirectMl) => {
                if cuda_class_chosen() && cuda_usable() {
                    return false;
                }
                let probe = || -> Result<(), String> {
                    let mut builder = session_builder()?
                        .with_execution_providers([directml_ep().build().error_on_failure()])
                        .map_err(|e| e.to_string())?
                        .with_memory_pattern(false)
                        .map_err(|e| e.to_string())?
                        .with_parallel_execution(false)
                        .map_err(|e| e.to_string())?;
                    let _guard = CREATION.lock().unwrap_or_else(|p| p.into_inner());
                    builder
                        .commit_from_memory(PROBE_MODEL)
                        .map_err(|e| e.to_string())?;
                    Ok(())
                };
                match probe() {
                    Ok(()) => true,
                    Err(e) => {
                        docling_core::debug_log!(
                            "docling-onnx: the DirectML provider did not initialize ({e}); \
                             sessions run on CPU with the CPU defaults"
                        );
                        false
                    }
                }
            }
            _ => false,
        })
    }
    #[cfg(not(feature = "directml"))]
    {
        false
    }
}

/// Whether the sessions' CUDA arena is shrunk after every run (#842): on
/// whenever a CUDA-class provider runs the sessions, `DOCLING_RS_CUDA_ARENA_SHRINK=0`
/// opts out.
fn arena_shrink_wanted() -> bool {
    docling_core::env::nonempty("DOCLING_RS_CUDA_ARENA_SHRINK").is_none_or(|v| {
        !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// The run options every session run in the workspace passes ([`run`]):
/// `None` on CPU and the non-CUDA providers; under a CUDA-class provider,
/// ONNX Runtime's `memory.enable_memory_arena_shrinkage` for the device, so
/// the arena hands the chunks a run grew by back to the driver when the run
/// ends instead of keeping them for the life of the session (#842: with
/// page-shaped tensors every document is a new high-water mark, and a
/// server's ~10 sessions each kept theirs — 15.9 GB of a 16 GB T4 after a
/// day). The initial chunk and the weights stay; what a run needs again it
/// reallocates, a few ms of `cudaMalloc` against a conversion of seconds.
/// Built once; `RunOptions` is shareable across sessions and threads.
pub fn run_options() -> Option<&'static ort::session::RunOptions> {
    static OPTS: OnceLock<Option<ort::session::RunOptions>> = OnceLock::new();
    OPTS.get_or_init(|| {
        if !arena_shrink_wanted() || !cuda_usable() {
            return None;
        }
        let mut opts = match ort::session::RunOptions::new() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("docling-rs: run options unavailable ({e}); the CUDA arena is not shrunk between runs");
                return None;
            }
        };
        let device = format!("gpu:{}", cuda_options().device);
        if let Err(e) = opts.set("memory.enable_memory_arena_shrinkage", &device) {
            eprintln!("docling-rs: arena shrinkage for {device} not accepted ({e}); the CUDA arena is not shrunk between runs");
            return None;
        }
        docling_core::debug_log!("docling-onnx: CUDA arena shrinkage after each run ({device})");
        Some(opts)
    })
    .as_ref()
}

/// [`run_options`] for a run that writes some outputs into values the caller
/// provides (`OutputSelector::preallocate`) — built per run, since the
/// selector holds them — with the same CUDA arena shrinkage.
pub fn run_options_with_outputs(
    outputs: ort::session::OutputSelector,
) -> Result<ort::session::RunOptions<ort::session::HasSelectedOutputs>, String> {
    let mut opts = ort::session::RunOptions::new()
        .map_err(|e| format!("run options: {e}"))?
        .with_outputs(outputs);
    if arena_shrink_wanted() && cuda_usable() {
        let device = format!("gpu:{}", cuda_options().device);
        opts.set("memory.enable_memory_arena_shrinkage", &device)
            .map_err(|e| format!("arena shrinkage for {device}: {e}"))?;
    }
    Ok(opts)
}

/// An owned ONNX Runtime `f32` tensor of `shape` over the first elements of
/// `data` — memory the caller owns, which ONNX Runtime writes or reads in
/// place and never frees (dropping the value releases only its handle). The
/// TableFormer decoder binds its per-layer KV-cache outputs to such views
/// (#871): `OutputSelector::preallocate` takes an owned value, and `ort`'s
/// safe constructors either take ownership of a `Vec` (which could then not
/// be reused for the next step) or return a borrowed `TensorRefMut` that
/// cannot become one.
///
/// # Safety
///
/// Until the returned value and every clone of it are dropped — the clone in
/// the `OutputSelector`/`RunOptions` it is preallocated in, and the session
/// output a run hands back for it — `data` must not be moved, freed, read or
/// written by anything but the ONNX Runtime run it is bound to.
pub unsafe fn borrowed_f32(
    data: &mut [f32],
    shape: &[usize],
    allocator: &ort::memory::Allocator,
) -> Result<ort::value::DynValue, String> {
    use ort::AsPointer as _;
    let n: usize = shape.iter().product();
    if n > data.len() {
        return Err(format!(
            "borrowed tensor {shape:?} needs {n} elements, the buffer holds {}",
            data.len()
        ));
    }
    let dims: Vec<i64> = shape.iter().map(|&d| d as i64).collect();
    // ONNX Runtime copies the memory info into the value.
    let info = ort::memory::MemoryInfo::default();
    let api = ort::api();
    let mut value: *mut ort::sys::OrtValue = std::ptr::null_mut();
    // SAFETY: `data` holds at least `n` f32s (checked above) and, per this
    // function's contract, outlives the value; `dims` and `info` are valid
    // for the call.
    let status = unsafe {
        (api.CreateTensorWithDataAsOrtValue)(
            info.ptr(),
            data.as_mut_ptr().cast(),
            n * std::mem::size_of::<f32>(),
            dims.as_ptr(),
            dims.len(),
            ort::sys::ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT,
            &mut value,
        )
    };
    if !status.0.is_null() {
        // SAFETY: a non-null status is an error the API allocated; its message
        // is a NUL-terminated string valid until the status is released.
        let message = unsafe {
            let msg = std::ffi::CStr::from_ptr((api.GetErrorMessage)(status.0))
                .to_string_lossy()
                .into_owned();
            (api.ReleaseStatus)(status.0);
            msg
        };
        return Err(format!("borrowed tensor {shape:?}: {message}"));
    }
    let value = std::ptr::NonNull::new(value).ok_or("borrowed tensor: null value")?;
    // SAFETY: `value` is the tensor just created; ownership passes to the
    // returned handle, whose drop calls ReleaseValue (not the data's free).
    Ok(unsafe { ort::value::DynValue::from_ptr(value, Some(allocator)) })
}

/// Run a session with the workspace's run options ([`run_options`]) — what
/// every `Session::run` in docling-pdf, docling-asr and docling-rag goes
/// through, so the GPU arena policy is one place.
pub fn run<'s, 'i, 'v: 'i, const N: usize>(
    session: &'s mut Session,
    inputs: impl Into<ort::session::SessionInputs<'i, 'v, N>>,
) -> ort::Result<ort::session::SessionOutputs<'s>> {
    match run_options() {
        Some(opts) => session.run_with_options(inputs, opts),
        None => session.run(inputs),
    }
}

/// Whether the sessions share one GPU, so every extra session is device
/// memory rather than throughput (#842): under a CUDA-class provider the
/// page-worker pool and the OCR lanes default to one — each worker holds
/// its own layout/OCR sessions, and ~0.4 GB of host RAM per worker is
/// several GB of VRAM per worker on a GPU, where inference is already
/// parallel inside the device. `DOCLING_RS_PDF_WORKERS` /
/// `DOCLING_RS_OCR_SESSIONS` still set any count. DirectML too (#840): one
/// worker measured 2.5x faster than its four-worker batched default on a
/// 10-page document (2.62 s vs 6.40 s, NVIDIA T600) at a quarter of the
/// VRAM (0.6 vs 2.6 GB). CPU and CoreML keep their defaults.
pub fn prefers_single_worker() -> bool {
    (cuda_class_chosen() && cuda_usable()) || is_directml()
}

/// The process-wide session-creation lock GPU builds take (#452).
static CREATION: Mutex<()> = Mutex::new(());

/// Serialize ONNX Runtime session creation while a GPU (non-CPU) execution
/// provider is registered — held for the duration of `commit_from_file` by
/// every session in the workspace ([`commit`], [`commit_uncached`]).
///
/// The page-worker pool, the OCR lanes and the enrichment models all open
/// their sessions concurrently (`thread::scope`) so the start-up latency is
/// paid once, not per session. ONNX Runtime's CUDA provider does not tolerate
/// that on one device (#452): with `DOCLING_RS_PDF_WORKERS` or
/// `DOCLING_RS_OCR_SESSIONS` above 1 initialization failed intermittently
/// with `Exception during initialization: … stride > 0 was false` — the
/// racing provider set-ups corrupt each other's graph state while the
/// sessions, once created, run side by side without trouble. So creation is
/// funnelled through one lock; inference stays fully parallel. CPU builds
/// (and `DOCLING_RS_EP=cpu`) keep the parallel start-up: the CPU provider
/// has no such race, and the overlap is the whole point of the lanes.
///
/// `None` when no lock is needed, so callers hold it as `let _guard = …;`.
pub fn creation_guard() -> Option<MutexGuard<'static, ()>> {
    dispatches()?;
    // A panic while creating a session poisons the lock; the next creation
    // is unaffected by that failure, so take the lock anyway.
    Some(CREATION.lock().unwrap_or_else(|p| p.into_inner()))
}

/// The ONNX Runtime library's own `(major, minor)` version — its
/// `OrtApiBase::GetVersionString`, read once. The statically linked runtime
/// answers through its entry point; a `load-dynamic` build (#504) asks the
/// library [`runtime_library`] loaded, whatever the machine provided. `None`
/// when that library is unavailable or the string does not parse. (The
/// build-info string is no substitute: Microsoft's release builds report
/// `git-branch=HEAD`, pyke's `rel-1.28.0`.)
pub fn runtime_version() -> Option<(u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| parse_version(&version_string()?))
}

#[cfg(not(feature = "load-dynamic"))]
fn version_string() -> Option<String> {
    // SAFETY: the linked runtime's C entry point; it returns a pointer to a
    // static table whose `GetVersionString` yields a static C string.
    unsafe { read_version(ort::sys::OrtGetApiBase()) }
}

#[cfg(feature = "load-dynamic")]
fn version_string() -> Option<String> {
    let path = runtime_library().ok()?;
    // SAFETY: `runtime_library` already opened this library through `ort`;
    // reopening it only bumps the loader's reference count, and the symbol
    // is ONNX Runtime's documented C entry point.
    unsafe {
        let lib = libloading::Library::new(path).ok()?;
        let base: libloading::Symbol<unsafe extern "C" fn() -> *const ort::sys::OrtApiBase> =
            lib.get(b"OrtGetApiBase").ok()?;
        read_version(base())
    }
}

/// # Safety
/// `base` is null or ONNX Runtime's `OrtApiBase`.
unsafe fn read_version(base: *const ort::sys::OrtApiBase) -> Option<String> {
    if base.is_null() {
        return None;
    }
    let s = ((*base).GetVersionString)();
    (!s.is_null()).then(|| std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned())
}

/// `"1.28.0"` → `(1, 28)`.
fn parse_version(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Cap `builder` below the layout-level graph optimizations when the runtime
/// predates ONNX Runtime 1.29 (#517) — the linked builds carry 1.31 since
/// #523, so this only ever fires for an older `libonnxruntime` under
/// `load-dynamic`: on x86-64 with AVX2/AVX-512, 1.26–1.28's
/// `NchwcTransformer` rewrites the picture classifier into a graph that
/// returns the same logits for every input — every picture came out `table`
/// 0.091 / `logo` 0.085 / `icon` 0.078. Disabling that one transformer, or
/// `EXTENDED` (Level2), restores the reference output exactly; 1.29 fixed it
/// at every level. The cost is the NCHWc speed-up, ~5 ms per picture. An
/// unknown version is treated as affected — the capped graph is correct on
/// every runtime, the uncapped one is not. Only the classifier opts in: the
/// layout, TableFormer and OCR graphs are pinned by the conformance
/// snapshots at the full level.
pub fn cap_before_1_29(builder: SessionBuilder) -> Result<SessionBuilder, String> {
    let version = runtime_version();
    if version.is_some_and(|v| v >= (1, 29)) {
        return Ok(builder);
    }
    docling_core::debug_log!(
        "docling-onnx: ONNX Runtime {version:?} predates 1.29 — graph optimization capped at \
         EXTENDED (#517)"
    );
    builder
        .with_optimization_level(GraphOptimizationLevel::Level2)
        .map_err(|e| e.to_string())
}

/// `DOCLING_RS_ORT_PROFILE=<dir>`: have ONNX Runtime write its per-operator
/// profile of every run of this session to `<dir>/<model stem>_<timestamp>.json`
/// (ORT's Chrome-trace JSON, one `model_run` event per run and one
/// `*_kernel_time` event per node). Off unless the variable names a
/// directory; a session option, so it costs nothing otherwise. Summarize a
/// file with `scripts/dev/ort_profile_summary.py`. The profile is written when
/// the session is dropped — a process that is killed loses it.
fn with_profile(builder: SessionBuilder, model_path: &Path) -> Result<SessionBuilder, String> {
    let Some(dir) = docling_core::env::nonempty("DOCLING_RS_ORT_PROFILE") else {
        return Ok(builder);
    };
    let stem = model_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    builder
        .with_profiling(Path::new(&dir).join(stem))
        .map_err(|e| format!("DOCLING_RS_ORT_PROFILE: {e}"))
}

/// Create the session for `model_path` the plain way (no graph cache) under
/// the [`creation_guard`] — for the models whose graph is not worth caching
/// (enrichment, Whisper, the embedder) but which must still serialize their
/// creation on a GPU (#452).
pub fn commit_uncached(
    builder: SessionBuilder,
    model_path: impl AsRef<Path>,
) -> Result<Session, String> {
    let model_path = model_path.as_ref();
    let mut builder = with_profile(builder, model_path)?;
    let _guard = creation_guard();
    builder
        .commit_from_file(model_path)
        .map_err(|e| e.to_string())
}

/// Create the session for `model_path`, serving ONNX Runtime's *optimized*
/// graph from an on-disk cache when this machine has built it before.
///
/// Session creation is dominated by graph optimization — for the int8 layout
/// model ~0.8 s of the ~0.95 s (measured, 4 threads), for the TableFormer
/// encoder ~1 s — and every process pays it again, per pool worker: on a
/// one-page digital PDF that is more than half of the whole wall time. ONNX
/// Runtime can serialize the optimized graph, and loading that with the
/// optimizer disabled takes ~0.15 s and runs the identical graph (same
/// kernels, same numerics — the corpus output is byte-identical). The cache
/// is keyed on the model file (path, size, mtime), `variant` (session options
/// that change the graph, e.g. a pinned batch dimension), the ONNX Runtime
/// API version and the CPU's SIMD features, because the saved graph carries
/// hardware-specific (NCHWc) kernels; a miss on any of those simply rebuilds.
/// CPU provider only — a GPU EP partitions the graph differently. Location:
/// `DOCLING_RS_GRAPH_CACHE_DIR`, else `$XDG_CACHE_HOME/docling-rs/graphs`,
/// else `~/.cache/docling-rs/graphs`; `DOCLING_RS_NO_GRAPH_CACHE=1` opts out,
/// as does an unwritable directory (the model then loads the ordinary way).
/// Every failure on the cached path falls back to the ordinary load, so a
/// stale or truncated entry costs one rebuild, never a wrong answer.
pub fn commit(builder: SessionBuilder, model_path: &str, variant: &str) -> Result<Session, String> {
    let builder = with_profile(builder, Path::new(model_path))?;
    // Every `commit_from_file` below runs under the GPU creation lock (#452);
    // on CPU there is no lock and the workers/lanes still start in parallel.
    let _guard = creation_guard();
    let plain = |mut b: SessionBuilder| b.commit_from_file(model_path).map_err(|e| e.to_string());
    let Some(cache) = graph_cache_path(model_path, variant) else {
        return plain(builder);
    };
    if cache.is_file() {
        let hit = builder
            .clone()
            .with_optimization_level(GraphOptimizationLevel::Disable)
            .map_err(|e| e.to_string())
            .and_then(|mut b| b.commit_from_file(&cache).map_err(|e| e.to_string()));
        match hit {
            Ok(session) => {
                docling_core::debug_log!("docling-onnx: graph cache hit {}", cache.display());
                return Ok(session);
            }
            Err(e) => {
                docling_core::debug_log!(
                    "docling-onnx: graph cache entry {} unusable ({e}); rebuilding",
                    cache.display()
                );
                let _ = std::fs::remove_file(&cache);
            }
        }
    }
    // Miss: build normally, asking ORT to serialize the optimized graph next
    // to the final name, then publish it atomically. Two workers racing here
    // both write their own temp file; whichever renames first wins, the other
    // overwrites with identical content.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = cache.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let built = builder
        .clone()
        .with_optimized_model_path(&tmp)
        .map_err(|e| e.to_string())
        .and_then(plain);
    match built {
        Ok(session) => {
            // ORT serializes the optimized graph as a side effect of session
            // creation and does not fail the session when that write comes up
            // short (a full disk truncates the file and the session still
            // initializes from memory). Publishing such a prefix would poison
            // the cache: every later process would try it, fail, remove it and
            // rebuild — a ~50 s cold start for the TableFormer graphs, seen
            // exactly once after a disk-full episode. So check the protobuf
            // skeleton first: every top-level field's declared length must end
            // within the file (a truncation cuts inside the graph field).
            if !onnx_file_complete(&tmp) {
                docling_core::debug_log!(
                    "docling-onnx: graph cache write of {} incomplete (disk full?); not kept",
                    cache.display()
                );
                let _ = std::fs::remove_file(&tmp);
            } else if std::fs::rename(&tmp, &cache).is_err() {
                let _ = std::fs::remove_file(&tmp);
            } else {
                docling_core::debug_log!("docling-onnx: graph cache wrote {}", cache.display());
            }
            Ok(session)
        }
        Err(_) => {
            // Serialization itself may be what failed (read-only dir, disk
            // full): load without it before giving up.
            let _ = std::fs::remove_file(&tmp);
            plain(builder)
        }
    }
}

/// Whether `path` holds a structurally complete protobuf message: walk the
/// top-level fields of the serialized `ModelProto`, seeking over each
/// length-delimited body, and require the walk to land exactly on EOF. A file
/// cut short by a failed write (disk full) fails this — the truncation lands
/// inside the multi-megabyte `graph` field, whose length prefix then promises
/// more bytes than exist — while costing only a handful of reads, never a
/// parse of the weights. Malformed wire types or zero length fail too.
fn onnx_file_complete(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    let mut pos = 0u64;
    let read_varint = |f: &mut std::fs::File, pos: &mut u64| -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let mut b = [0u8; 1];
            f.read_exact(&mut b).ok()?;
            *pos += 1;
            v |= u64::from(b[0] & 0x7f) << shift;
            if b[0] & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    };
    while pos < len {
        let Some(key) = read_varint(&mut f, &mut pos) else {
            return false;
        };
        let skip = match key & 7 {
            0 => {
                // varint payload
                if read_varint(&mut f, &mut pos).is_none() {
                    return false;
                }
                0
            }
            1 => 8,
            2 => match read_varint(&mut f, &mut pos) {
                Some(n) => n,
                None => return false,
            },
            5 => 4,
            _ => return false,
        };
        if skip > len - pos {
            return false;
        }
        if skip > 0 && f.seek(SeekFrom::Current(skip as i64)).is_err() {
            return false;
        }
        pos += skip;
    }
    pos == len
}

/// Where the optimized graph for `model_path` + `variant` lives on this
/// machine, or `None` when caching is off (env, non-CPU provider, no usable
/// directory, unreadable model file).
fn graph_cache_path(model_path: &str, variant: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    if docling_core::env::flag("DOCLING_RS_NO_GRAPH_CACHE") || choice() != Ep::Cpu {
        return None;
    }
    let dir = graph_cache_dir()?;
    let meta = std::fs::metadata(model_path).ok()?;
    let canonical = std::fs::canonicalize(model_path).unwrap_or_else(|_| PathBuf::from(model_path));
    let mut h = std::hash::DefaultHasher::new();
    canonical.hash(&mut h);
    meta.len().hash(&mut h);
    if let Ok(m) = meta.modified() {
        if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
            d.as_nanos().hash(&mut h);
        }
    }
    variant.hash(&mut h);
    ort::MINOR_VERSION.hash(&mut h);
    // The runtime's own build string (version, commit, flags): the optimized
    // graph is that build's output, and with `load-dynamic` (#504) the
    // library under one API version changes with whatever is on the machine.
    ort::info().hash(&mut h);
    cpu_features().hash(&mut h);
    let stem = Path::new(model_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("model");
    Some(dir.join(format!("{stem}-{:016x}.onnx", h.finish())))
}

fn graph_cache_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = if let Some(d) = docling_core::env::nonempty("DOCLING_RS_GRAPH_CACHE_DIR") {
            PathBuf::from(d)
        } else if let Some(x) = docling_core::env::nonempty("XDG_CACHE_HOME") {
            PathBuf::from(x).join("docling-rs").join("graphs")
        } else {
            PathBuf::from(docling_core::env::nonempty("HOME")?)
                .join(".cache")
                .join("docling-rs")
                .join("graphs")
        };
        std::fs::create_dir_all(&dir).ok()?;
        // Writable? A read-only cache (a baked container layer) still serves hits.
        Some(dir)
    })
    .clone()
}

/// The SIMD feature set the saved graph was specialized for.
fn cpu_features() -> String {
    let arch = std::env::consts::ARCH;
    // Only x86_64 has runtime-detected feature tiers worth keying the cache
    // on; elsewhere the arch alone is the key (and a `mut` String would be an
    // `unused_mut` warning on the aarch64 macOS CI runner).
    #[cfg(not(target_arch = "x86_64"))]
    {
        String::from(arch)
    }
    #[cfg(target_arch = "x86_64")]
    {
        let mut s = String::from(arch);
        for (name, on) in [
            ("avx", std::arch::is_x86_feature_detected!("avx")),
            ("avx2", std::arch::is_x86_feature_detected!("avx2")),
            ("fma", std::arch::is_x86_feature_detected!("fma")),
            ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
            ("avx512bw", std::arch::is_x86_feature_detected!("avx512bw")),
            (
                "avx512vnni",
                std::arch::is_x86_feature_detected!("avx512vnni"),
            ),
        ] {
            if on {
                s.push(' ');
                s.push_str(name);
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #602: the provider-dependent defaults, per provider and per
    /// compiled-in set (the closures stand in for the builds — the real
    /// features differ per host).
    #[test]
    fn bare_program_names_need_an_absolute_argv0() {
        use std::ffi::OsStr;
        assert!(needs_absolute_argv0(OsStr::new("docling-serve")));
        assert!(!needs_absolute_argv0(OsStr::new(
            "/usr/local/bin/docling-serve"
        )));
        assert!(!needs_absolute_argv0(OsStr::new("./docling-rs")));
        assert!(!needs_absolute_argv0(OsStr::new(
            "target/release/docling-rs"
        )));
    }

    #[test]
    fn provider_defaults_follow_the_provider() {
        let cpu_only = |ep: Ep| matches!(ep, Ep::Cpu | Ep::Auto);
        let mac = |ep: Ep| matches!(ep, Ep::Cpu | Ep::Auto | Ep::CoreMl);
        let cuda = |ep: Ep| matches!(ep, Ep::Cpu | Ep::Auto | Ep::Cuda);
        // fp32 under every GPU provider, CoreML included; int8 on CPU-class.
        for ep in [Ep::Cuda, Ep::TensorRt, Ep::DirectMl, Ep::CoreMl] {
            assert!(prefers_fp32_for(ep, mac), "{ep:?}");
        }
        assert!(!prefers_fp32_for(Ep::Cpu, mac));
        assert!(!prefers_fp32_for(Ep::Xnnpack, mac));
        assert!(prefers_fp32_for(Ep::Auto, mac));
        assert!(!prefers_fp32_for(Ep::Auto, cpu_only));
        // An implicit `auto` (DOCLING_RS_EP unset) leaves CoreML out, so a
        // macOS build's default stays on the int8 CPU path.
        let mac_default = |ep: Ep| auto_member(ep, mac, false);
        assert!(!prefers_fp32_for(Ep::Auto, mac_default));
        assert!(prefers_fp32_for(Ep::Auto, |ep| auto_member(ep, mac, true)));
        // Batched layout only for the CUDA class: CoreML needs the static,
        // per-page graph to be handed anything at all.
        assert!(!prefers_batching_for(Ep::CoreMl, mac));
        assert!(!prefers_batching_for(Ep::Auto, mac));
        assert!(!prefers_batching_for(Ep::Auto, cpu_only));
        assert!(!prefers_batching_for(Ep::Cpu, cuda));
        assert!(!prefers_batching_for(Ep::Xnnpack, cuda));
        assert!(prefers_batching_for(Ep::Auto, cuda));
        assert!(!prefers_batching_for(Ep::DirectMl, cuda));
        assert!(!prefers_batching_for(Ep::Auto, |ep| ep == Ep::DirectMl));
        for ep in [Ep::Cuda, Ep::TensorRt] {
            assert!(prefers_batching_for(ep, cuda), "{ep:?}");
        }
    }

    /// #602: CoreML is opt-in. With `DOCLING_RS_EP` unset a CoreML build
    /// stays on CPU, while CUDA-class (and XNNPACK) builds keep defaulting
    /// to `auto`; a named `auto` registers CoreML, the implicit one does not.
    #[test]
    fn coreml_is_opt_in() {
        let cpu_only = |ep: Ep| matches!(ep, Ep::Cpu | Ep::Auto);
        let mac = |ep: Ep| matches!(ep, Ep::Cpu | Ep::Auto | Ep::CoreMl);
        let mac_xnn = |ep: Ep| matches!(ep, Ep::Cpu | Ep::Auto | Ep::CoreMl | Ep::Xnnpack);
        let cuda = |ep: Ep| matches!(ep, Ep::Cpu | Ep::Auto | Ep::Cuda);
        assert_eq!(default_choice_for(cpu_only), Ep::Cpu);
        assert_eq!(default_choice_for(mac), Ep::Cpu);
        assert_eq!(default_choice_for(cuda), Ep::Auto);
        assert_eq!(default_choice_for(mac_xnn), Ep::Auto);
        assert!(!auto_member(Ep::CoreMl, mac_xnn, false));
        assert!(auto_member(Ep::Xnnpack, mac_xnn, false));
        assert!(auto_member(Ep::CoreMl, mac, true));
        assert!(auto_member(Ep::Cuda, cuda, false));
        assert!(!auto_member(Ep::Cuda, mac, true));
        assert!(!auto_member(Ep::Cpu, mac, true));
        assert!(!auto_member(Ep::Auto, mac, true));
    }

    /// The binding-facing list names exactly the compiled-in providers, by
    /// the names [`parse`] reads back.
    #[test]
    fn compiled_providers_round_trip_through_parse() {
        let names = compiled_providers();
        assert_eq!(names.first(), Some(&"cpu"));
        for name in &names {
            assert!(
                compiled(parse(name).expect("a DOCLING_RS_EP name")),
                "{name}"
            );
        }
        assert_eq!(names.contains(&"coreml"), cfg!(feature = "coreml"));
        assert_eq!(names.contains(&"cuda"), cfg!(feature = "cuda"));
    }

    /// #602: NeuralNetwork is the CoreML default (CPU-identical output);
    /// MLProgram is an explicit opt-in; anything else is reported.
    #[test]
    fn coreml_format_defaults_to_neuralnetwork() {
        assert_eq!(coreml_format(None), Ok(CoreMlFormat::NeuralNetwork));
        assert_eq!(
            coreml_format(Some(" NeuralNetwork ")),
            Ok(CoreMlFormat::NeuralNetwork)
        );
        assert_eq!(
            coreml_format(Some("MLProgram")),
            Ok(CoreMlFormat::MlProgram)
        );
        assert_eq!(coreml_format(Some("onnx")), Err("onnx".into()));
    }

    /// #452: the creation lock exists only for GPU providers. A CPU-only build
    /// has nothing to serialize (the OCR lanes keep their parallel start-up);
    /// with a GPU feature compiled in, the guard is a real lock — taken and
    /// released per creation, never held across two of them.
    #[test]
    fn creation_guard_matches_the_provider_choice() {
        let g = creation_guard();
        assert_eq!(g.is_some(), dispatches().is_some());
        drop(g);
        // A second acquisition must not deadlock — a poisoned or held lock
        // would turn every later model load into a hang.
        let g = creation_guard();
        assert_eq!(g.is_some(), dispatches().is_some());
    }

    #[test]
    fn version_strings_parse() {
        assert_eq!(parse_version("1.28.0"), Some((1, 28)));
        assert_eq!(parse_version(" 1.30.1\n"), Some((1, 30)));
        assert_eq!(parse_version("1"), None);
        assert_eq!(parse_version("dev"), None);
    }

    /// The linked runtime reports a version, and it is one `ort` accepts.
    #[test]
    #[cfg(not(feature = "load-dynamic"))]
    fn linked_runtime_reports_its_version() {
        let (major, minor) = runtime_version().expect("version string");
        assert_eq!(major, 1);
        assert!(
            minor >= ort::MINOR_VERSION,
            "1.{minor} < 1.{}",
            ort::MINOR_VERSION
        );
    }

    #[test]
    fn parse_known_names_and_aliases() {
        assert_eq!(parse(""), Some(Ep::Cpu));
        assert_eq!(parse("cpu"), Some(Ep::Cpu));
        assert_eq!(parse("CUDA"), Some(Ep::Cuda));
        assert_eq!(parse(" cuda "), Some(Ep::Cuda));
        assert_eq!(parse("tensorrt"), Some(Ep::TensorRt));
        assert_eq!(parse("trt"), Some(Ep::TensorRt));
        assert_eq!(parse("directml"), Some(Ep::DirectMl));
        assert_eq!(parse("dml"), Some(Ep::DirectMl));
        assert_eq!(parse("CoreML"), Some(Ep::CoreMl));
        assert_eq!(parse("xnnpack"), Some(Ep::Xnnpack));
        assert_eq!(parse("auto"), Some(Ep::Auto));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(parse("gpu"), None);
        assert_eq!(parse("rocm"), None);
        assert_eq!(parse("cuda:0"), None);
    }

    #[test]
    fn cpu_and_auto_are_always_compiled() {
        // `choice()` relies on this to keep the unreachable!() arm honest.
        assert!(compiled(Ep::Cpu));
        assert!(compiled(Ep::Auto));
    }

    #[test]
    fn unset_defaults_to_auto_exactly_in_gpu_builds() {
        // CI's ep-features matrix runs this with each GPU feature on, the
        // plain test job with none — both arms get exercised. CoreML alone
        // is no GPU build for this purpose: it is opt-in (#602).
        #[cfg(any(
            feature = "cuda",
            feature = "tensorrt",
            feature = "directml",
            feature = "xnnpack"
        ))]
        assert_eq!(default_choice_for(compiled), Ep::Auto);
        #[cfg(not(any(
            feature = "cuda",
            feature = "tensorrt",
            feature = "directml",
            feature = "xnnpack"
        )))]
        assert_eq!(default_choice_for(compiled), Ep::Cpu);
    }
}

#[cfg(test)]
mod cache_guard_tests {
    use super::onnx_file_complete;

    fn write(bytes: &[u8]) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "docling-onnx-guard-{}-{}.onnx",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// A ModelProto-shaped message: `ir_version = 7` (field 1, varint), then a
    /// `graph` (field 7, length-delimited) of 300 bytes — long enough for a
    /// two-byte length prefix like the real thing.
    fn model_like() -> Vec<u8> {
        let mut v = vec![0x08, 0x07, 0x3a, 0xac, 0x02];
        v.extend(std::iter::repeat_n(0x55u8, 300));
        v
    }

    #[test]
    fn complete_file_passes_truncated_fails() {
        let full = model_like();
        assert!(onnx_file_complete(&write(&full)));
        // cut inside the graph body — what a disk-full write leaves behind
        assert!(!onnx_file_complete(&write(&full[..full.len() - 1])));
        assert!(!onnx_file_complete(&write(&full[..40])));
        // cut inside the length prefix itself
        assert!(!onnx_file_complete(&write(&full[..4])));
        assert!(!onnx_file_complete(&write(&[])));
        // trailing garbage that is not a valid field key/wire type
        let mut bad = full.clone();
        bad.push(0x07);
        assert!(!onnx_file_complete(&write(&bad)));
    }

    #[test]
    fn real_optimized_graph_passes() {
        // Any real ONNX file in the repo's model dir, when present.
        for p in [
            "../../.models/tableformer/bbox.onnx",
            "../../.models/layout_heron_int8.onnx",
        ] {
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(p);
            if p.is_file() {
                assert!(onnx_file_complete(&p), "{}", p.display());
            }
        }
    }

    /// `load-dynamic` with no library on the machine: a plain error naming
    /// the lookup, never `ort`'s panic — what every ML stage degrades on
    /// (#504). The first call pins the process-wide result, so this is the
    /// only test that touches `runtime_library`.
    #[cfg(feature = "load-dynamic")]
    #[test]
    fn missing_runtime_library_is_an_error_not_a_panic() {
        std::env::set_var(
            "ORT_DYLIB_PATH",
            "/nonexistent/docling-rs/libonnxruntime.so",
        );
        let err = super::runtime_library().expect_err("no library at that path");
        assert!(err.contains("ONNX Runtime library unavailable"), "{err}");
        assert!(
            err.contains("/nonexistent/docling-rs/libonnxruntime.so"),
            "{err}"
        );
        assert!(err.contains("ORT_DYLIB_PATH"), "{err}");
        let err2 = super::session_builder()
            .err()
            .expect("builder goes through the loader");
        assert_eq!(err, err2);
    }
}

#[cfg(test)]
mod cuda_option_tests {
    use super::{
        cuda_options_from, parse_dml_device, ArenaExtend, ConvAlgo, CudaOptions, PROBE_MODEL,
    };
    use std::collections::HashMap;

    fn parse(vars: &[(&str, &str)]) -> (CudaOptions, Vec<String>) {
        let env: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut warnings = Vec::new();
        let o = cuda_options_from(|name| env.get(name).cloned(), |m| warnings.push(m));
        (o, warnings)
    }

    #[test]
    fn defaults_are_onnx_runtimes() {
        let (o, warnings) = parse(&[]);
        assert_eq!(o, CudaOptions::default());
        assert_eq!(o.device, 0);
        assert_eq!(o.mem_limit_mb, None);
        assert_eq!(o.arena_extend, ArenaExtend::NextPowerOfTwo);
        assert_eq!(o.conv_algo, ConvAlgo::Exhaustive);
        assert!(o.conv_max_workspace);
        assert!(warnings.is_empty());
    }

    #[test]
    fn dml_device_parses() {
        assert_eq!(parse_dml_device(None), Ok(None));
        assert_eq!(parse_dml_device(Some(" 1 ".into())), Ok(Some(1)));
        assert!(parse_dml_device(Some("-1".into())).is_err());
        assert!(parse_dml_device(Some("igpu".into())).is_err());
    }

    #[test]
    fn every_knob_parses() {
        let (o, warnings) = parse(&[
            ("DOCLING_RS_CUDA_DEVICE", "1"),
            ("DOCLING_RS_CUDA_MEM_LIMIT_MB", "4096"),
            ("DOCLING_RS_CUDA_ARENA_EXTEND", "Same_As_Requested"),
            ("DOCLING_RS_CUDA_CONV_ALGO_SEARCH", "heuristic"),
            ("DOCLING_RS_CUDA_CONV_MAX_WORKSPACE", "0"),
        ]);
        assert_eq!(o.device, 1);
        assert_eq!(o.mem_limit_mb, Some(4096));
        assert_eq!(o.arena_extend, ArenaExtend::SameAsRequested);
        assert_eq!(o.conv_algo, ConvAlgo::Heuristic);
        assert!(!o.conv_max_workspace);
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_zero_limit_means_unbounded() {
        let (o, warnings) = parse(&[("DOCLING_RS_CUDA_MEM_LIMIT_MB", "0")]);
        assert_eq!(o.mem_limit_mb, None);
        assert!(warnings.is_empty());
    }

    #[test]
    fn bad_values_warn_and_keep_the_default() {
        let (o, warnings) = parse(&[
            ("DOCLING_RS_CUDA_DEVICE", "-1"),
            ("DOCLING_RS_CUDA_MEM_LIMIT_MB", "lots"),
            ("DOCLING_RS_CUDA_ARENA_EXTEND", "sometimes"),
            ("DOCLING_RS_CUDA_CONV_ALGO_SEARCH", "fastest"),
        ]);
        assert_eq!(o, CudaOptions::default());
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        // Each names the variable and the value it rejected.
        assert!(warnings.iter().all(|w| w.contains("=\"")), "{warnings:?}");
    }

    /// The hand-encoded probe graph is a model ONNX Runtime accepts (on the
    /// CPU provider here) and runs as the identity it claims to be.
    #[test]
    fn probe_model_is_a_valid_identity_graph() {
        let Ok(builder) = super::session_builder() else {
            return; // no runtime library on this host (load-dynamic)
        };
        let mut builder = builder;
        let mut session = builder
            .commit_from_memory(PROBE_MODEL)
            .expect("the probe model loads");
        let x = ort::value::Tensor::from_array(([1usize], vec![42.0f32])).unwrap();
        let out = session.run(ort::inputs!["x" => x]).expect("identity runs");
        let (_, y) = out["y"].try_extract_tensor::<f32>().unwrap();
        assert_eq!(y, &[42.0]);
    }

    /// A preallocated output over a borrowed buffer is written in place, and
    /// the buffer is the caller's again once the run's values are dropped.
    #[test]
    fn borrowed_output_is_written_in_place() {
        let Ok(builder) = super::session_builder() else {
            return; // no runtime library on this host (load-dynamic)
        };
        let mut builder = builder;
        let mut session = builder
            .commit_from_memory(PROBE_MODEL)
            .expect("the probe model loads");
        let mut buf = vec![0.0f32; 4];
        {
            // SAFETY: `buf` is untouched until `opts` and `out` are dropped.
            let y = unsafe { super::borrowed_f32(&mut buf, &[1], session.allocator()) }.unwrap();
            let opts = super::run_options_with_outputs(
                ort::session::OutputSelector::default().preallocate("y", y),
            )
            .unwrap();
            let x = ort::value::Tensor::from_array(([1usize], vec![7.5f32])).unwrap();
            let out = session
                .run_with_options(ort::inputs!["x" => x], &opts)
                .expect("identity runs");
            drop(out);
        }
        assert_eq!(buf, [7.5, 0.0, 0.0, 0.0]);
        let mut short = vec![0.0f32; 1];
        // SAFETY: never run; only the size check is exercised.
        assert!(unsafe { super::borrowed_f32(&mut short, &[2], session.allocator()) }.is_err());
    }
}
