#!/bin/sh
# 릴리스 zip 에 minisign 서명(<zip>.minisig)을 만들어 같은 릴리스에 올린다.
# Release 워크플로가 끝난 뒤 개발자 컴퓨터에서 직접 실행한다. 서명 키는 GitHub 에 두지 않는다.
#   scripts/sign-release.sh vX.Y.Z
# 비밀키 비밀번호는 minisign 이 터미널에서 직접 묻는다 (이 스크립트는 읽지 않는다).
# 키 위치를 바꾸려면 URL_DOWNLOADER_SIGN_KEY / URL_DOWNLOADER_SIGN_PUB 를 지정한다.
set -eu

REPO=bluelang1/url-downloader
KEY="${URL_DOWNLOADER_SIGN_KEY:-$HOME/.config/url-downloader/release-main.key}"
PUB="${URL_DOWNLOADER_SIGN_PUB:-$HOME/.config/url-downloader/release-main.pub}"

if [ "$#" -ne 1 ]; then
  echo "사용법: $0 vX.Y.Z" >&2
  exit 2
fi
TAG=$1
case "$TAG" in
  v[0-9]*) ;;
  *) echo "태그는 vX.Y.Z 형식이어야 합니다: $TAG" >&2; exit 2 ;;
esac

for cmd in gh minisign shasum; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "$cmd 명령이 필요합니다" >&2; exit 1; }
done

if [ ! -f "$KEY" ] || [ ! -f "$PUB" ]; then
  echo "서명 키 파일이 없습니다:" >&2
  echo "  비밀키 $KEY" >&2
  echo "  공개키 $PUB" >&2
  echo "만드는 방법 (비밀번호를 꼭 설정하고, 비밀키는 백업해 두세요):" >&2
  echo "  mkdir -p \"$(dirname "$KEY")\" && minisign -G -s \"$KEY\" -p \"$PUB\"" >&2
  echo "그다음 공개키 파일의 두 번째 줄을 src/main.rs 의 UPDATE_PUBLIC_KEYS 에 넣고 새 버전을 배포해야 합니다." >&2
  exit 1
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/url-downloader-sign.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM

gh release download "$TAG" --repo "$REPO" --pattern '*.zip' --dir "$WORK"

COUNT=$(find "$WORK" -maxdepth 1 -type f -name '*.zip' | wc -l | tr -d ' ')
if [ "$COUNT" -ne 1 ]; then
  echo "릴리스 $TAG 의 zip 이 정확히 하나가 아닙니다 ($COUNT 개). 중단합니다" >&2
  exit 1
fi
ZIP=$(find "$WORK" -maxdepth 1 -type f -name '*.zip')
NAME=$(basename "$ZIP")
case "$NAME" in
  "URL-Downloader-$TAG-"*.zip) ;;
  *) echo "zip 파일명이 태그와 맞지 않습니다: $NAME" >&2; exit 1 ;;
esac

echo "파일:   $NAME"
echo "크기:   $(wc -c < "$ZIP" | tr -d ' ') 바이트"
echo "sha256: $(shasum -a 256 "$ZIP" | cut -d ' ' -f 1)"

# trusted comment 는 서명에 포함되어 위조할 수 없다. 앱은 이 값이 받은 파일명과 같은지 확인한다
minisign -S -s "$KEY" -t "file:$NAME" -m "$ZIP"
minisign -V -p "$PUB" -m "$ZIP"

gh release upload "$TAG" "$ZIP.minisig" --repo "$REPO"
echo "올림: $NAME.minisig"
