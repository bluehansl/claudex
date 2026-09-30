"""배포 경계·메타데이터·부분 성공 재시도 회귀 검증."""

import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

import claudex_npm_release as release


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def tarball(self, native=False, metadata_update=None, extra=None):
        version = "1.2.3-darwin-arm64" if native else "1.2.3"
        metadata = {
            "name": release.PACKAGE,
            "version": version,
            "repository": {"url": release.REPOSITORY},
        }
        files = {}
        if native:
            metadata.update(os=["darwin"], cpu=["arm64"])
            prefix = f"package/vendor/{release.TARGET}/"
            files[prefix + "codex-package.json"] = json.dumps(
                {
                    "layoutVersion": 1,
                    "version": "1.2.3",
                    "variant": "claudex",
                    "target": release.TARGET,
                    "entrypoint": "bin/claudex",
                }
            ).encode()
            for binary in ("bin/claudex", "bin/codex-code-mode-host", "codex-path/rg"):
                files[prefix + binary] = b"test fixture"
        else:
            metadata.update(
                bin={"claudex": "bin/claudex.js"},
                optionalDependencies={
                    "@bluehansl/claudex-darwin-arm64": "npm:@bluehansl/claudex@1.2.3-darwin-arm64",
                },
            )
            files["package/bin/claudex.js"] = b"// fixture"
        metadata.update(metadata_update or {})
        files["package/package.json"] = json.dumps(metadata).encode()
        files.update(extra or {})
        path = self.root / ("platform.tgz" if native else "root.tgz")
        with tarfile.open(path, "w:gz") as archive:
            for name, content in files.items():
                info = tarfile.TarInfo(name)
                info.size = len(content)
                info.mode = 0o755
                archive.addfile(info, io.BytesIO(content))
        return path

    def test_matching_root_and_platform(self):
        release.inspect_tarball(self.tarball(), "1.2.3", native=False)
        release.inspect_tarball(self.tarball(native=True), "1.2.3", native=True)

    def test_daemon_smoke_uses_long_isolated_home_and_manual_updater(self):
        replies = [
            {},
            {
                "status": "noUpdate",
                "installedVersion": "1.2.3",
                "runningVersion": "1.2.3",
            },
            {
                "cliVersion": "1.2.3",
                "managedCodexVersion": "1.2.3",
                "appServerVersion": "1.2.3",
            },
            {},
        ]
        with patch.object(
            release.subprocess,
            "run",
            side_effect=[
                subprocess.CompletedProcess([], 0, json.dumps(reply), "")
                for reply in replies
            ],
        ) as run:
            release.smoke_daemon_long_home(
                self.root / "bin/claudex", "1.2.3", self.root, {"PATH": "/usr/bin"}
            )
        self.assertEqual(
            [c.args[0][-1] for c in run.call_args_list],
            ["start", "update", "version", "stop"],
        )
        for call_ in run.call_args_list:
            self.assertNotIn("--from-cli", call_.args[0])
            home = Path(call_.kwargs["env"]["CODEX_HOME"])
            self.assertTrue(home.is_relative_to(self.root))
            self.assertGreaterEqual(
                len(
                    os.fsencode(home / "claudex-app-server-daemon/daemon-updater.sock")
                ),
                104,
            )

    def test_daemon_smoke_stops_after_update_failure(self):
        results = [
            subprocess.CompletedProcess([], 0, "{}", ""),
            subprocess.CompletedProcess([], 1, "", "updater failed"),
            subprocess.CompletedProcess([], 0, "{}", ""),
        ]
        with patch.object(release.subprocess, "run", side_effect=results) as run:
            with self.assertRaisesRegex(RuntimeError, "updater failed"):
                release.smoke_daemon_long_home(
                    self.root / "bin/claudex", "1.2.3", self.root, {}
                )
        self.assertEqual(run.call_args_list[-1].args[0][-1], "stop")

    def test_daemon_smoke_rejects_old_running_version(self):
        results = [
            subprocess.CompletedProcess([], 0, "{}", ""),
            subprocess.CompletedProcess(
                [],
                0,
                json.dumps(
                    {
                        "status": "updated",
                        "installedVersion": "1.2.3",
                        "runningVersion": "1.2.2",
                    }
                ),
                "",
            ),
            subprocess.CompletedProcess([], 0, "{}", ""),
        ]
        with patch.object(release.subprocess, "run", side_effect=results):
            with self.assertRaisesRegex(ValueError, "버전 불일치"):
                release.smoke_daemon_long_home(
                    self.root / "bin/claudex", "1.2.3", self.root, {}
                )

    def test_mock_harness_loads_with_standard_library_only(self):
        directory = str(release.REPO_ROOT / "sdk/python/tests")
        subprocess.run(
            [
                sys.executable,
                "-S",
                "-c",
                f"import sys; sys.path.insert(0, {directory!r}); "
                "from app_server_harness import MockResponsesServer, ev_completed, ev_response_created, sse; "
                "assert all(callable(value) for value in (MockResponsesServer, ev_completed, ev_response_created, sse))",
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=5,
        )

    def test_rejects_wrong_package_version_repository_and_dependency(self):
        for change in [
            {"name": "@openai/codex"},
            {"version": "1.2.4"},
            {"repository": {"url": "https://github.com/openai/codex"}},
            {"optionalDependencies": {}},
        ]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                release.inspect_tarball(
                    self.tarball(metadata_update=change), "1.2.3", native=False
                )

    def test_rejects_paths_and_private_instructions(self):
        for name in (
            "package/../escape",
            "/absolute",
            "package/.codex/skills/a",
            "package/AGENTS.md",
        ):
            with self.subTest(name=name), self.assertRaises(ValueError):
                release.inspect_tarball(
                    self.tarball(extra={name: b"private"}), "1.2.3", native=False
                )

    def test_rejects_mixed_platform(self):
        with self.assertRaises(ValueError):
            release.inspect_tarball(
                self.tarball(native=True, metadata_update={"cpu": ["x64"]}),
                "1.2.3",
                native=True,
            )

    def test_publish_input_gate(self):
        (self.root / "codex-rs").mkdir()
        (self.root / "codex-rs/Cargo.toml").write_text(
            '[workspace.package]\nversion="1.2.3"\n'
        )
        with patch.object(release, "REPO_ROOT", self.root):
            release.validate_inputs(
                "1.2.3", "darwin-arm64", "refs/heads/claudex-1.2.3", "bluehansl/claudex"
            )
            for args in [
                ("1.2.3", "all", "refs/heads/claudex-1.2.3", "bluehansl/claudex"),
                ("1.2.3", "core", "refs/heads/main", "bluehansl/claudex"),
                ("1.2.3", "core", "refs/heads/claudex-1.2.3", "other/fork"),
                ("1.2.4", "core", "refs/heads/claudex-1.2.4", "bluehansl/claudex"),
            ]:
                with self.subTest(args=args), self.assertRaises(ValueError):
                    release.validate_inputs(*args)

    def test_rejects_unstable_and_path_versions(self):
        for version in ("1.2.3-beta", "../main", "01.2.3", "1.2", "1.2.3\n"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.stable_version(version)

    def test_retry_skips_only_identical_published_bytes_and_tag(self):
        path = self.tarball()
        with (
            patch.object(
                release, "npm_view", side_effect=[release.integrity(path), "1.2.3"]
            ),
            patch.object(release.subprocess, "run") as run,
        ):
            release.publish_one(path, "1.2.3", "latest")
            run.assert_not_called()
        for response in (["sha512-other"], [release.integrity(path), "1.2.4"]):
            with (
                patch.object(release, "npm_view", side_effect=response),
                self.assertRaises(ValueError),
            ):
                release.publish_one(path, "1.2.3", "latest")

    def test_registry_errors_are_not_missing_versions(self):
        for code in ("E401", "E500", "E404"):
            result = subprocess.CompletedProcess(
                [], 1, json.dumps({"error": {"code": code}}), ""
            )
            with patch.object(release.subprocess, "run", return_value=result):
                if code == "E404":
                    self.assertIsNone(release.npm_view("test", "dist.integrity"))
                else:
                    with self.assertRaises(RuntimeError):
                        release.npm_view("test", "dist.integrity")

    def test_platform_is_published_before_root_and_failure_stops_root(self):
        root, native = self.tarball(), self.tarball(native=True)
        args = [
            "release",
            "--version",
            "1.2.3",
            "--root",
            str(root),
            "--platform",
            str(native),
            "--publish",
        ]
        with (
            patch.object(sys, "argv", args),
            patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}),
            patch.object(release, "validate_inputs"),
            patch.object(
                release,
                "npm_view",
                side_effect=["1.0.0", "1.0.0-darwin-arm64", "1.2.3"],
            ),
            patch.object(release, "publish_one") as publish,
        ):
            self.assertEqual(release.main(), 0)
            self.assertEqual(
                publish.call_args_list,
                [
                    call(native, "1.2.3-darwin-arm64", "darwin-arm64"),
                    call(root, "1.2.3", "latest"),
                ],
            )
        with (
            patch.object(sys, "argv", args),
            patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}),
            patch.object(release, "validate_inputs"),
            patch.object(release, "npm_view", return_value="1.0.0"),
            patch.object(
                release, "publish_one", side_effect=RuntimeError("publish failed")
            ) as publish,
        ):
            with self.assertRaises(RuntimeError):
                release.main()
            self.assertEqual(publish.call_count, 1)

    def test_newer_latest_cannot_be_downgraded(self):
        root, native = self.tarball(), self.tarball(native=True)
        args = [
            "release",
            "--version",
            "1.2.3",
            "--root",
            str(root),
            "--platform",
            str(native),
            "--publish",
        ]
        with (
            patch.object(sys, "argv", args),
            patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}),
            patch.object(release, "validate_inputs"),
            patch.object(release, "npm_view", return_value="2.0.0"),
            patch.object(release, "publish_one") as publish,
        ):
            with self.assertRaises(ValueError):
                release.main()
            publish.assert_not_called()

    def test_registry_processing_wait_is_bounded_and_does_not_republish(self):
        path = self.tarball()
        with (
            patch.object(
                release, "npm_view", side_effect=[None, release.integrity(path)]
            ),
            patch.object(release.time, "sleep") as sleep,
        ):
            release.wait_for_integrity(path, "1.2.3")
            sleep.assert_called_once_with(900)
        with (
            patch.object(release, "npm_view", return_value=None),
            patch.object(release.time, "sleep") as sleep,
        ):
            with self.assertRaises(RuntimeError):
                release.wait_for_integrity(path, "1.2.3")
            sleep.assert_called_once_with(900)

    def test_wrong_integrity_is_not_treated_as_registry_delay(self):
        with (
            patch.object(release, "npm_view", return_value="sha512-other"),
            patch.object(release.time, "sleep") as sleep,
        ):
            with self.assertRaises(ValueError):
                release.wait_for_integrity(self.tarball(), "1.2.3")
            sleep.assert_not_called()

    def test_prebuild_gate_rejects_root_or_platform_already_published(self):
        for response in (["1.2.3"], [None, "1.2.3-darwin-arm64"]):
            with patch.object(release, "npm_view", side_effect=response):
                with self.assertRaises(ValueError):
                    release.require_unpublished("1.2.3")
        with patch.object(release, "npm_view", side_effect=[None, None]):
            release.require_unpublished("1.2.3")


if __name__ == "__main__":
    unittest.main()
