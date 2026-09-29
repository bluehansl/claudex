# Claudex

OpenAI Codex를 기반으로 한 비공식 커스텀 CLI입니다. npm 패키지와 실행 명령은 원본 Codex와 분리되어 있습니다.

## 설치와 실행

Node.js 환경에서 다음 명령으로 설치합니다. Rust/Cargo는 필요하지 않습니다.

```shell
npm install -g @bluehansl/claudex
claudex
```

현재 기본 배포 대상은 **macOS Apple Silicon (arm64, M1 이후)** 입니다.
다른 운영체제 패키지는 별도 요청 시 추가합니다.

## 업데이트

```shell
npm install -g @bluehansl/claudex@latest
claudex --version
```

특정 버전은 `npm install -g @bluehansl/claudex@<version>`으로 설치합니다.
업데이트 확인은 원본 `@openai/codex`가 아닌 `@bluehansl/claudex`를 기준으로 합니다.

## Claude 세션 연결

Claude Code 독립 세션과 통신할 때는 peer 이름을 지정합니다.

```shell
claudex --claude-peer my-claudex
claudex peer list
```

Claude Team 연결은 별도의 `--claude-team`, `--claude-team-agent` 옵션을 사용합니다.
peer 메시징과 Team inbox는 다른 프로토콜이며, 해당 세션은 공유 daemon 대신 로컬 프로세스로 실행됩니다.

## 개발과 배포

배포 workflow는 GitHub Actions에서 macOS ARM 패키지를 빌드하고,
검증 후 npm 플랫폼 패키지와 루트 패키지를 순서대로 게시할 수 있습니다.
일반 push는 배포를 실행하지 않으며, 수동 실행의 `publish=true`가 필요합니다.

배포자용 설정, npm Trusted Publisher 등록 및 재시도 절차:
[npm 배포 안내](codex-cli/scripts/README.md).

## Upstream

- [OpenAI Codex 원본 저장소](https://github.com/openai/codex)
- [Codex 공식 사용 문서](https://developers.openai.com/codex)
- [소스 빌드 안내](docs/install.md)

원본 Codex의 standalone/Homebrew 설치 명령은 Claudex 설치 명령이 아닙니다.
이 저장소는 [Apache-2.0](LICENSE) 라이선스를 따릅니다.
