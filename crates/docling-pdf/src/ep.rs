//! Execution-provider selection for the ONNX sessions (#74).
//!
//! CPU is the default and the only provider in a default build — the GPU
//! providers exist behind cargo features (`cuda`, `tensorrt`, `directml`,
//! `coreml`) so the standard build keeps zero GPU dependencies. A feature only
//! *compiles* a provider in (it makes `ort` link/download an ONNX Runtime
//! binary that contains that EP); which provider actually runs is chosen at
//! startup from `DOCLING_RS_EP`:
//!
//! * unset — `auto` in a build that compiled any GPU provider in (you chose
//!   a GPU build or installed the GPU wheel: use the GPU when one is usable,
//!   fall back to CPU when not); plain CPU in a default build
//! * `cpu` — force CPU, exactly the pre-#74 behavior
//! * `cuda` / `tensorrt` (`trt`) / `directml` (`dml`) / `coreml` — that
//!   provider, registered with error-on-failure: an *explicitly requested*
//!   accelerator that can't initialize (missing driver, no device) fails the
//!   session load loudly instead of silently degrading to a CPU run that
//!   looks fine but is 10× slower than expected. Requesting a provider the
//!   binary wasn't compiled with warns once and stays on CPU (there is
//!   nothing to register at all in that case).
//! * `auto` — every compiled-in provider is registered in performance order
//!   (TensorRT, CUDA, CoreML, DirectML) and ONNX Runtime falls back down the
//!   list — ultimately to CPU — at session creation. The "try GPU if there is
//!   one" mode for images built once and deployed on mixed fleets.
//!
//! Every session in this crate (layout, TableFormer×3, OCR recognition, both
//! enrichment models) routes through [`apply`], so one env var switches the
//! whole pipeline. The int8 model defaults are skipped whenever a GPU
//! provider is selected ([`prefers_fp32`]): the int8 exports are QDQ graphs
//! calibrated for CPU kernels — on GPU they only add de-quantize traffic and
//! were never conformance-validated there, while fp32 is (see
//! docs/PDF_CONFORMANCE.md).

use std::sync::OnceLock;

use ort::ep::ExecutionProviderDispatch;
use ort::session::builder::SessionBuilder;

/// The parsed `DOCLING_RS_EP` choice. Named GPU variants are only ever
/// *selected* (returned by [`choice`]) when their cargo feature is compiled
/// in; [`parse`] itself is feature-blind so it can be unit-tested everywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ep {
    Cpu,
    Cuda,
    TensorRt,
    DirectMl,
    CoreMl,
    /// Register everything compiled in, let ONNX Runtime fall back.
    Auto,
}

/// Parse a `DOCLING_RS_EP` value. `None` for values that name no known
/// provider (the caller warns and stays on CPU).
pub(crate) fn parse(v: &str) -> Option<Ep> {
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "cpu" => Some(Ep::Cpu),
        "cuda" => Some(Ep::Cuda),
        "tensorrt" | "trt" => Some(Ep::TensorRt),
        "directml" | "dml" => Some(Ep::DirectMl),
        "coreml" => Some(Ep::CoreMl),
        "auto" => Some(Ep::Auto),
        _ => None,
    }
}

/// Is this provider compiled into the binary (cargo feature enabled)?
fn compiled(ep: Ep) -> bool {
    match ep {
        Ep::Cpu => true,
        Ep::Cuda => cfg!(feature = "cuda"),
        Ep::TensorRt => cfg!(feature = "tensorrt"),
        Ep::DirectMl => cfg!(feature = "directml"),
        Ep::CoreMl => cfg!(feature = "coreml"),
        Ep::Auto => true,
    }
}

fn any_gpu_compiled() -> bool {
    [Ep::Cuda, Ep::TensorRt, Ep::DirectMl, Ep::CoreMl]
        .into_iter()
        .any(compiled)
}

/// The choice when `DOCLING_RS_EP` is unset (or empty): a build that
/// compiled a GPU provider in defaults to `auto` — whoever built with
/// `--features cuda` (or installed the `docling-rs-cuda` wheel) wants the
/// GPU used when one is usable, and `auto`'s per-session registration
/// falls back to CPU when not. A default build has nothing to register and
/// stays on the exact pre-#74 CPU path.
fn default_choice() -> Ep {
    if any_gpu_compiled() {
        Ep::Auto
    } else {
        Ep::Cpu
    }
}

/// The effective provider choice for this process, resolved once. Invalid or
/// not-compiled-in requests degrade to CPU with a single stderr warning —
/// same convention as a missing model file.
pub(crate) fn choice() -> Ep {
    static CHOICE: OnceLock<Ep> = OnceLock::new();
    *CHOICE.get_or_init(|| {
        let raw = std::env::var("DOCLING_RS_EP").unwrap_or_default();
        if raw.trim().is_empty() {
            return default_choice();
        }
        let Some(ep) = parse(&raw) else {
            eprintln!(
                "docling-pdf: DOCLING_RS_EP={raw:?} names no known execution provider \
                 (cpu|cuda|tensorrt|directml|coreml|auto); using CPU"
            );
            return Ep::Cpu;
        };
        if !compiled(ep) {
            eprintln!(
                "docling-pdf: DOCLING_RS_EP={raw:?} requested, but this binary was built \
                 without that provider — rebuild with `--features {}`; using CPU",
                match ep {
                    Ep::Cuda => "cuda",
                    Ep::TensorRt => "tensorrt",
                    Ep::DirectMl => "directml",
                    Ep::CoreMl => "coreml",
                    Ep::Cpu | Ep::Auto => unreachable!("always compiled"),
                }
            );
            return Ep::Cpu;
        }
        ep
    })
}

/// True when the int8 model defaults should be skipped in favor of fp32
/// because inference is (or may be) leaving the CPU. `Auto` counts as GPU as
/// soon as any GPU provider is compiled in: whether registration succeeds is
/// only known per-session, and a CPU fall-back running fp32 is merely the
/// pre-int8 speed, while a GPU running the CPU-calibrated int8 graph is a
/// conformance risk.
pub(crate) fn prefers_fp32() -> bool {
    match choice() {
        Ep::Cpu => false,
        Ep::Cuda | Ep::TensorRt | Ep::DirectMl | Ep::CoreMl => true,
        Ep::Auto => any_gpu_compiled(),
    }
}

/// The dispatch list for the current choice. `None` means "register nothing"
/// (CPU — leave the builder untouched, the pre-#74 code path).
fn dispatches() -> Option<Vec<ExecutionProviderDispatch>> {
    // The `ort::ep::*` structs exist regardless of cargo features (features
    // gate the ONNX Runtime *binary*, not the Rust API), so no cfg-gating is
    // needed here: `choice()` already guarantees a named provider is compiled.
    let d = match choice() {
        Ep::Cpu => return None,
        Ep::Cuda => vec![ort::ep::CUDA::default().build().error_on_failure()],
        Ep::TensorRt => vec![ort::ep::TensorRT::default().build().error_on_failure()],
        Ep::DirectMl => vec![ort::ep::DirectML::default().build().error_on_failure()],
        Ep::CoreMl => vec![ort::ep::CoreML::default().build().error_on_failure()],
        Ep::Auto => {
            let mut v = Vec::new();
            if cfg!(feature = "tensorrt") {
                v.push(ort::ep::TensorRT::default().build());
            }
            if cfg!(feature = "cuda") {
                v.push(ort::ep::CUDA::default().build());
            }
            if cfg!(feature = "coreml") {
                v.push(ort::ep::CoreML::default().build());
            }
            if cfg!(feature = "directml") {
                v.push(ort::ep::DirectML::default().build());
            }
            if v.is_empty() {
                return None; // CPU-only build: auto ≡ cpu
            }
            v
        }
    };
    Some(d)
}

/// Register the selected execution providers on a session builder. Called by
/// every session in this crate; a no-op (and infallible) in the default
/// CPU configuration.
pub(crate) fn apply(builder: SessionBuilder) -> Result<SessionBuilder, String> {
    let Some(eps) = dispatches() else {
        return Ok(builder);
    };
    static LOGGED: OnceLock<()> = OnceLock::new();
    LOGGED.get_or_init(|| {
        if crate::timing::enabled() {
            eprintln!("docling-pdf: execution providers: {eps:?}");
        }
    });
    builder
        .with_execution_providers(eps)
        .map_err(|e| format!("execution provider registration: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // plain test job with none — both arms get exercised.
        #[cfg(any(
            feature = "cuda",
            feature = "tensorrt",
            feature = "directml",
            feature = "coreml"
        ))]
        assert_eq!(default_choice(), Ep::Auto);
        #[cfg(not(any(
            feature = "cuda",
            feature = "tensorrt",
            feature = "directml",
            feature = "coreml"
        )))]
        assert_eq!(default_choice(), Ep::Cpu);
    }
}
