#!/bin/sh
# 릴리스를 개발자 컴퓨터에서 만든다: 태그 소스를 깨끗한 작업 폴더에서 빌드 → minisign 서명 → GitHub 릴리스 생성.
#   scripts/release.sh vX.Y.Z
# 신뢰의 뿌리는 이 컴퓨터의 서명 키다. CI(Release 워크플로)는 같은 태그를 검증만 하고 아무것도 게시하지 않으며,
# 이 스크립트는 CI 가 만든 파일을 받지 않는다 (서명할 zip 은 여기서 직접 빌드한다).
# 비밀키 비밀번호는 minisign 이 터미널에서 직접 묻는다 (이 스크립트는 읽지 않는다).
# 키 위치를 바꾸려면 URL_DOWNLOADER_SIGN_KEY / URL_DOWNLOADER_SIGN_PUB 를 지정한다.
set -eu

REPO=bluelang1/url-downloader
KEY="${URL_DOWNLOADER_SIGN_KEY:-$HOME/.config/url-downloader/release-main.key}"
PUB="${URL_DOWNLOADER_SIGN_PUB:-$HOME/.config/url-downloader/release-main.pub}"

die() {
  echo "중단: $*" >&2
  exit 1
}

# ---- a. 인자·도구·키 파일 ----
if [ "$#" -ne 1 ]; then
  echo "사용법: $0 vX.Y.Z" >&2
  exit 2
fi
TAG=$1
# 사전 릴리스 태그(v0.6.0-rc1 등)는 쓰지 않는다: 앱의 버전 비교가 정식 버전과 구분하지 못한다
if ! printf '%s\n' "$TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "태그는 vX.Y.Z 형식이어야 합니다 (사전 릴리스 태그는 쓰지 않음): $TAG" >&2
  exit 2
fi

for cmd in git gh minisign cargo rustup shasum ditto; do
  command -v "$cmd" >/dev/null 2>&1 || die "$cmd 명령이 필요합니다"
done

if [ ! -f "$KEY" ] || [ ! -f "$PUB" ]; then
  echo "서명 키 파일이 없습니다:" >&2
  echo "  비밀키 $KEY" >&2
  echo "  공개키 $PUB" >&2
  echo "만드는 방법 (비밀번호를 꼭 설정하고, 비밀키는 백업해 두세요):" >&2
  echo "  mkdir -p \"$(dirname "$KEY")\" && minisign -G -s \"$KEY\" -p \"$PUB\"" >&2
  echo "그다음 공개키 파일의 두 번째 줄을 src/main.rs 의 UPDATE_PUBLIC_KEYS 에 넣고 (예비 키 포함 2개 이상) 새 버전을 배포해야 합니다." >&2
  exit 1
fi
PUBKEY=$(sed -n 2p "$PUB" | tr -d '[:space:]')
[ -n "$PUBKEY" ] || die "공개키 파일의 두 번째 줄이 비어 있습니다: $PUB"

ROOT=$(cd "$(dirname "$0")/.." && git rev-parse --show-toplevel)
cd "$ROOT"

# ---- b. 로컬 태그 = 원격 태그 ----
LOCAL=$(git rev-parse -q --verify "refs/tags/$TAG^{commit}" 2>/dev/null) || die "로컬 태그 $TAG 가 없습니다 (git tag $TAG 후 main 과 태그를 푸시하세요)"
REMOTE_REFS=$(git ls-remote --tags origin "refs/tags/$TAG" "refs/tags/$TAG^{}") || die "원격 태그를 확인하지 못했습니다"
# 주석 태그면 ^{} 줄(태그가 가리키는 커밋), 가벼운 태그면 태그 줄
REMOTE=$(printf '%s\n' "$REMOTE_REFS" | awk -v r="refs/tags/$TAG^{}" '$2 == r { print $1 }')
if [ -z "$REMOTE" ]; then
  REMOTE=$(printf '%s\n' "$REMOTE_REFS" | awk -v r="refs/tags/$TAG" '$2 == r { print $1 }')
fi
[ -n "$REMOTE" ] || die "원격(origin)에 태그 $TAG 가 없습니다. 먼저 태그를 푸시하세요"
[ "$REMOTE" = "$LOCAL" ] || die "로컬 태그($LOCAL)와 원격 태그($REMOTE)가 다른 커밋입니다"
echo "태그:   $TAG ($LOCAL)"

# ---- c. 같은 커밋에서 Release 워크플로(검증 전용)가 성공했는지 ----
RUN=$(gh run list --repo "$REPO" --workflow Release --branch "$TAG" --limit 1 \
  --json status,conclusion,headSha --jq '.[0] | select(.) | "\(.status) \(.conclusion) \(.headSha)"') \
  || die "Release 워크플로 실행 기록을 확인하지 못했습니다 (gh auth status 확인)"
[ -n "$RUN" ] || die "태그 $TAG 의 Release 워크플로 실행 기록이 없습니다"
set -- $RUN
[ "$1" = "completed" ] || die "태그 $TAG 의 Release 워크플로가 아직 끝나지 않았습니다 ($1)"
[ "$2" = "success" ] || die "태그 $TAG 의 Release 워크플로가 성공하지 않았습니다 ($2)"
[ "${3:-}" = "$LOCAL" ] || die "Release 워크플로가 검증한 커밋(${3:-?})이 태그 커밋과 다릅니다"
echo "CI:     Release 워크플로 성공"

if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  die "릴리스 $TAG 가 이미 있습니다 (초안 포함). 확인 후 필요하면 GitHub 에서 직접 정리하세요"
fi

# ---- d. 깨끗한 소스에서 빌드 (작업 중인 체크아웃의 수정 사항이 섞이지 않게 태그를 새 워크트리로 꺼낸다) ----
WORK=$(mktemp -d "${TMPDIR:-/tmp}/url-downloader-release.XXXXXX")
SRC="$WORK/src"
cleanup() {
  if [ -d "$SRC" ]; then
    git -C "$ROOT" worktree remove --force "$SRC" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
  git -C "$ROOT" worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

git worktree add --quiet --detach "$SRC" "$LOCAL"

# ---- e. 서명할 키가 그 태그의 앱에 들어 있는지 (빌드 전에 확인) ----
KEYS_BLOCK=$(awk '/^const UPDATE_PUBLIC_KEYS/ { p = 1 } p { print } p && /\];/ { exit }' "$SRC/src/main.rs")
if ! printf '%s\n' "$KEYS_BLOCK" | grep -Fq -- "\"$PUBKEY\""; then
  die "앱이 모르는 키로 서명하려 합니다: $PUB 의 공개키가 $TAG 의 UPDATE_PUBLIC_KEYS 에 없습니다"
fi

(
  cd "$SRC"
  cargo test --locked release_keys_are_configured -- --ignored
  UNIVERSAL=1 INSTALL=0 ./bundle.sh
)
NAME="URL-Downloader-$TAG-macos-universal.zip"
ZIP="$SRC/dist/$NAME"
[ -f "$ZIP" ] || die "빌드 결과 $NAME 이 없습니다"

# ---- f. 서명 ----
echo "파일:   $NAME"
echo "크기:   $(wc -c < "$ZIP" | tr -d ' ') 바이트"
echo "sha256: $(shasum -a 256 "$ZIP" | cut -d ' ' -f 1)"
# trusted comment 는 서명에 포함되어 위조할 수 없다. 앱은 이 값이 받은 파일명과 같은지 확인한다
minisign -S -s "$KEY" -t "file:$NAME" -m "$ZIP"
minisign -V -p "$PUB" -m "$ZIP"

# ---- g. 초안으로 만들고 두 파일이 다 올라간 것을 확인한 뒤 공개 (zip 만 있고 서명이 없는 상태를 사용자가 보지 않게) ----
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  die "릴리스 $TAG 가 이미 있습니다 (초안 포함)"
fi
gh release create "$TAG" "$ZIP" "$ZIP.minisig" --repo "$REPO" --verify-tag --title "$TAG" --generate-notes --draft

ZIP_SIZE=$(wc -c < "$ZIP" | tr -d ' ')
SIG_SIZE=$(wc -c < "$ZIP.minisig" | tr -d ' ')
ASSETS=$(gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets[] | "\(.name) \(.size) \(.state)"')
printf '%s\n' "$ASSETS" | grep -Fxq "$NAME $ZIP_SIZE uploaded" \
  || die "초안 릴리스에 $NAME 이 제대로 올라가지 않았습니다. 초안을 확인하세요: $ASSETS"
printf '%s\n' "$ASSETS" | grep -Fxq "$NAME.minisig $SIG_SIZE uploaded" \
  || die "초안 릴리스에 $NAME.minisig 가 제대로 올라가지 않았습니다. 초안을 확인하세요: $ASSETS"

gh release edit "$TAG" --repo "$REPO" --draft=false
echo "공개함: $TAG ($NAME, $NAME.minisig)"
