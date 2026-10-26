"""Smoke-script contracts using fake tools: no Docker, network, or bound ports.

The real system timeout/ps and an owned fake server exercise process cleanup;
ss/curl are deterministic mocks, not an HTTP integration or ABI test.
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "deploy" / "centos7" / "smoke.sh"


class Centos7SmokeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="nyro-smoke-contract-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.config = self.directory / "config.json"
        self.config.write_text(json.dumps({"mode": "success"}))
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}")
        self.server = self.make_tool("fake-nyro", """
            import signal
            import time
            if sys.argv[1:] == ['--version']:
                (root / 'version-called').touch()
                print('nyro-server smoke-contract')
                sys.exit(0)
            (root / 'server-pid').write_text(str(os.getpid()))
            (root / 'server-env.json').write_text(json.dumps(dict(os.environ)))
            (root / 'server-argv.json').write_text(json.dumps(sys.argv[1:]))
            if config['mode'] == 'early-exit':
                sys.exit(17)
            def terminate(signum, frame):
                (root / 'terminated').write_text(str(signum))
                sys.exit(0)
            signal.signal(signal.SIGTERM, terminate)
            (root / 'started').touch()
            while True:
                signal.pause()
        """)
        self.make_tool("ss", """
            with (root / 'ss-calls.jsonl').open('a') as output:
                output.write(json.dumps(sys.argv[1:]) + '\\n')
            if config['mode'] == 'ss-failure':
                print('mock ss inspection failed', file=sys.stderr)
                sys.exit(23)
            print('State Recv-Q Send-Q Local Address:Port Peer Address:Port Process')
            if sys.argv[1:] == ['-ltn']:
                if config['mode'] == 'occupied':
                    print('LISTEN 0 128 127.0.0.1:29530 0.0.0.0:*')
            else:
                pid = (root / 'server-pid').read_text()
                for port in (29530, 29531):
                    owner = '999999999' if config['mode'] == 'wrong-owner' and port == 29530 else pid
                    print(f'LISTEN 0 128 127.0.0.1:{port} 0.0.0.0:* users:(("fake-nyro",pid={owner},fd=7))')
        """)
        self.make_tool("mktemp", """
            import tempfile
            path = tempfile.mkdtemp(prefix='scratch-', dir=root)
            (root / 'scratch-path').write_text(path)
            print(path)
        """)
        self.make_tool("curl", """
            import time
            args = sys.argv[1:]
            with (root / 'curl-calls.jsonl').open('a') as output:
                output.write(json.dumps(args) + '\\n')
            # Fail deterministically if the script drops proxy/rc isolation.
            if args[:3] != ['-q', '--noproxy', '*']:
                print('curl configuration/proxy isolation missing', file=sys.stderr)
                sys.exit(91)
            if config['mode'] == 'early-exit':
                sys.exit(7)
            deadline = time.monotonic() + 2
            while not (root / 'started').exists():
                if time.monotonic() > deadline:
                    sys.exit(7)
                time.sleep(0.01)
            url = next(arg for arg in args if arg.startswith('http://'))
            if '-o' in args:
                Path(args[args.index('-o') + 1]).write_text('<!doctype html><title>Nyro</title>')
            elif config['mode'] == 'bad-body' and url.endswith('/health'):
                print('{"status":"wrong-instance"}')
            else:
                print('{"status":"ok"}')
        """)

    def make_tool(self, name, body):
        path = self.bin / name
        prelude = (
            f"#!{sys.executable}\n"
            "import json, os, sys\nfrom pathlib import Path\n"
            f"root = Path({str(self.directory)!r})\n"
            "config = json.loads((root / 'config.json').read_text())\n"
        )
        path.write_text(prelude + textwrap.dedent(body))
        path.chmod(0o755)
        return path

    def run_smoke(self, mode="success"):
        self.config.write_text(json.dumps({"mode": mode}))
        # Own a separate session. GNU timeout may create its own process group;
        # emergency cleanup checks session ownership before signaling it.
        process = subprocess.Popen(
            ["bash", str(SCRIPT), str(self.server)], env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=10)
            return subprocess.CompletedProcess(process.args, process.returncode, stdout, stderr)
        finally:
            # Never confuse a recycled PID with a process owned by this test.
            # timeout and its child can be outside the shell's process group.
            pid_file = self.directory / "server-pid"
            if pid_file.exists():
                server_pid = int(pid_file.read_text())
                try:
                    if os.getsid(server_pid) == process.pid:
                        os.killpg(os.getpgid(server_pid), signal.SIGKILL)
                except ProcessLookupError:
                    pass
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)

    def assert_not_started(self):
        self.assertFalse((self.directory / "version-called").exists())
        self.assertFalse((self.directory / "server-pid").exists())
        self.assertFalse((self.directory / "scratch-path").exists())
        self.assertFalse((self.directory / "curl-calls.jsonl").exists())

    def assert_clean_shutdown(self, result):
        # The signal marker must be written before the test's emergency SIGKILL;
        # a mere dead PID would not prove the script cleaned up its child.
        self.assertTrue((self.directory / "terminated").exists(), result.stderr)
        self.assertEqual((self.directory / "terminated").read_text(), str(signal.SIGTERM))
        pid = int((self.directory / "server-pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        scratch = Path((self.directory / "scratch-path").read_text())
        self.assertTrue(scratch.is_dir())
        self.assertTrue((scratch / "server.log").is_file())
        self.assertIn(str(scratch), result.stdout)

    def test_occupied_port_fails_before_start_or_scratch(self):
        result = self.run_smoke("occupied")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already in use", result.stderr)
        self.assert_not_started()

    def test_ss_failure_fails_closed(self):
        result = self.run_smoke("ss-failure")
        self.assertEqual(result.returncode, 23)
        self.assertIn("mock ss inspection failed", result.stderr)
        self.assert_not_started()

    def test_server_early_exit_reports_failure_and_retains_log(self):
        result = self.run_smoke("early-exit")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Server exited before readiness", result.stderr)
        self.assertNotIn("PASS:", result.stdout)
        scratch = Path((self.directory / "scratch-path").read_text())
        self.assertTrue((scratch / "server.log").is_file())
        self.assertIn(str(scratch), result.stdout)
        pid = int((self.directory / "server-pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_success_stops_owned_server_and_retains_scratch(self):
        result = self.run_smoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PASS: embedded WebUI", result.stdout)
        self.assert_clean_shutdown(result)
        scratch = Path((self.directory / "scratch-path").read_text())
        self.assertIn("<!doctype html", (scratch / "index.html").read_text())
        server_env = json.loads((self.directory / "server-env.json").read_text())
        self.assertEqual(server_env["HOME"], str(scratch))
        self.assertEqual(server_env["PATH"], "/usr/bin:/bin")
        args = json.loads((self.directory / "server-argv.json").read_text())
        self.assertEqual(args[args.index("--data-dir") + 1], str(scratch / "data"))
        for flag in ("--proxy-host", "--admin-host"):
            self.assertEqual(args[args.index(flag) + 1], "127.0.0.1")

    def test_wrong_listener_owner_fails_and_cleans_up(self):
        result = self.run_smoke("wrong-owner")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not owned by smoke server", result.stderr)
        self.assertNotIn("PASS:", result.stdout)
        self.assert_clean_shutdown(result)

    def test_bad_probe_body_fails_and_cleans_up(self):
        result = self.run_smoke("bad-body")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unexpected response", result.stderr)
        self.assertNotIn("PASS: embedded WebUI", result.stdout)
        self.assert_clean_shutdown(result)

    def test_proxy_and_nyro_environment_cannot_change_probe_contract(self):
        self.env.update(
            http_proxy="http://invalid-proxy.example:1",
            ALL_PROXY="http://invalid-proxy.example:1", NO_PROXY="",
            NYRO_STORAGE_BACKEND="postgres", NYRO_POSTGRES_DSN="invalid-dsn",
            NYRO_LAN="true", CURL_HOME=str(self.directory),
        )
        (self.directory / ".curlrc").write_text("proxy = http://invalid-proxy.example:1\n")
        result = self.run_smoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_clean_shutdown(result)
        calls = [json.loads(line) for line in
                 (self.directory / "curl-calls.jsonl").read_text().splitlines()]
        self.assertGreaterEqual(len(calls), 7)
        for args in calls:
            self.assertEqual(args[:3], ["-q", "--noproxy", "*"])
        server_env = json.loads((self.directory / "server-env.json").read_text())
        for key in ("http_proxy", "ALL_PROXY", "NYRO_STORAGE_BACKEND",
                    "NYRO_POSTGRES_DSN", "NYRO_LAN", "CURL_HOME"):
            self.assertNotIn(key, server_env)


if __name__ == "__main__":
    unittest.main()
