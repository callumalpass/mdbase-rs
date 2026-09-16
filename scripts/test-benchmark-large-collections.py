#!/usr/bin/env python3
"""Harness safety tests, independent of Rust build or LAB services."""
import importlib.util
from pathlib import Path
import tempfile
import subprocess
import sys
from types import SimpleNamespace
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "benchmark", Path(__file__).with_name("benchmark-large-collections.py")
)
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.args = SimpleNamespace(
            noise_files=10000, body_bytes=1024, noise_bytes=4096,
            deadline_ms=30000, page_size=1000, memory_mib=1024,
            phase_timeout=0.3, min_free_mib=1,
        )

    def fake(self, source):
        path = self.root / "fake-benchmark"
        path.write_text("#!/usr/bin/env python3\n" + source)
        path.chmod(0o700)
        return path

    def run_fake(self, source):
        config = benchmark.fixture_config(self.args, 20, "markdown", self.root, self.root / "manifest")
        result = benchmark.run_case(self.fake(source), self.root, "test", config, self.args)
        self.assertFalse(Path(result["config"]["root"]).exists())
        self.assertFalse(list(self.root.glob("fixture-*")))
        self.assertTrue((self.root / "test.jsonl").exists())
        self.assertTrue((self.root / "test.stderr.log").exists())
        return result

    def test_matrix_distinguishes_notes_from_tasks_and_noise(self):
        mixed = benchmark.fixture_config(self.args, 10000, "mixed", self.root, self.root)
        self.assertEqual(mixed["task_percent"], 20)
        self.assertEqual(mixed["noise_files"], 10000)
        md = benchmark.fixture_config(self.args, 10000, "markdown", self.root, self.root)
        self.assertEqual(md["noise_files"], 0)
        self.assertEqual(md["task_percent"], 100)
        excluded = benchmark.fixture_config(self.args, 10000, "excluded-json", self.root, self.root)
        self.assertEqual(excluded["noise_kind"], "excluded-json")

    def test_success_requires_complete_event(self):
        result = self.run_fake('print(\'{"event":"complete"}\', flush=True)\n')
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["fixture_cleanup"], "complete")

    def test_empty_success_is_not_a_benchmark_pass(self):
        result = self.run_fake("pass\n")
        self.assertEqual(result["status"], "failed")

    def test_failure_preserves_error_and_cleans_fixture(self):
        result = self.run_fake(
            'import sys\nprint(\'{"event":"error","message":"synthetic failure"}\')\nsys.exit(2)\n'
        )
        self.assertEqual(result["exit_code"], 2)
        self.assertEqual(result["status"], "failed")
        self.assertIn("synthetic failure", benchmark.report([result]))

    def test_watchdog_kills_noncooperative_work_and_cleans_staging(self):
        result = self.run_fake(
            'import os, pathlib, time\n'
            'pathlib.Path(os.environ["TMPDIR"], "staging-file").write_text("fixture")\n'
            'print(\'{"event":"start","phase":"blocked"}\', flush=True)\n'
            'time.sleep(30)\n'
        )
        self.assertEqual(result["stop_reason"], "watchdog_timeout:blocked")
        self.assertEqual(result["status"], "failed")
        self.assertLess(result["wall_seconds"], 3)

    def test_disk_guard_stops_child_without_removing_evidence(self):
        self.args.min_free_mib = 2**50
        result = self.run_fake("import time\ntime.sleep(30)\n")
        self.assertTrue(result["stop_reason"].startswith("disk_guard:"))
        self.assertEqual(result["status"], "failed")

    def test_address_space_limit_is_applied_to_child(self):
        result = self.run_fake("bytearray(2 * 1024**3)\n")
        self.assertNotEqual(result["exit_code"], 0)
        self.assertEqual(result["status"], "failed")
        self.assertIn("MemoryError", (self.root / "test.stderr.log").read_text())

    def test_cli_refuses_existing_output_without_modifying_it(self):
        output = self.root / "existing"
        output.mkdir()
        sentinel = output / "keep.txt"
        sentinel.write_text("preserve")
        manifest = self.root / "manifest.json"
        manifest.write_text("{}")
        result = subprocess.run([
            sys.executable, str(Path(benchmark.__file__)),
            "--manifest", str(manifest), "--binary", str(self.fake("pass\n")),
            "--output", str(output), "--notes", "1", "--noise-files", "1",
            "--min-free-mib", "1",
        ], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sentinel.read_text(), "preserve")
        self.assertEqual(list(output.iterdir()), [sentinel])

    def test_interrupted_last_event_retains_previous_evidence(self):
        path = self.root / "events"
        path.write_text('{"event":"start","phase":"work"}\n{"event":')
        self.assertEqual(benchmark.read_events(path), [{"event": "start", "phase": "work"}])


if __name__ == "__main__":
    unittest.main()
