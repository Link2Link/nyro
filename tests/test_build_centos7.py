"""Fast CLI contract tests: no Docker daemon, downloads or compilation required."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "build-centos7.sh"


class Centos7BuildCliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="nyro-centos7-test-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.log = self.directory / "docker.calls"
        docker = self.directory / "docker"
        docker.write_text(
            f'#!{sys.executable}\n'
            'import json, os, sys\n'
            'with open(os.environ["DOCKER_TEST_LOG"], "a") as log:\n'
            '    log.write(json.dumps(sys.argv[1:]) + "\\n")\n'
            'if sys.argv[1:3] == ["buildx", "build"]:\n'
            '    stage = sys.argv[sys.argv.index("--target") + 1]\n'
            '    if os.environ.get("FAIL_STAGE") == stage: sys.exit(42)\n'
        )
        docker.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.directory}:{os.environ['PATH']}",
                        DOCKER_TEST_LOG=str(self.log), BUILD_JOBS="4", FAIL_STAGE="")

    def run_cli(self, *args):
        return subprocess.run(["bash", str(SCRIPT), *args], cwd=self.directory,
                              env=self.env, text=True, capture_output=True)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def builds(self):
        return [call for call in self.calls() if call[:2] == ["buildx", "build"]]

    def test_help_does_not_call_docker(self):
        result = self.run_cli("--help")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--image-only", result.stdout)
        self.assertEqual(self.calls(), [])

    def test_invalid_arguments_fail_before_docker(self):
        for args in [("--jobs", "0"), ("--jobs", "oops"), ("--jobs",),
                     ("--output",), ("--image",), ("--unknown",),
                     ("--output", ""), ("--output", "a,b"), ("--image", "")]:
            with self.subTest(args=args):
                self.assertEqual(self.run_cli(*args).returncode, 2)
                self.assertEqual(self.calls(), [])

    def test_image_only_does_not_export_artifacts(self):
        result = self.run_cli("--image-only", "--image", "nyro-test:builder")
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = self.builds()
        self.assertEqual(len(builds), 1)
        self.assertEqual(builds[0][-6:], ["--target", "builder", "--tag", "nyro-test:builder", "--load", str(ROOT)])
        self.assertEqual(builds[0][2:4], ["--platform", "linux/amd64"])

    def test_export_uses_resolved_output_and_job_count(self):
        result = self.run_cli("--output", "output with spaces", "--jobs", "2")
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = self.builds()
        self.assertEqual(len(builds), 2)
        self.assertEqual(builds[1][-7:], ["--target", "artifacts", "--build-arg", "BUILD_JOBS=2",
                                        "--output", f"type=local,dest={self.directory}/output with spaces", str(ROOT)])

    def test_invalid_registry_fails_before_docker(self):
        incomplete = self.directory / "incomplete-registry"
        (incomplete / "index").mkdir(parents=True)
        for args in [("--cargo-registry",),
                     ("--cargo-registry", "missing-registry"),
                     ("--cargo-registry", str(incomplete))]:
            with self.subTest(args=args):
                result = self.run_cli(*args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(self.calls(), [])

    def test_registry_context_resolves_relative_path_with_spaces(self):
        registry = self.directory / "registry with spaces"
        (registry / "index").mkdir(parents=True)
        (registry / "cache").mkdir()
        result = self.run_cli("--cargo-registry", registry.name, "--output", "out")
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = self.builds()
        self.assertEqual(len(builds), 2)
        self.assertNotIn("--build-context", builds[0])
        position = builds[1].index("--build-context")
        self.assertEqual(builds[1][position + 1], f"cargo-seed={registry.resolve()}")
        self.assertEqual(builds[1].count("--build-context"), 1)

    def test_offline_boolean_is_only_passed_to_artifact_build(self):
        for options, expected in [((), "false"), (("--offline",), "true")]:
            with self.subTest(options=options):
                before = len(self.builds())
                result = self.run_cli(*options, "--output", "out")
                self.assertEqual(result.returncode, 0, result.stderr)
                builds = self.builds()[before:]
                self.assertEqual(len(builds), 2)
                self.assertFalse(any(arg.startswith("CARGO_OFFLINE=") for arg in builds[0]))
                args = builds[1]
                self.assertIn("CARGO_OFFLINE=" + expected, args)
                self.assertEqual(args[args.index("CARGO_OFFLINE=" + expected) - 1], "--build-arg")
                self.assertNotIn("--build-context", args)

    def test_builder_failure_prevents_export(self):
        self.env["FAIL_STAGE"] = "builder"
        result = self.run_cli("--output", "output")
        self.assertEqual(result.returncode, 42)
        self.assertFalse((self.directory / "output").exists())
        self.assertEqual(len(self.builds()), 1)

    def test_artifact_failure_is_not_reported_as_success(self):
        self.env["FAIL_STAGE"] = "artifacts"
        result = self.run_cli("--output", "output")
        self.assertEqual(result.returncode, 42)
        self.assertNotIn("CentOS 7.6 artifacts:", result.stdout)
        self.assertEqual(len(self.builds()), 2)


if __name__ == "__main__":
    unittest.main()
