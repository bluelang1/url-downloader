# URL Downloader (Rust/egui, macOS)

## 검증
- `cargo test --locked` - 68개, ~5초. 로컬 Range 서버로 분할·이어받기까지 검증
- `cargo build --release 2>&1 | grep -c ^warning` - 경고 0 유지
- 앱 확인은 `cargo run --release`. 로컬 패키징·재설치(bundle.sh)는 CI·비상용
- 의존성을 바꾸면 고지 파일 재생성: `cargo about generate --fail about.hbs -o THIRD-PARTY-LICENSES.html` (cargo-about, `--features cli` 로 설치)

## 개발용 환경변수 (src/main.rs)
- `URL_DOWNLOADER_INPUT` / `URL_DOWNLOADER_AUTOSTART=1` - 입력 채우고 자동 시작
- `URL_DOWNLOADER_START_VIEW=help|about` - 시작 화면
- `URL_DOWNLOADER_MODE=url|s3` / `URL_DOWNLOADER_BUCKET=<이름>` - 입력 모드·버킷. 켤 때 바로 저장하지는 않지만, 실행 중 다른 설정이 바뀌어 config.json 을 저장하면 이 값도 함께 저장됨
- `URL_DOWNLOADER_SCREENSHOT=<path.ppm>` (+`_FRAMES=n`) - 프레임버퍼 덤프, `sips -s format png` 로 변환
- `URL_DOWNLOADER_UPDATE_API=<url>` - 릴리스 API 를 가짜 JSON 으로 대체
- `URL_DOWNLOADER_STS_ENDPOINT=<url>` - MFA 세션 발급(STS GetSessionToken)을 가짜 서버로 대체

## S3 모드 (src/s3.rs + main.rs)
- 외부 CLI 없이 직접 구현: `~/.aws/credentials` 읽기 → SigV4 presign → 기존 다운로드 엔진. `~/.aws/*` 는 읽기 전용, 세션은 config.json 옆 `s3_session.json`(0600)에만 저장
- S3 항목의 식별자는 `s3://버킷/키`(메타·표시용), 요청 주소는 시작·재개·403 때마다 재발급. 비밀 키·토큰·MFA 코드·서명된 주소는 화면·오류·파일에 남기지 않음
- 설정 키(config.json): `s3_profile`(기본 default), `s3_session_profile`, `s3_mfa_serial`
- 화면 확인은 `HOME=<임시 폴더>` 로 실행해 실제 설정·자격증명을 건드리지 않음. 실제 AWS 호출과 MFA 입력은 사용자가 직접

## 릴리스 (승인 후에만)
- Cargo.toml version 올림 → `git tag vX.Y.Z` → main+태그 푸시 → Release 워크플로가 universal zip 업로드. 태그 ≠ Cargo 버전이면 CI 실패
- Release 워크플로는 `build`(읽기 권한: 테스트·번들·zip artifact) → `publish`(쓰기 권한, 체크아웃 없이 `gh release create --verify-tag`) 두 작업. 액션은 커밋 SHA 고정이라 올릴 때 `gh api repos/<owner>/<repo>/git/ref/tags/<tag>` 로 SHA 를 다시 조회
- 자체 업데이트는 `https://github.com/bluelang1/url-downloader/releases/download/` 주소만 설치(https 리다이렉트만 따라감). 받은 파일에는 `com.apple.quarantine` 을 달지만 업데이트 번들에는 달지 않음
- 배포 확인: `gh api repos/bluelang1/url-downloader/releases/latest --jq .tag_name` 와 설치 앱 `--check-update`. 미인증 API 는 IP 당 60회/시간이라 리뷰 중 많이 찌르면 앱 확인이 403 - `gh api` 로 대신 확인하고 리셋(`x-ratelimit-reset`) 후 재시도
