#!/usr/bin/env python3
"""Write the execution record of a models-release publish run (#955, #961).

`export_record.json` is uploaded beside the model assets by
publish-models.yml and linked from `models.json` (`execution`) by
scripts/ci/models_manifest.sh. It records *how* the run produced the bytes
it staged — what a source revision and a script name alone do not pin:

- the workflow run (URL, id, attempt, commit, actor) and the runner (image,
  OS, CPU — the int8 accuracy gate infers on that CPU);
- one **stage** per export / quantization / repack step (#961): its
  outcome as GitHub reports it (`success` / `failure` / `skipped`), the
  Python and the version of every package installed *at that moment* —
  the workflow re-pins between steps (ONNX 1.23 for the layout export,
  1.22 for the quantizations), so one snapshot at the end described the
  last environment only — the sha256 of the upstream input files the
  stage read (the Hugging Face hub cache the export scripts populate at
  the revisions scripts/install/models_catalog.json pins), of the local
  model files a quantization or repack consumed, and of every file the
  stage produced;
- the catalog's inputs and the staged assets, each bound to the stage
  that read or produced it. An input the catalog names but the stage did
  not fetch is recorded as exactly that (`status: not_fetched`), an input
  of a stage that was skipped or failed as `stage_skipped` /
  `stage_failed`, and a stage the workflow did not record as
  `unrecorded`: the record states execution evidence, it never infers a
  skip from a missing cache file.

Each step runs the `stage` subcommand right after its command, in the same
interpreter and package set::

    python scripts/ci/export_record.py stage --dir release-stages \\
        --name export_layout --kind export --artifacts layout_heron \\
        --outcome "${{ steps.layout-export.outcome }}" \\
        --outputs .models/layout_heron.onnx

and the run ends by merging the stage snapshots into the record::

    python scripts/ci/export_record.py --tag models-v1 --assets release-assets \\
        --stages release-stages --out release-assets/export_record.json

A workflow run's logs and artifacts expire; a release asset does not, so
the record outlives the run it describes. Nothing in it is a secret: the
environment variables read are GitHub's public run coordinates.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import importlib.metadata
import json
import os
import platform
import sys
from pathlib import Path

CATALOG = Path(__file__).resolve().parents[1] / "install" / "models_catalog.json"
SCHEMA = 2

# Every package an export / quantization / repack step may run with; the
# ones not installed are left out of the record.
PACKAGES = [
    "torch",
    "torchvision",
    "transformers",
    "tokenizers",
    "safetensors",
    "huggingface_hub",
    "docling_ibm_models",
    "onnx",
    "onnx_ir",
    "onnxscript",
    "onnxruntime",
    "numpy",
    "sympy",
    "pillow",
    "pypdfium2",
    "opencv-python-headless",
    "protobuf",
]

STAGE_KINDS = ("export", "quantization", "repack")
OUTCOMES = ("success", "failure", "skipped", "cancelled")
HF_PREFIX = "https://huggingface.co/"


def now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def hub_cache() -> Path:
    try:
        from huggingface_hub import constants  # type: ignore

        return Path(constants.HF_HUB_CACHE)
    except Exception:  # noqa: BLE001 — any import failure: the default layout
        home = os.environ.get("HF_HOME")
        if os.environ.get("HF_HUB_CACHE"):
            return Path(os.environ["HF_HUB_CACHE"])
        return Path(home) / "hub" if home else Path.home() / ".cache" / "huggingface" / "hub"


def cpu_model() -> str | None:
    try:
        with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as f:
            for line in f:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or None


def int8_isa() -> list[str]:
    try:
        with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as f:
            flags = next((l for l in f if l.startswith("flags")), "")
    except OSError:
        return []
    return sorted(x for x in ("avx512_vnni", "avx_vnni", "avx2") if x in flags.split())


def workflow_record() -> dict:
    env = os.environ.get
    server, repo, run_id = env("GITHUB_SERVER_URL"), env("GITHUB_REPOSITORY"), env("GITHUB_RUN_ID")
    url = f"{server}/{repo}/actions/runs/{run_id}" if server and repo and run_id else None
    return {
        "name": env("GITHUB_WORKFLOW"),
        "run_id": int(run_id) if run_id else None,
        "run_attempt": int(env("GITHUB_RUN_ATTEMPT")) if env("GITHUB_RUN_ATTEMPT") else None,
        "url": url,
        "repository": f"{server}/{repo}" if server and repo else None,
        "commit": env("GITHUB_SHA"),
        "ref": env("GITHUB_REF_NAME"),
        "actor": env("GITHUB_ACTOR"),
    }


def runner_record() -> dict:
    env = os.environ.get
    return {
        "os": env("RUNNER_OS") or platform.system(),
        "arch": env("RUNNER_ARCH") or platform.machine(),
        "image": f"{env('ImageOS')} {env('ImageVersion')}".strip() if env("ImageOS") else None,
        "cpu": cpu_model(),
        "int8_isa": int8_isa(),
    }


def package_versions() -> dict[str, str]:
    out: dict[str, str] = {}
    for name in PACKAGES:
        try:
            out[name] = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            continue
    return out


def environment_record() -> dict:
    return {"python": platform.python_version(), "packages": package_versions()}


def file_record(path: Path) -> dict:
    """A local file as the record states it: hashed when present, named
    with `exists: false` when the step did not produce it (an optional
    sidecar, a quantization its gate rejected)."""
    entry: dict = {"path": str(path)}
    if path.is_file():
        real = path.resolve()
        entry.update(exists=True, sha256=sha256_of(real), size=real.stat().st_size)
    else:
        entry["exists"] = False
    return entry


def catalog_inputs(catalog: dict, artifact_ids: list[str]) -> list[dict]:
    """The upstream files the catalog names for these artifacts, one entry
    per (repository, revision, file)."""
    seen: set[tuple[str, str, str]] = set()
    out: list[dict] = []
    for art in catalog["artifacts"]:
        if art["id"] not in artifact_ids:
            continue
        up = art["upstream"]
        for rel in up.get("files", []):
            key = (up["repository"], up["revision"], rel)
            if key in seen:
                continue
            seen.add(key)
            out.append({"artifact": art["id"], "repository": up["repository"], "revision": up["revision"], "file": rel})
    return out


def hub_file(cache: Path, repository: str, revision: str, rel: str) -> Path | None:
    if not repository.startswith(HF_PREFIX):
        return None
    repo_id = repository[len(HF_PREFIX) :].strip("/")
    return cache / f"models--{repo_id.replace('/', '--')}" / "snapshots" / revision / rel


def hash_hub_inputs(entries: list[dict], cache: Path) -> list[dict]:
    """Hash each catalog input from the hub cache — right after the stage
    that reads them, so the digest is of the bytes that stage consumed.
    `present: false` says only that the file is not in the cache now; what
    that means is decided with the stage's outcome in the final record."""
    out = []
    for e in entries:
        e = dict(e)
        path = hub_file(cache, e["repository"], e["revision"], e["file"])
        if path is None:
            e.update(present=None, sha256=None, note="not a Hugging Face file: hashed by its own fetch step, see the workflow log")
        elif path.is_file():
            real = path.resolve()
            e.update(present=True, sha256=sha256_of(real), size=real.stat().st_size)
        else:
            e.update(present=False, sha256=None)
        out.append(e)
    return out


def with_sidecars(paths: list[str]) -> list[Path]:
    """An ONNX graph and its `.onnx.data` external-weights sidecar are one
    artifact; a sidecar that exists beside a listed graph is recorded too."""
    out: list[Path] = []
    for p in paths:
        path = Path(p)
        out.append(path)
        sidecar = Path(str(path) + ".data")
        if path.suffix == ".onnx" and sidecar.is_file() and sidecar not in out:
            out.append(sidecar)
    return out


def cmd_stage(args: argparse.Namespace) -> int:
    catalog = json.loads(args.catalog.read_text(encoding="utf-8"))
    known = {a["id"] for a in catalog["artifacts"]}
    unknown = [a for a in args.artifacts if a not in known]
    if unknown:
        print(f"error: artifacts not in {args.catalog}: {', '.join(unknown)}", file=sys.stderr)
        return 2
    stage = {
        "name": args.name,
        "kind": args.kind,
        "artifacts": args.artifacts,
        "tool": next((a.get("tool") for a in catalog["artifacts"] if a["id"] == args.artifacts[0]), None),
        "outcome": args.outcome,
        "dependencies_install": args.deps_outcome,
        "recorded_at": now(),
        "environment": environment_record(),
        # An export reads upstream files from the hub cache; a quantization
        # or repack reads the local graphs an earlier stage produced
        # (--inputs), and its catalog entries name the upstream files only
        # for lineage, so they are not re-hashed here.
        "upstream_inputs": hash_hub_inputs(catalog_inputs(catalog, args.artifacts), hub_cache()) if args.kind == "export" else [],
        "inputs": [file_record(p) for p in with_sidecars(args.inputs)],
        "outputs": [file_record(p) for p in with_sidecars(args.outputs)],
    }
    args.dir.mkdir(parents=True, exist_ok=True)
    out = args.dir / f"{args.name}.json"
    out.write_text(json.dumps(stage, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    produced = sum(1 for o in stage["outputs"] if o["exists"])
    print(
        f"{out}: {args.kind} {args.name} outcome={args.outcome}, {produced}/{len(stage['outputs'])} outputs present, "
        f"{len(stage['environment']['packages'])} package versions",
        file=sys.stderr,
    )
    return 0


def load_stages(stages_dir: Path | None) -> list[dict]:
    if stages_dir is None or not stages_dir.is_dir():
        return []
    stages = [json.loads(p.read_text(encoding="utf-8")) for p in sorted(stages_dir.glob("*.json"))]
    return sorted(stages, key=lambda s: s.get("recorded_at", ""))


def bound_inputs(catalog: dict, stages: list[dict], cache: Path) -> list[dict]:
    """The catalog's inputs of every `export` artifact, each bound to the
    stage that read it, with a `status` that states the evidence:

    - `hashed`: the stage ran and the file was in the hub cache after it;
    - `not_fetched`: the stage ran (outcome success) and the file was not
      in the hub cache after it — this run did not download it (the export
      does not read that file); nothing about the stage is inferred;
    - `stage_failed` / `stage_skipped` / `stage_cancelled`: the step's own
      outcome, so no export produced; the file may or may not be cached;
    - `unrecorded`: no stage snapshot names this artifact — the workflow
      did not record the step (the record is older than the stage
      snapshots, or the step was added without one); the file is hashed
      from the cache if present, but which step read it is unknown;
    - `external`: not a Hugging Face file; its own fetch step hashes it.
    """
    export_ids = [a["id"] for a in catalog["artifacts"] if a.get("provenance") == "export"]
    by_artifact: dict[str, dict] = {}
    for s in stages:
        for a in s.get("artifacts", []):
            by_artifact.setdefault(a, s)
    out = []
    for e in catalog_inputs(catalog, export_ids):
        stage = by_artifact.get(e["artifact"])
        e = dict(e)
        e["stage"] = stage["name"] if stage else None
        if hub_file(cache, e["repository"], e["revision"], e["file"]) is None:
            e.update(status="external", sha256=None, note="not a Hugging Face file: hashed by its own fetch step, see the workflow log")
            out.append(e)
            continue
        snap = None
        if stage:
            snap = next(
                (u for u in stage.get("upstream_inputs", []) if (u["repository"], u["revision"], u["file"]) == (e["repository"], e["revision"], e["file"])),
                None,
            )
        if stage is None:
            fresh = hash_hub_inputs([e], cache)[0]
            e.update(
                status="unrecorded",
                sha256=fresh["sha256"],
                note="no stage snapshot names this artifact: the workflow did not record the step that reads it"
                + (" (hashed from the hub cache when the record was written)" if fresh["sha256"] else " and the file is not in the hub cache"),
            )
            if fresh.get("size") is not None:
                e["size"] = fresh["size"]
        elif stage["outcome"] != "success":
            e.update(
                status=f"stage_{stage['outcome']}",
                sha256=snap["sha256"] if snap else None,
                note=f"stage {stage['name']} outcome: {stage['outcome']}"
                + (f" (its dependency install: {stage['dependencies_install']})" if stage.get("dependencies_install") else "")
                + " — no export produced",
            )
        elif snap and snap.get("present"):
            e.update(status="hashed", sha256=snap["sha256"], size=snap.get("size"))
        else:
            e.update(
                status="not_fetched",
                sha256=None,
                note=f"stage {stage['name']} ran (outcome success) and the file was not in the hub cache after it: this run did not download it",
            )
        out.append(e)
    return out


def staged_outputs(assets: Path, skip: str, stages: list[dict]) -> list[dict]:
    """Every staged asset, bound by digest to the stage that produced it
    (`stage: null` for a re-hosted or externally built asset)."""
    producer: dict[str, str] = {}
    for s in stages:
        for o in s.get("outputs", []):
            if o.get("sha256"):
                producer.setdefault(o["sha256"], s["name"])
    out = []
    for p in sorted(assets.iterdir()):
        if not p.is_file() or p.name == skip:
            continue
        digest = sha256_of(p)
        out.append({"asset": p.name, "sha256": digest, "size": p.stat().st_size, "stage": producer.get(digest)})
    return out


def cmd_record(args: argparse.Namespace) -> int:
    catalog = json.loads(args.catalog.read_text(encoding="utf-8"))
    stages = load_stages(args.stages)
    record = {
        "$comment": (
            "Execution record of the publish-models.yml run that produced the export, quantization "
            "and repack assets of this models release and fetched its re-hosted ones (#955, #961): the run, "
            "the runner, one stage per conversion step with its outcome, the toolchain installed when it "
            "ran and the digests of what it read and produced, and the catalog inputs and staged assets "
            "bound to those stages. models.json links it as `execution`; the upstream lineage of each "
            "model is the catalog's `lineage`."
        ),
        "schema": SCHEMA,
        "release": args.tag,
        "generated": now(),
        "workflow": workflow_record(),
        "runner": runner_record(),
        # The interpreter this record was written with, after every stage —
        # kept for the packages no stage installs (the record step's own);
        # the environment a conversion ran with is its stage's.
        "environment": environment_record(),
        "stages": stages,
        "inputs": bound_inputs(catalog, stages, hub_cache()),
        "outputs": staged_outputs(args.assets, args.out.name, stages),
    }
    if not stages:
        record["stages_note"] = (
            "no stage snapshots were given (--stages): the step environments and outcomes of this run "
            "are not recorded; inputs are `unrecorded`, outputs unbound"
        )
    args.out.write_text(json.dumps(record, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    statuses = sorted({i["status"] for i in record["inputs"]})
    print(
        f"{args.out}: run {record['workflow']['url'] or '(local)'}, {len(stages)} stages, "
        f"{len(record['inputs'])} inputs ({', '.join(statuses) or 'none'}), {len(record['outputs'])} outputs",
        file=sys.stderr,
    )
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--catalog", type=Path, default=CATALOG)
    sub = ap.add_subparsers(dest="command")

    st = sub.add_parser("stage", help="snapshot one export / quantization / repack step, right after it ran")
    st.add_argument("--dir", required=True, type=Path, help="where the stage snapshots collect")
    st.add_argument("--name", required=True, help="the stage's name (one snapshot file per name)")
    st.add_argument("--kind", required=True, choices=STAGE_KINDS)
    st.add_argument("--artifacts", required=True, nargs="+", help="catalog artifact ids this stage produces")
    st.add_argument("--outcome", required=True, choices=OUTCOMES, help="the step's outcome as GitHub reports it")
    st.add_argument("--deps-outcome", choices=OUTCOMES, help="the outcome of the step that installed its dependencies")
    st.add_argument("--inputs", nargs="*", default=[], help="local files the stage read (a quantization's fp32 graph)")
    st.add_argument("--outputs", nargs="*", default=[], help="files the stage was to produce")

    # The record itself is the default command, so the original invocation
    # (`export_record.py --tag … --assets … --out …`) keeps working.
    for p in (ap, sub.add_parser("record", help="write export_record.json from the stage snapshots")):
        p.add_argument("--tag", help="the models release tag this run publishes into")
        p.add_argument("--assets", type=Path, help="the staged release assets directory")
        p.add_argument("--out", type=Path, help="where to write export_record.json")
        p.add_argument("--stages", type=Path, help="the directory of `stage` snapshots")

    args = ap.parse_args()
    if args.command == "stage":
        return cmd_stage(args)
    if not (args.tag and args.assets and args.out):
        ap.error("--tag, --assets and --out are required to write the record")
    return cmd_record(args)


if __name__ == "__main__":
    sys.exit(main())
