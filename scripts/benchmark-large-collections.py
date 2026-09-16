#!/usr/bin/env python3
"""Bounded local-only benchmark orchestrator; see docs/large-collection-benchmark.md."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import shutil
import signal
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parent.parent
CASES = ("markdown", "binary", "json", "excluded-json", "mixed")


def positive(value):
    value = int(value)
    if value <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return value


def digest(path):
    hasher = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def command(*args):
    return subprocess.check_output(args, cwd=REPO, text=True).strip()


def fixture_config(args, notes, case, root, manifest):
    return {
        "root": str(root), "manifest": str(manifest), "notes": notes,
        "task_percent": 20 if case == "mixed" else 100,
        "noise_files": 0 if case == "markdown" else args.noise_files,
        "noise_kind": case if case in ("json", "excluded-json") else "binary",
        "body_bytes": args.body_bytes, "noise_bytes": args.noise_bytes,
        "deadline_ms": args.deadline_ms, "page_size": args.page_size,
    }


def child_limits(memory_mib):
    # Only the benchmark child is constrained; no changes to desktop/daemon limits.
    resource.setrlimit(resource.RLIMIT_AS, (memory_mib * 1024**2,) * 2)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    os.nice(10)


def read_events(path):
    result = []
    for line in path.read_text().splitlines():
        try:
            result.append(json.loads(line))
        except json.JSONDecodeError:
            # The final event may have been interrupted by the watchdog.
            continue
    return result


def run_case(binary, output, name, config, args):
    events_path = output / f"{name}.jsonl"
    stderr_path = output / f"{name}.stderr.log"
    started = time.monotonic()
    stop_reason = None
    active = "process_start"
    phase_start = started
    processed = 0
    # Includes engine temp staging: timeout/abort cannot leak multi-GB /tmp shadows.
    with tempfile.TemporaryDirectory(prefix=f"fixture-{name}-", dir=output) as sandbox:
        sandbox = Path(sandbox)
        temp = sandbox / "tmp"
        temp.mkdir()
        config["root"] = str(sandbox / "collection")
        config_path = output / f"{name}.config.json"
        config_path.write_text(json.dumps(config, indent=2) + "\n")
        env = {**os.environ, "TMPDIR": str(temp)}
        with events_path.open("w") as stdout, stderr_path.open("w") as stderr:
            process = subprocess.Popen(
                [str(binary), str(config_path)], stdout=stdout, stderr=stderr,
                env=env, start_new_session=True,
                preexec_fn=lambda: child_limits(args.memory_mib),
            )
            try:
                while process.poll() is None:
                    events = read_events(events_path)
                    for event in events[processed:]:
                        if event.get("event") == "start":
                            active = event["phase"]
                            phase_start = time.monotonic()
                    processed = len(events)
                    if time.monotonic() - phase_start > args.phase_timeout:
                        stop_reason = f"watchdog_timeout:{active}"
                        break
                    if shutil.disk_usage(output).free < args.min_free_mib * 1024**2:
                        stop_reason = f"disk_guard:{active}"
                        break
                    time.sleep(0.1)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        events = read_events(events_path)
        completed = any(event.get("event") == "complete" for event in events)
        result = {
            "case": name, "config": config, "exit_code": process.returncode,
            "status": "passed" if process.returncode == 0 and completed and stop_reason is None else "failed",
            "stop_reason": stop_reason, "wall_seconds": time.monotonic() - started,
            "events": events, "fixture_cleanup": "complete",
        }
    return result


def report(results):
    lines = ["# Large collection benchmark", "",
             "Local engine/runtime only; no HTTP, encryption, browser rendering or TaskNotes recurrence maintenance.",
             "Fresh synthetic fixtures; OS page cache is **not** dropped. No timing thresholds are asserted.",
             "RSS high water is process-lifetime, not a per-phase peak. Each case uses a fresh process.", "",
             "| Case | Outcome | Wall seconds | Process peak MiB |", "|---|---|---:|---:|"]
    for result in results:
        peak = max((e.get("memory", {}).get("process_hwm_kib") or 0 for e in result["events"]), default=0)
        outcome = result["stop_reason"] or result["status"]
        lines.append(f"| {result['case']} | {outcome} | {result['wall_seconds']:.2f} | {peak / 1024:.1f} |")
    for result in results:
        lines += ["", f"## {result['case']}", "", "| Phase | ms | Outcome |", "|---|---:|---|"]
        for event in result["events"]:
            if event.get("event") == "finish":
                status = "ok" if event["ok"] else "failed"
                lines.append(f"| {event['phase']} | {event['elapsed_ms']:.2f} | {status} |")
            elif event.get("event") == "query_summary":
                lines.append(f"| {event['phase']} TOTAL ({event['rows']} rows, {event['pages']} pages, {event['response_bytes']} JSON bytes) | {event['elapsed_ms']:.2f} | ok |")
            elif event.get("event") == "error":
                lines += ["", "```text", event["message"], "```"]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True, help="TaskNotes generated mdbase-app.json (public, no credentials)")
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/large-collection-benchmark")
    parser.add_argument("--output", type=Path, required=True, help="NEW output directory; existing paths are refused")
    parser.add_argument("--notes", type=positive, nargs="+", default=[1000, 10000])
    parser.add_argument("--cases", choices=CASES, nargs="+", default=list(CASES))
    parser.add_argument("--repeats", type=positive, default=1)
    parser.add_argument("--noise-files", type=positive, default=10000)
    parser.add_argument("--body-bytes", type=positive, default=1024)
    parser.add_argument("--noise-bytes", type=positive, default=4096)
    parser.add_argument("--page-size", type=positive, default=1000)
    parser.add_argument("--deadline-ms", type=positive, default=300000, help="runtime operation budget; use 20000/30000 for deadline experiments")
    parser.add_argument("--phase-timeout", type=positive, default=360, help="external hard watchdog, including non-cooperative work")
    parser.add_argument("--memory-mib", type=positive, default=4096, help="child address-space limit (not RSS)")
    parser.add_argument("--min-free-mib", type=positive, default=1024)
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.manifest = args.manifest.resolve(strict=True)
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    # Conservative allowance for overlapping staging maps/files and cache. Not a quota.
    fixture_bytes = max(args.notes) * (args.body_bytes + 256) + args.noise_files * (args.noise_bytes + 4096)
    required = args.min_free_mib * 1024**2 + fixture_bytes * 12
    if shutil.disk_usage(output.parent).free < required:
        parser.error(f"insufficient free space: require approximately {required / 1024**3:.2f} GiB")
    output.mkdir()  # refuses reuse, including a preexisting real collection
    manifest = output / "manifest.json"
    shutil.copyfile(args.manifest, manifest)
    # Pin the executable too: rebuilding between cases must not mix engine versions.
    binary = output / "benchmark-binary"
    shutil.copy2(args.binary, binary)
    (output / "benchmark-source.rs").write_bytes((REPO / "src/bin/large-collection-benchmark.rs").read_bytes())
    (output / "runner-source.py").write_bytes(Path(__file__).read_bytes())
    meminfo = {}
    if Path("/proc/meminfo").exists():
        meminfo = {line.split(":", 1)[0]: line.split(":", 1)[1].strip()
                   for line in Path("/proc/meminfo").read_text().splitlines()
                   if line.split(":", 1)[0] in ("MemTotal", "MemAvailable", "SwapTotal", "SwapFree")}
    metadata = {
        "schema_version": 1, "engine_commit": command("git", "rev-parse", "HEAD"),
        "source_status": command("git", "status", "--short"),
        "benchmark_source_sha256": digest(REPO / "src/bin/large-collection-benchmark.rs"),
        "runner_sha256": digest(Path(__file__)), "binary_sha256": digest(binary),
        "manifest_sha256": digest(manifest), "rustc": command("rustc", "--version"),
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "memory_at_start": meminfo, "free_disk_bytes_at_start": shutil.disk_usage(output).free,
        "load_average_at_start": os.getloadavg(), "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        "cache_policy": "fresh fixture and runtime; OS cache uncontrolled, generated files likely warm",
        "upgrade_policy": "synthetic managed contract body edit and task pack version 99.0.0; not historical migration",
    }
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    results = []
    for repeat in range(1, args.repeats + 1):
        for notes in args.notes:
            for case in args.cases:
                name = f"n{notes}-{case}-r{repeat}"
                print(f"Running {name}", flush=True)
                config = fixture_config(args, notes, case, output / "unused", manifest)
                result = run_case(binary, output, name, config, args)
                results.append(result)
                (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
                (output / "summary.md").write_text(report(results))
                print(f"  {result['status']}: {result['wall_seconds']:.2f}s", flush=True)
                if result["stop_reason"] and result["stop_reason"].startswith("disk_guard"):
                    raise SystemExit("Stopped matrix: disk guard")
    print(f"Report: {output / 'summary.md'}")
    raise SystemExit(0 if all(r["status"] == "passed" for r in results) else 1)


if __name__ == "__main__":
    main()
