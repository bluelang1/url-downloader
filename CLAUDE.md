# URL Downloader (Rust/egui, macOS)

## 검증
- `cargo test --locked` - 95개(+릴리스 전용 무시 2개), ~5초. 로컬 Range 서버로 분할·이어받기까지 검증
- `cargo build --release 2>&1 | grep -c ^warning` - 경고 0 유지
- 앱 확인은 `cargo run --release`. 로컬 패키징·재설치(bundle.sh)는 CI(Release build)·비상용 release.sh 용
- 의존성을 바꾸면 고지 파일 재생성: `cargo about generate --fail about.hbs -o THIRD-PARTY-LICENSES.html` (cargo-about, `--features cli` 로 설치)

## 개발용 환경변수 (src/main.rs)
- `URL_DOWNLOADER_INPUT` / `URL_DOWNLOADER_AUTOSTART=1` - 입력 채우고 자동 시작
- `URL_DOWNLOADER_START_VIEW=help|about` - 시작 화면
- `URL_DOWNLOADER_MODE=url|s3` / `URL_DOWNLOADER_BUCKET=<이름>` - 입력 모드·버킷. 켤 때 바로 저장하지는 않지만, 실행 중 다른 설정이 바뀌어 config.json 을 저장하면 이 값도 함께 저장됨
- `URL_DOWNLOADER_SCREENSHOT=<path.ppm>` (+`_FRAMES=n`) - 프레임버퍼 덤프, `sips -s format png` 로 변환
- `URL_DOWNLOADER_UPDATE_API=<url>` - 릴리스 API 를 가짜 JSON 으로 대체. 업데이트 확인 화면까지만 시험 가능(설치는 공식 릴리스 주소만 허용)
- `URL_DOWNLOADER_STS_ENDPOINT=<url>` - MFA 세션 발급(STS GetSessionToken)을 가짜 서버로 대체

## S3 모드 (src/s3.rs + main.rs)
- 외부 CLI 없이 직접 구현: `~/.aws/credentials` 읽기 → SigV4 presign → 기존 다운로드 엔진. `~/.aws/*` 는 읽기 전용, 세션은 config.json 옆 `s3_session.json`(0600)에만 저장
- S3 항목의 식별자는 `s3://버킷/키`(메타·표시용), 요청 주소는 시작·재개·403 때마다 재발급. 비밀 키·토큰·MFA 코드·서명된 주소는 화면·오류·파일에 남기지 않음
- 설정 키(config.json): `s3_profile`(기본 default), `s3_session_profile`, `s3_mfa_serial`
- 화면 확인은 `HOME=<임시 폴더>` 로 실행해 실제 설정·자격증명을 건드리지 않음. 실제 AWS 호출과 MFA 입력은 사용자가 직접

## 릴리스 (승인 후에만)
- Cargo.toml version 올림 → `git tag vX.Y.Z` → main+태그 푸시 → Release 워크플로가 build → sign → verify → publish. 서명 키는 환경 `release` 의 시크릿(`MINISIGN_KEY` 비밀키 파일 내용, `MINISIGN_PASSWORD`). 비상시 `scripts/release.sh vX.Y.Z`. 태그는 vX.Y.Z 만(사전 릴리스 태그 금지, build 가 형식도 확인)
- 권한 분리(작업마다 하나만): 시크릿을 읽는 `sign` 은 쓰기 토큰·체크아웃 없음, 쓰기 토큰을 가진 `publish` 는 시크릿·체크아웃 없음, 리포 코드를 실행하는 `build`·`verify` 는 시크릿·쓰기 토큰 없음. 최상위 `contents: read`, 같은 태그는 `concurrency` 로 한 번에 하나
  - `build`(macOS): 태그 형식·Cargo 버전 일치 → `cargo test --locked -- --ignored --exact security_tests::release_keys_are_configured`(UPDATE_PUBLIC_KEYS 서로 다른 키 2개 이상, `1 passed` 확인) → `cargo test --locked` → `UNIVERSAL=1 INSTALL=0 ./bundle.sh` → artifact `release-zip`(1일)
  - `sign`(ubuntu, `environment: release`): zip 이 정확히 하나·이름 확인 → apt 로 minisign 설치(noble 은 0.11) → 비밀키를 `umask 077` 로 `$RUNNER_TEMP` 에 쓰고 비밀번호는 표준입력으로 `minisign -S -t "file:<zip>"` → 키 파일 삭제(trap + `if: always()`) → artifact `release-sig`. minisign 은 표준입력이 터미널이 아니어도 비밀번호 한 줄을 읽는다(get_password → fgets)
  - `verify`(macOS): 두 artifact 로 `URL_DOWNLOADER_VERIFY_ZIP=<zip> cargo test --locked -- --ignored --exact security_tests::release_artifact_verifies`, `1 passed` 확인
  - `publish`(ubuntu, `contents: write`): 같은 태그 릴리스(초안 포함)가 있으면 실패 → `gh release create --verify-tag --generate-notes --draft` → zip·.minisig 업로드(이름·크기·state) 확인 → `gh release edit --draft=false`
- 환경·시크릿 설정은 `scripts/setup-release-env.sh [비밀키 파일]`(리포 설정을 바꾸므로 사람이 실행): `PUT .../environments/release`(`deployment_branch_policy[custom_branch_policies]=true`) → `POST .../deployment-branch-policies -f name='v*' -f type=tag`(이미 있으면 건너뜀, 중복이면 API 가 303) → `gh secret set ... --env release`. 암호화된 비밀키만 받음
- publish 가 초안만 만들고 실패한 경우: GitHub 에서 그 초안을 지운 뒤 하루 안에 "Re-run failed jobs"(artifact 보존 1일). 하루가 지났으면 전체 재실행
- 액션은 커밋 SHA 고정이라 올릴 때 `gh api repos/<owner>/<repo>/git/ref/tags/<tag>` 로 SHA 를 다시 조회
- `scripts/release.sh`(비상용)는 CI 산출물을 받지 않고 터미널에서만 돈다(표준입력이 터미널이 아니면 중단, 건너뛰는 옵션 없음). 순서: 키 확인(키 파일 → 로컬·원격 태그 커밋 일치 → 그 커밋의 Release 실행이 끝났고 `build` 작업 성공(sign·verify·publish 실패는 허용) → 릴리스 목록에 같은 태그 없음(워크플로가 게시했다면 여기서 멈추는 게 정상, 초안이면 "지운 뒤 다시 실행") → 태그를 임시 워크트리로 꺼내 공개키가 UPDATE_PUBLIC_KEYS 에 있는지 grep, `//` 줄 제외) → 커밋 확인(직전 태그부터의 `git log`·`git diff --stat` 을 보여 주고 태그 이름을 그대로 입력받음) → 빌드(`release_keys_are_configured` + bundle.sh) → 서명(minisign -S, -V) → 앱 코드로 검증(`release_artifact_verifies`, `1 passed` 확인) → 초안(zip·.minisig 업로드 확인) → 공개. 키 위치 `URL_DOWNLOADER_SIGN_KEY`/`_PUB`(기본 `$HOME/.config/url-downloader/release-main.key`/`.pub`)
- `gh release view <tag>` 도 초안을 찾지만(gh 의 fetchDraftRelease) 최근 것만 보므로, 워크플로·스크립트 모두 `gh api repos/<repo>/releases --paginate` 에서 `tag_name` 으로 찾는다
- 앱은 이름이 정확히 `URL-Downloader-<tag>-macos-universal.zip` 인 자산 + 그 `.minisig` 가 둘 다 있는 릴리스만 설치 가능한 업데이트로 제안. 새 버전인데 짝이 없으면 "서명 확인 전" 안내만(설치 버튼 없음, `--check-update` 종료코드 2. 0 은 최신·설치 가능, 1 은 확인 실패). 설치는 서명 파일 먼저(크기·형식) → zip(멈춤 타임아웃 30초, 전체 시간 제한 없음, 크기 상한 200MB: API `size`·Content-Length·받은 양) → 검증 → 압축 풀기. 작업 폴더는 임의 이름 0700 새 폴더
- 자체 업데이트는 `https://github.com/bluelang1/url-downloader/releases/download/` 주소만 설치(https 리다이렉트만 따라감). 받은 파일에는 `com.apple.quarantine` 을 달지만 업데이트 번들에는 달지 않음
- 업데이트 설치 조건: `UPDATE_PUBLIC_KEYS`(src/main.rs, 비면 전부 거부. 실제 키는 리포 주인이 넣는다) 중 하나로 `.minisig` 검증 + trusted comment `file:<zip 파일명>` + 태그·주소·파일명 버전 일치 + 현재보다 높은 버전
- 남는 위험: 키가 GitHub 환경 시크릿에 있으므로 GitHub 계정을 탈취해 워크플로·태그를 바꿀 수 있는 공격자는 서명할 수 있음(릴리스 자산만 바꿀 수 있는 토큰 유출·전송 중 변조는 막음). 서명 키 유출 시 무력(예비 키는 GitHub 에 두지 않으며 주 키 교체용), 정식 서명된 예전 버전 중 현재보다 높은 버전으로의 고정은 못 막음, 0.5.0 이하 설치본은 서명을 확인하지 않음(서명 검증이 들어간 첫 버전으로 올라온 뒤부터 보호), 태그가 가리키는 커밋이 그대로 서명됨(사람의 커밋 확인 단계는 비상용 release.sh 에만 있음)
- 배포 확인: `gh api repos/bluelang1/url-downloader/releases/latest --jq .tag_name` 와 설치 앱 `--check-update`. 미인증 API 는 IP 당 60회/시간이라 리뷰 중 많이 찌르면 앱 확인이 403 - `gh api` 로 대신 확인하고 리셋(`x-ratelimit-reset`) 후 재시도
