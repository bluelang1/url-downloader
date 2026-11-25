# URL Downloader (Rust/egui, macOS)

## 검증
- `cargo test` - 27개, ~4초. 로컬 Range 서버로 분할·이어받기까지 검증
- `cargo build --release 2>&1 | grep -c ^warning` - 경고 0 유지
- 앱 확인은 `cargo run --release`. 로컬 패키징·재설치(bundle.sh)는 CI·비상용

## 개발용 환경변수 (src/main.rs)
- `URL_DOWNLOADER_INPUT` / `URL_DOWNLOADER_AUTOSTART=1` - 입력 채우고 자동 시작
- `URL_DOWNLOADER_START_VIEW=help|about` - 시작 화면
- `URL_DOWNLOADER_MODE=url|s3` / `URL_DOWNLOADER_BUCKET=<이름>` - 입력 모드·버킷 (설정 파일에는 저장 안 함)
- `URL_DOWNLOADER_SCREENSHOT=<path.ppm>` (+`_FRAMES=n`) - 프레임버퍼 덤프, `sips -s format png` 로 변환
- `URL_DOWNLOADER_UPDATE_API=<url>` - 릴리스 API 를 가짜 JSON 으로 대체

## 릴리스 (승인 후에만)
- Cargo.toml version 올림 → `git tag vX.Y.Z` → main+태그 푸시 → Release 워크플로가 universal zip 업로드. 태그 ≠ Cargo 버전이면 CI 실패
- 배포 확인: `gh api repos/bluelang1/url-downloader/releases/latest --jq .tag_name` 와 설치 앱 `--check-update`. 미인증 API 는 IP 당 60회/시간이라 리뷰 중 많이 찌르면 앱 확인이 403 - `gh api` 로 대신 확인하고 리셋(`x-ratelimit-reset`) 후 재시도
