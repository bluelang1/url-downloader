# URL Downloader (Rust/egui, macOS)

## 검증
- `cargo test --locked` - 95개(+릴리스 전용 무시 2개), ~5초. 로컬 Range 서버로 분할·이어받기까지 검증
- `cargo build --release 2>&1 | grep -c ^warning` - 경고 0 유지
- 앱 확인은 `cargo run --release`. 로컬 재설치는 `./bundle.sh`(/Applications 에 설치, `INSTALL=0` 이면 빌드만)
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
- 절차: Cargo.toml version 올림 → `cargo about generate ...` 로 고지 파일 갱신(버전 표기) → `git tag vX.Y.Z` → main+태그 푸시. Release 워크플로가 build → sign → verify → publish 로 서명된 릴리스를 공개. 태그는 vX.Y.Z 만(사전 릴리스 태그 금지)
- 서명 키는 GitHub 환경 `release`(태그 `v*` 전용)의 시크릿에만 있고 로컬 사본은 없음: `MINISIGN_KEY`/`MINISIGN_PASSWORD`(주), `MINISIGN_SPARE_KEY`/`MINISIGN_SPARE_PASSWORD`(예비, 워크플로 미사용). 시크릿은 다시 읽을 수 없으니 환경·시크릿을 지우거나 덮어쓰지 말 것(기존 설치본의 자동 업데이트가 끊김)
- 워크플로 권한 분리를 깨지 말 것: 시크릿은 `sign` 만(쓰기 토큰·체크아웃 없음), 쓰기 토큰은 `publish` 만(시크릿·체크아웃 없음), 리포 코드는 `build`·`verify` 만 실행. 상세는 release.yml 머리 주석. 액션은 커밋 SHA 고정(올릴 때 `gh api repos/<owner>/<repo>/git/ref/tags/<tag>` 로 조회)
- 앱의 `UPDATE_PUBLIC_KEYS`(src/main.rs)는 위 두 키의 공개키. 바꾸면 기존 설치본이 새 릴리스를 설치하지 못함. 비었거나 1개면 build 가 실패(`security_tests::release_keys_are_configured`)
- 앱은 이름이 정확히 `URL-Downloader-<tag>-macos-universal.zip` 인 자산과 그 `.minisig` 가 둘 다 있고, 서명·파일명·버전이 맞는 더 높은 버전만 설치. `--check-update` 종료코드: 0 최신·설치 가능, 1 확인 실패, 2 새 버전 있으나 서명 없음
- publish 가 초안만 만들고 실패한 경우: GitHub 에서 초안을 지운 뒤 하루 안에 "Re-run failed jobs"(artifact 보존 1일), 지났으면 전체 재실행. 릴리스 조회는 초안까지 잡히게 `gh api repos/<repo>/releases --paginate` 에서 `tag_name` 으로
- `scripts/release.sh vX.Y.Z` 는 CI 를 쓸 수 없을 때의 비상용(로컬 빌드·서명, 로컬 키 파일 필요 - 지금은 키가 시크릿에만 있어 그대로는 못 씀). `scripts/setup-release-env.sh` 는 환경·시크릿을 새로 만들 때만
- 남는 위험: GitHub 계정을 탈취해 워크플로·태그를 바꿀 수 있으면 서명 가능, 0.5.0 이하 설치본은 서명 미확인. 상세는 README "업데이트 서명 검증"
- 배포 확인: `gh api repos/bluelang1/url-downloader/releases/latest --jq '.tag_name, .assets[].name'` 와 설치 앱 `--check-update`. 미인증 API 는 IP 당 60회/시간이라 많이 찌르면 앱 확인이 403 - `gh api` 로 대신 확인
