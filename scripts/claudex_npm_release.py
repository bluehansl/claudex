#!/usr/bin/env python3
"""Claudex npm 산출물 검증과 GitHub OIDC 게시 절차."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
PACKAGE = "@bluehansl/claudex"
REPOSITORY = "git+https://github.com/bluehansl/claudex.git"
TARGET = "aarch64-apple-darwin"
TAG = "darwin-arm64"


def stable_version(value: str) -> tuple[int, int, int]:
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", value):
        raise ValueError("안정 버전 major.minor.patch가 필요합니다.")
    return tuple(map(int, value.split(".")))


def validate_inputs(version: str, package_filter: str, ref: str, repo: str) -> None:
    stable_version(version)
    if repo != "bluehansl/claudex" or ref != f"refs/heads/claudex-{version}":
        raise ValueError(
            "bluehansl/claudex의 해당 claudex-<version> 브랜치만 게시할 수 있습니다."
        )
    if package_filter not in {"core", "darwin", TAG, "claudex-darwin-arm64"}:
        raise ValueError(
            "자동 게시 대상은 darwin-arm64 하나입니다. 다른 플랫폼은 build-only를 사용하세요."
        )
    manifest = tomllib.loads((REPO_ROOT / "codex-rs/Cargo.toml").read_text())
    if manifest["workspace"]["package"]["version"] != version:
        raise ValueError("요청 버전과 Cargo workspace 버전이 다릅니다.")


def read_json_member(archive: tarfile.TarFile, name: str) -> dict:
    member = archive.getmember(name)
    if not member.isfile() or member.size > 1_048_576:
        raise ValueError(f"잘못된 JSON member: {name}")
    stream = archive.extractfile(member)
    if stream is None:
        raise ValueError(f"읽을 수 없는 member: {name}")
    return json.load(stream)


def inspect_tarball(path: Path, version: str, *, native: bool) -> dict:
    stable_version(version)
    with tarfile.open(path, "r:gz") as archive:
        names: set[str] = set()
        for member in archive.getmembers():
            name = Path(member.name)
            if (
                name.is_absolute()
                or ".." in name.parts
                or not name.parts
                or name.parts[0] != "package"
            ):
                raise ValueError(f"tar 경로 이탈: {member.name}")
            if not (member.isfile() or member.isdir()) or member.name in names:
                raise ValueError(f"허용되지 않은 tar member: {member.name}")
            names.add(member.name)
        metadata = read_json_member(archive, "package/package.json")
        expected = f"{version}-{TAG}" if native else version
        if metadata.get("name") != PACKAGE or metadata.get("version") != expected:
            raise ValueError("npm package name/version 불일치")
        if metadata.get("repository", {}).get("url") != REPOSITORY:
            raise ValueError(
                "npm repository와 GitHub trusted publisher 저장소가 다릅니다."
            )
        if native:
            if metadata.get("os") != ["darwin"] or metadata.get("cpu") != ["arm64"]:
                raise ValueError("잘못된 플랫폼 제약")
            prefix = f"package/vendor/{TARGET}/"
            manifest = read_json_member(archive, prefix + "codex-package.json")
            for key, value in {
                "layoutVersion": 1,
                "version": version,
                "variant": "claudex",
                "target": TARGET,
                "entrypoint": "bin/claudex",
            }.items():
                if manifest.get(key) != value:
                    raise ValueError(f"native manifest 불일치: {key}")
            for binary in ("bin/claudex", "bin/codex-code-mode-host", "codex-path/rg"):
                member = archive.getmember(prefix + binary)
                if not member.isfile() or not member.mode & 0o111:
                    raise ValueError(f"실행 파일 누락/권한 오류: {binary}")
        else:
            if metadata.get("bin") != {"claudex": "bin/claudex.js"}:
                raise ValueError("잘못된 root executable mapping")
            if metadata.get("optionalDependencies") != {
                "@bluehansl/claudex-darwin-arm64": f"npm:{PACKAGE}@{version}-{TAG}",
            }:
                raise ValueError("root/platform dependency 버전 또는 대상 불일치")
            if "package/bin/claudex.js" not in names:
                raise ValueError("root wrapper 누락")
            if any(name.startswith("package/vendor/") for name in names):
                raise ValueError("root package에 native vendor가 포함됨")
        if any("/.codex/" in name or name.endswith("/AGENTS.md") for name in names):
            raise ValueError("로컬 지침이 package에 포함됨")
    return metadata


def smoke_native(platform_tarball: Path, version: str) -> None:
    if (sys.platform, platform.machine()) != ("darwin", "arm64"):
        raise RuntimeError("native smoke는 macOS arm64 runner에서 실행해야 합니다.")
    sys.path.insert(0, str(REPO_ROOT / "sdk/python/tests"))
    from app_server_harness import (
        MockResponsesServer,
        ev_completed,
        ev_response_created,
        sse,
    )

    with tempfile.TemporaryDirectory(prefix="claudex-smoke-") as temporary:
        root = Path(temporary).resolve()
        with tarfile.open(platform_tarball, "r:gz") as archive:
            archive.extractall(root, filter="data")
        package_root = root / "package/vendor" / TARGET
        binary = package_root / "bin/claudex"
        home = root / "home"
        home.mkdir()
        env = {
            "PATH": str(package_root / "codex-path")
            + os.pathsep
            + os.environ.get("PATH", ""),
            "HOME": str(home),
            "CODEX_HOME": str(home),
            "ZDOTDIR": str(home),
            "TMPDIR": str(root),
            "NO_PROXY": "127.0.0.1,localhost",
            "RUST_LOG": "warn",
        }
        result = subprocess.run(
            [str(binary), "--version"],
            env=env,
            cwd=root,
            capture_output=True,
            text=True,
            check=True,
            timeout=60,
        )
        if result.stdout.strip() != f"codex-cli {version}":
            raise ValueError(f"실제 바이너리 버전 불일치: {result.stdout.strip()}")
        subprocess.run(
            [str(package_root / "bin/codex-code-mode-host"), "--help"],
            env=env,
            capture_output=True,
            check=True,
            timeout=60,
        )
        with MockResponsesServer() as server:
            (home / "config.toml").write_text(f"""
model = "claudex-package-smoke"
model_provider = "claudex_smoke"
approval_policy = "never"
sandbox_mode = "workspace-write"
suppress_unstable_features_warning = true
[sandbox_workspace_write]
network_access = true
[features]
code_mode = true
code_mode_only = true
memories = false
apps = false
plugins = false
[features.code_mode_host]
enabled = true
disable_in_process_fallback = true
[analytics]
enabled = false
[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"
[model_providers.claudex_smoke]
name = "isolated package smoke"
base_url = "{server.url}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
""")
            arguments = {
                "cmd": "command -v rg && rg --version",
                "login": False,
                "yield_time_ms": 10000,
            }
            server.enqueue_sse(
                sse(
                    [
                        ev_response_created("claudex-smoke"),
                        {
                            "type": "response.output_item.done",
                            "item": {
                                "type": "custom_tool_call",
                                "call_id": "claudex-smoke",
                                "name": "exec",
                                "input": "text(JSON.stringify(await tools.exec_command("
                                + json.dumps(arguments)
                                + ")))",
                            },
                        },
                        ev_completed("claudex-smoke"),
                    ]
                )
            )
            server.enqueue_assistant_message(
                "CLAUDEX_PACKAGE_SMOKE_OK", response_id="claudex-done"
            )
            result = subprocess.run(
                [
                    str(binary),
                    "exec",
                    "--ephemeral",
                    "--skip-git-repo-check",
                    "--color",
                    "never",
                    "Run the requested package smoke tool and finish.",
                ],
                env=env,
                cwd=root,
                capture_output=True,
                text=True,
                timeout=120,
            )
            if result.returncode != 0:
                raise RuntimeError(
                    "격리된 package smoke 실패:\n" + result.stderr[-5000:]
                )
            outputs = [
                item["output"]
                for request in server.requests()
                if request.path == "/v1/responses"
                for item in request.input()
                if item.get("type") == "custom_tool_call_output"
                and item.get("call_id") == "claudex-smoke"
            ]
            if not outputs:
                raise ValueError("code-mode helper 도구 실행 결과가 없습니다.")
            output = outputs[-1]
            if not isinstance(output, str):
                output = next(
                    part["text"]
                    for part in output
                    if part.get("text", "").startswith("{")
                )
            execution = json.loads(output)
            reported = execution.get("output", "")
            if execution.get("exit_code") != 0 or "ripgrep" not in reported:
                raise ValueError("패키지 code-mode 도구 실행 실패")
            if not Path(reported.splitlines()[0].strip()).is_relative_to(package_root):
                raise ValueError("패키지 외부의 ripgrep을 실행했습니다.")
            if "CLAUDEX_PACKAGE_SMOKE_OK" not in result.stdout:
                raise ValueError("모의 API turn이 완료되지 않았습니다.")
    print("native version, standalone code-mode host, bundled rg: OK")


def integrity(path: Path) -> str:
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha512").digest()
    return "sha512-" + base64.b64encode(digest).decode()


def npm_view(spec: str, field: str) -> object | None:
    result = subprocess.run(
        ["npm", "view", spec, field, "--json"], capture_output=True, text=True
    )
    if result.returncode == 0:
        return json.loads(result.stdout) if result.stdout.strip() else None
    try:
        code = json.loads(result.stdout).get("error", {}).get("code")
    except json.JSONDecodeError:
        code = None
    if code == "E404":
        return None
    raise RuntimeError(
        f"npm metadata 조회 실패 ({code or 'unknown'}). 인증 또는 네트워크 상태를 확인하세요."
    )


def wait_for_integrity(path: Path, version: str) -> None:
    expected = integrity(path)
    for attempt in range(2):
        actual = npm_view(f"{PACKAGE}@{version}", "dist.integrity")
        if actual is not None:
            if actual != expected:
                raise ValueError(f"{version}의 게시 후 integrity 불일치")
            return
        if attempt == 0:
            print(
                f"{version}: registry 처리 중, 15분 뒤 한 번 재확인합니다.", flush=True
            )
            time.sleep(900)
    raise RuntimeError(
        f"{version}: 게시 요청은 수락됐지만 registry에서 아직 조회되지 않습니다. 재게시하지 말고 registry 노출을 확인한 뒤 실패한 publish job만 재실행하세요."
    )


def publish_one(path: Path, version: str, tag: str) -> None:
    existing = npm_view(f"{PACKAGE}@{version}", "dist.integrity")
    if existing is not None:
        if existing != integrity(path):
            raise ValueError(
                f"{version}은 이미 다른 내용으로 배포됐습니다. 새 버전이 필요합니다."
            )
        if npm_view(PACKAGE, f"dist-tags.{tag}") != version:
            raise ValueError(
                f"{version} 배포 내용은 일치하지만 {tag}가 다릅니다. 자동으로 태그를 되돌리지 않습니다."
            )
        print(f"{version}: 동일 산출물이 이미 게시됨")
        return
    subprocess.run(
        ["npm", "publish", str(path), "--access", "public", "--tag", tag], check=True
    )
    wait_for_integrity(path, version)


def verify_install(version: str) -> None:
    with tempfile.TemporaryDirectory(prefix="claudex-install-") as temporary:
        root = Path(temporary)
        for spec in (f"{PACKAGE}@{version}", f"{PACKAGE}@latest"):
            subprocess.run(
                [
                    "npm",
                    "install",
                    "--global",
                    spec,
                    "--prefix",
                    str(root),
                    "--ignore-scripts",
                ],
                check=True,
            )
            result = subprocess.run(
                [str(root / "bin/claudex"), "--version"],
                capture_output=True,
                text=True,
                check=True,
                timeout=60,
            )
            if result.stdout.strip() != f"codex-cli {version}":
                raise ValueError("npm 설치 또는 latest 버전 불일치")
        installed = json.loads(
            (root / "lib/node_modules/@bluehansl/claudex/package.json").read_text()
        )
        if installed["version"] != version:
            raise ValueError("root npm 설치 버전 불일치")
    print("version-specific/latest temporary-prefix install: OK")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--platform", type=Path)
    parser.add_argument("--check-inputs", action="store_true")
    parser.add_argument("--package-filter", default=TAG)
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--publish", action="store_true")
    parser.add_argument("--verify-install", action="store_true")
    args = parser.parse_args()
    stable_version(args.version)
    if args.check_inputs or args.publish:
        validate_inputs(
            args.version,
            args.package_filter,
            os.environ.get("GITHUB_REF", ""),
            os.environ.get("GITHUB_REPOSITORY", ""),
        )
    if args.root or args.platform or args.publish or args.smoke:
        if not args.root or not args.platform:
            parser.error("--root와 --platform이 모두 필요합니다.")
        inspect_tarball(args.root, args.version, native=False)
        inspect_tarball(args.platform, args.version, native=True)
        print("root/platform package metadata: OK")
    if args.smoke:
        smoke_native(args.platform, args.version)
    if args.publish:
        if os.environ.get("GITHUB_ACTIONS") != "true":
            raise ValueError("자동 publish는 GitHub Actions에서만 허용합니다.")
        latest = npm_view(PACKAGE, "dist-tags.latest")
        if isinstance(latest, str) and stable_version(latest) > stable_version(
            args.version
        ):
            raise ValueError("latest를 이전 버전으로 되돌릴 수 없습니다.")
        native_latest = npm_view(PACKAGE, f"dist-tags.{TAG}")
        if isinstance(native_latest, str) and stable_version(
            native_latest.removesuffix(f"-{TAG}")
        ) > stable_version(args.version):
            raise ValueError(
                "더 최신 플랫폼 버전이 이미 게시됐습니다. 이전 배포로 태그를 되돌리지 않습니다."
            )
        publish_one(args.platform, f"{args.version}-{TAG}", TAG)
        publish_one(args.root, args.version, "latest")
        if npm_view(PACKAGE, "dist-tags.latest") != args.version:
            raise ValueError("게시 후 latest 태그 불일치")
    if args.verify_install:
        verify_install(args.version)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Claudex release verification failed: {error}", file=sys.stderr)
        raise SystemExit(1)
