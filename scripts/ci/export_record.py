#!/usr/bin/env python3
"""Write the execution record of a models-release publish run (#955).

`export_record.json` is uploaded beside the model assets by
publish-models.yml and linked from `models.json` (`execution`) by
scripts/ci/models_manifest.sh. It records *how* the run produced the bytes
it staged — what a source revision and a script name alone do not pin:

- the workflow run (URL, id, attempt, commit, actor) and the runner (image,
  OS, CPU — the int8 accuracy gate infers on that CPU);
- the Python and the version of every package the exports, quantizations
  and repacks ran with, as installed when the record was written (the
  per-step pins are in publish-models.yml at the recorded commit);
- the sha256 of every upstream input file an `export` artifact consumed,
  read from the Hugging Face hub cache the export scripts populate at the
  revisions scripts/install/models_catalog.json pins;
- the sha256 and size of every staged asset.

A workflow run's logs and artifacts expire; a release asset does not, so
the record outlives the run it describes. Nothing in it is a secret: the
environment variables read are GitHub's public run coordinates.

    python scripts/ci/export_record.py --tag models-v1 --assets release-assets \\
        --out release-assets/export_record.json
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


def upstream_inputs(catalog: dict, cache: Path) -> list[dict]:
    """The upstream files every `export` artifact consumed, hashed from the
    hub cache's snapshot of the catalog's revision — `sha256: null` with a
    note when the file is not there (an export step that did not run, or
    one that does not read that file)."""
    seen: set[tuple[str, str, str]] = set()
    out: list[dict] = []
    for art in catalog["artifacts"]:
        if art.get("provenance") != "export":
            continue
        up = art["upstream"]
        repo, rev = up["repository"], up["revision"]
        prefix = "https://huggingface.co/"
        for rel in up.get("files", []):
            key = (repo, rev, rel)
            if key in seen:
                continue
            seen.add(key)
            entry = {"artifact": art["id"], "repository": repo, "revision": rev, "file": rel}
            if repo.startswith(prefix):
                repo_id = repo[len(prefix) :].strip("/")
                path = cache / f"models--{repo_id.replace('/', '--')}" / "snapshots" / rev / rel
                if path.is_file():
                    entry["sha256"] = sha256_of(path.resolve())
                    entry["size"] = path.resolve().stat().st_size
                else:
                    entry["sha256"] = None
                    entry["note"] = "not in the hub cache after the exports: no step of this job read it"
            else:
                entry["sha256"] = None
                entry["note"] = "not a Hugging Face file: hashed by its own fetch step, see the workflow log"
            out.append(entry)
    return out


def staged_outputs(assets: Path, skip: str) -> list[dict]:
    out = []
    for p in sorted(assets.iterdir()):
        if not p.is_file() or p.name == skip:
            continue
        out.append({"asset": p.name, "sha256": sha256_of(p), "size": p.stat().st_size})
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tag", required=True, help="the models release tag this run publishes into")
    ap.add_argument("--assets", required=True, type=Path, help="the staged release assets directory")
    ap.add_argument("--out", required=True, type=Path, help="where to write export_record.json")
    ap.add_argument("--catalog", type=Path, default=CATALOG)
    args = ap.parse_args()

    catalog = json.loads(args.catalog.read_text(encoding="utf-8"))
    record = {
        "$comment": (
            "Execution record of the publish-models.yml run that produced the export, quantization "
            "and repack assets of this models release and fetched its re-hosted ones (#955): the run, "
            "the runner, the toolchain versions, the sha256 of every upstream input consumed and of "
            "every asset staged. models.json links it as `execution`; the upstream lineage of each "
            "model is the catalog's `lineage`."
        ),
        "schema": 1,
        "release": args.tag,
        "generated": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "workflow": workflow_record(),
        "runner": runner_record(),
        "python": platform.python_version(),
        "packages": package_versions(),
        "inputs": upstream_inputs(catalog, hub_cache()),
        "outputs": staged_outputs(args.assets, args.out.name),
    }
    args.out.write_text(json.dumps(record, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(
        f"{args.out}: run {record['workflow']['url'] or '(local)'}, "
        f"{len(record['inputs'])} inputs, {len(record['outputs'])} outputs, "
        f"{len(record['packages'])} package versions",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
