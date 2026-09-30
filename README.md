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

로컬 대화형 세션은 Claude Code의 통신 목록에 자동 등록됩니다.
저장된 대화 제목이 있으면 그 이름을 사용하며, `/rename`으로 변경하면 통신 이름도 갱신됩니다.

```shell
claudex
claudex peer list
```

대화창에서는 `/peers`로 상대를 선택하거나 `@` 뒤에 세션 이름의 앞부분을 입력합니다.
선택은 대상 참조를 입력할 뿐 메시지를 즉시 보내지는 않습니다. AI에게 전송할 내용을 요청하세요.
Claude에서도 `/list-agents`와 `@이름`으로 조회할 수 있으며, 공백이 있는 이름은 `@"세션 이름"`으로 표시됩니다.

`--claude-peer <이름>`은 대화 제목이 없을 때의 초기 이름을 지정합니다.
`--no-claude-peer`는 해당 실행의 로컬 통신 기능을 끕니다.
유효하지 않은 멘션 이름에는 기존 이름 또는 생성한 이름을 사용합니다.

소켓은 각 TUI가 소유하므로 공유 daemon에서도 세션이 서로 등록을 덮어쓰지 않습니다.
TUI와 서버는 같은 로컬 `CODEX_HOME`을 사용해야 하며, 서버는 Claudex 0.159.1 이상이어야 합니다.
구버전 서버가 실행 중이면 `/daemon`에서 갱신·재시작하거나 `--no-daemon`으로 실행합니다.
원격 서버와 임시(ephemeral) 대화는 자동 등록하지 않습니다.

Claude Team 연결은 별도의 `--claude-team`, `--claude-team-agent` 옵션을 사용합니다.
Team inbox와 peer 메시징은 별개이며, Team의 프로세스 종료 동작을 보호하기 위해 Team 연결은 독립 실행을 유지합니다.

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
