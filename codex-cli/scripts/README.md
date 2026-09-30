# Claudex npm 배포

기본 배포는 GitHub Actions에서 빌드, 산출물 검증, npm 게시, 설치 검증까지 수행합니다.
개발 PC로 대용량 아티팩트를 내려받거나 로컬 npm 토큰으로 게시할 필요가 없습니다.

## 최초 설정

npm의 `@bluehansl/claudex` 패키지 Settings → Trusted publishing에 GitHub를 등록합니다.

| 항목 | 값 |
| --- | --- |
| Organization or user | `bluehansl` |
| Repository | `claudex` |
| Workflow filename | `claudex-platform-packages.yml` |
| Environment name | 비워 둠 |
| Allowed actions | 직접 `npm publish` 허용 |

등록에는 npm 계정의 대화형 인증/2FA가 필요할 수 있습니다.
`NPM_TOKEN`을 workflow나 저장소에 추가하지 않습니다.
최초 OIDC 게시가 확인되기 전에는 기존 게시 인증을 제거하지 않습니다.
자세한 요구사항은 [npm Trusted Publisher 문서](https://docs.npmjs.com/trusted-publishers/)를 참고합니다.

## 정식 배포

1. upstream과 커스텀을 병합하고 필요한 소스·실제 연동 검증을 마칩니다.
2. `claudex-<version>` 브랜치와 Cargo workspace 버전을 일치시키고 push합니다.
3. 해당 브랜치에서 다음 workflow를 실행합니다.

```bash
VERSION=0.159.0
gh workflow run claudex-platform-packages.yml \
  --repo bluehansl/claudex \
  --ref "claudex-${VERSION}" \
  -f version="${VERSION}" \
  -f packages=darwin-arm64 \
  -f publish=true
```

일반 push는 이 배포 workflow를 실행하지 않습니다.
수동 실행에서도 `publish` 기본값은 `false`입니다.
현재 자동 게시 플랫폼은 `darwin-arm64` 하나이며, 다른 플랫폼을 요청할 때 별도 확장합니다.

## GitHub에서 수행하는 검증

- 선택된 target의 고정 V8 archive/binding과 SHA-256 검증
- 동일 commit에서 CLI와 code-mode helper의 release 빌드
- root/platform package 이름·버전·플랫폼·optionalDependencies·repository 일치
- 안전한 tar 경로와 실행 파일 존재/권한
- 실제 native `--version`, standalone code-mode host 실행, 번들 ripgrep 실행
- 긴 `CODEX_HOME`에서 daemon start/update/version/stop (일반 updater IPC, `--from-cli` 우회 금지)
- 격리된 임시 HOME과 모의 Responses API 사용: 개인 OpenAI 인증정보 없이 실행
- 플랫폼 게시 후 루트 `latest` 게시
- 정확한 버전과 `latest`의 임시 prefix 설치 및 실행 확인

새 모델의 실제 서비스 호환성이나 개인 Claude 세션과의 메시징처럼 계정·환경에
의존하는 검증은 관련 기능을 변경했을 때 별도로 수행합니다.
이 확인을 위해 개인 OAuth 자격증명을 GitHub Secrets에 복사하지 않습니다.

검증/게시 구현은 `scripts/claudex_npm_release.py`입니다.
루트 npm 패키지도 GitHub에서 같은 commit으로 생성합니다.

## 빌드만 실행

```bash
gh workflow run claudex-platform-packages.yml \
  --repo bluehansl/claudex \
  --ref "claudex-${VERSION}" \
  -f version="${VERSION}" \
  -f packages=darwin-arm64 \
  -f publish=false
```

성공한 빌드만으로 배포 완료를 판단하지 않습니다.
`Verify and publish npm` job과 npm `latest`, 설치 검증 결과까지 확인해야 합니다.

## 실패와 재시도

- OIDC 인증 실패: npm 등록의 저장소·workflow 파일명·직접 게시 권한을 확인합니다.
  `npm whoami`는 OIDC 게시 인증 상태를 검증하는 명령이 아닙니다.
- 플랫폼 게시 후 루트 게시만 실패한 경우, 같은 run의 실패한 publish job을 재실행합니다.
  기존 버전의 integrity와 태그가 동일할 때만 중복 게시를 건너뜁니다.
- npm이 게시 요청을 수락했지만 registry 반영 중이면 15분 뒤 한 번 재확인합니다.
  요청 수락만으로 완료를 선언하거나 같은 버전을 다시 게시하지 않습니다.
  여전히 조회되지 않으면 job 재실행보다 registry 노출 확인이 먼저입니다.
- 이미 게시된 버전의 내용이 다르면 덮어쓰지 않습니다. 새 버전이 필요합니다.
  정식 배포의 빌드 전 검사에서도 이미 게시된 버전을 거절하여 불필요한 재빌드를 막습니다.
- 더 최신 버전이 이미 `latest`라면 과거 run이 이를 낮추지 않도록 중단합니다.
- 전체 workflow 재실행은 새 tarball을 만들 수 있으므로 부분 성공 복구 때는
  성공했던 build job의 아티팩트를 재사용하는 실패 job 재실행을 우선합니다.

## 수동 진단

일반 릴리스에는 로컬 다운로드가 필요 없습니다. 명시적인 로컬 검증/장애 조사 때만
`scripts/stage_npm_packages.py`, `codex-cli/scripts/build_npm_package.py` 및
아티팩트 다운로드를 사용합니다. 수동 게시도 플랫폼이 먼저, 루트가 마지막입니다.
