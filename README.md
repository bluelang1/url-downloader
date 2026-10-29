# URL Downloader

URL 목록을 붙여넣고 폴더만 고르면 한꺼번에 받아 주는 가벼운 macOS 데스크톱 앱입니다.
Rust + [egui](https://github.com/emilk/egui) 로 만들었고, 대용량 파일의 분할·이어받기를 지원합니다.

A small macOS desktop app: paste a list of URLs, pick a folder, and download them all.
Written in Rust with egui. Supports concurrency limits, multi-connection (segmented)
downloads and resuming interrupted transfers.

## 기능

- **붙여넣고 받기**: 한 줄에 URL 하나. 줄 번호와 상태(대기·진행·완료·실패)를 바로 보여줍니다.
- **원하는 파일명으로 저장**: `파일명<Tab>URL` 또는 `파일명 | URL` 형식으로 쓰면 그 이름으로 저장합니다.
  확장자는 원본 URL 의 것을 그대로 붙입니다. 파일명을 생략하면 URL 의 원본 파일명을 씁니다.
- **동시 다운로드 수 지정**: 한 번에 받을 파일 수(기본 3)와 파일당 분할 수(기본 4)를 UI 에서 정합니다.
- **분할 다운로드**: 서버가 HTTP Range 를 지원하면 파일 하나를 여러 연결로 나눠 받습니다.
- **이어받기**: 연결이 끊기면 받은 지점부터 자동 재시도(3회)하고, 그래도 실패하면 진행 기록을 남겨
  "다시 받기" 또는 앱 재실행 후 같은 폴더에 다시 받을 때 이어서 받습니다.
- **다시 받기**: 실패한 항목만, 또는 체크한 항목만 골라 다시 받을 수 있습니다.
- **이름 충돌 처리**: 같은 이름이 있으면 `이름 (1).ext` 처럼 번호를 붙입니다.
- **앱 안 도움말**: 메뉴 **도움말** 에 목차가 있는 상세 사용법(입력 형식·설정·이어받기·문제 해결)이 들어 있고, **정보** 에서 버전·라이선스·저장소 링크를 볼 수 있습니다.

## 설치

Rust 툴체인(`cargo`)이 필요합니다.

```sh
git clone https://github.com/bluelang1/url-downloader.git
cd url-downloader
./bundle.sh        # release 빌드 → dist/URL Downloader.app → /Applications 에 설치
```

개발 중에는 `cargo run --release` 로 바로 띄울 수 있습니다.

## 사용법

1. 입력창에 링크를 붙여넣습니다. 지원하는 줄 형식:

   ```
   https://example.com/files/photo.jpg
   여행 사진<Tab>https://example.com/files/photo.jpg      → 여행 사진.jpg
   여행 사진 | https://example.com/files/photo.jpg        → 여행 사진.jpg
   https://example.com/files/photo.jpg<Tab>여행 사진      → 여행 사진.jpg
   ```

   스프레드시트에서 "파일명, URL" 두 열을 복사해 붙이면 자동으로 탭 구분이 됩니다.
   `/ \ : * ? " < > |` 같은 금지 문자는 `_` 로 바뀝니다.

2. 필요하면 **동시 파일** / **파일당 분할** 숫자를 조정합니다.
   열리는 연결 수는 두 값의 곱입니다. 서버가 연결 수를 제한하면 분할 수를 1~2 로 낮추세요.

3. **다운로드** 를 누르고 저장할 폴더를 고릅니다.

## 동작 방식

- 시작할 때 `Range: bytes=0-0` 으로 1바이트만 요청해 서버의 Range 지원 여부와 전체 크기를 알아냅니다.
- Range 를 지원하고 파일이 1 MB 이상이면 구간을 나눠 `<이름>.part` 에 동시에 쓰고,
  구간별 진행 상황을 `<이름>.meta` 에 1초마다 저장합니다.
- 모든 구간이 끝나면 `.part` 를 최종 이름으로 바꾸고 `.meta` 를 지웁니다.
- 실패하면 `.part` 와 `.meta` 를 남깁니다. 다시 받을 때 크기와 ETag 가 같으면 이어받고, 다르면 처음부터 받습니다.
- Range 를 지원하지 않는 서버는 한 줄기로 받습니다(이어받기 불가).

## 개발

```sh
cargo test          # 파싱·파일명·분할/이어받기 테스트 (로컬 Range 테스트 서버 포함, 일부는 인터넷 필요)
cargo build --release
```

## 라이선스

[MIT](LICENSE)
