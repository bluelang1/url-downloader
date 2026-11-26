# URL Downloader (Rust/egui, macOS)

## 검증
- `cargo test --locked` - 90개(+릴리스 전용 무시 1개), ~5초. 로컬 Range 서버로 분할·이어받기까지 검증
- `cargo build --release 2>&1 | grep -c ^warning` - 경고 0 유지
- 앱 확인은 `cargo run --release`. 로컬 패키징·재설치(bundle.sh)는 CI 검증·release.sh·비상용
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
- Cargo.toml version 올림 → `git tag vX.Y.Z` → main+태그 푸시 → Release 워크플로(검증 전용) 성공 확인 → `scripts/release.sh vX.Y.Z` (로컬 빌드·서명·릴리스 생성, 비밀번호는 사람이 입력). 태그는 vX.Y.Z 만(사전 릴리스 태그 금지)
- Release 워크플로는 `build` 한 작업, 읽기 권한만: 태그·Cargo 버전 일치 → `cargo test --locked release_keys_are_configured -- --ignored`(UPDATE_PUBLIC_KEYS 서로 다른 키 2개 이상) → `cargo test --locked` → `UNIVERSAL=1 INSTALL=0 ./bundle.sh`. 게시·artifact 없음. 액션은 커밋 SHA 고정이라 올릴 때 `gh api repos/<owner>/<repo>/git/ref/tags/<tag>` 로 SHA 를 다시 조회
- `scripts/release.sh` 는 CI 산출물을 받지 않는다: 로컬·원격 태그 커밋 일치 + 그 커밋의 Release 실행 성공 확인 → 태그를 임시 워크트리로 꺼내 빌드 → 서명 공개키가 태그의 UPDATE_PUBLIC_KEYS 에 있는지 확인 → minisign 서명·검증 → 초안 릴리스에 zip·.minisig 업로드 확인 후 공개. 릴리스가 이미 있으면(초안 포함) 멈춤. 키 위치 `URL_DOWNLOADER_SIGN_KEY`/`_PUB`(기본 `$HOME/.config/url-downloader/release-main.key`/`.pub`)
- 앱은 zip + 같은 이름 `.minisig` 자산이 둘 다 있는 릴리스만 업데이트로 제안. 설치는 서명 파일 먼저(크기·형식) → zip(멈춤 타임아웃 30초, 전체 제한 없음) → 검증 → 압축 풀기. 작업 폴더는 임의 이름 0700 새 폴더
- 자체 업데이트는 `https://github.com/bluelang1/url-downloader/releases/download/` 주소만 설치(https 리다이렉트만 따라감). 받은 파일에는 `com.apple.quarantine` 을 달지만 업데이트 번들에는 달지 않음
- 업데이트 설치 조건: `UPDATE_PUBLIC_KEYS`(src/main.rs, 비면 전부 거부. 실제 키는 리포 주인이 넣는다) 중 하나로 `.minisig` 검증 + trusted comment `file:<zip 파일명>` + 태그·주소·파일명 버전 일치 + 현재보다 높은 버전
- 남는 위험: 서명 키 유출 시 무력, 정식 서명된 예전 버전 중 현재보다 높은 버전으로의 고정은 못 막음
- 배포 확인: `gh api repos/bluelang1/url-downloader/releases/latest --jq .tag_name` 와 설치 앱 `--check-update`. 미인증 API 는 IP 당 60회/시간이라 리뷰 중 많이 찌르면 앱 확인이 403 - `gh api` 로 대신 확인하고 리셋(`x-ratelimit-reset`) 후 재시도
