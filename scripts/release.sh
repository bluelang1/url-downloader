#!/bin/sh
# 비상용: 릴리스를 개발자 컴퓨터에서 만든다. 평소에는 태그를 푸시하면 Release 워크플로가
# build → sign → verify → publish 로 빌드·서명·게시하므로 이 스크립트를 쓰지 않는다.
# CI 가 서명·게시하지 못했을 때(시크릿 문제, GitHub Actions 장애 등)만, 같은 서명 키의 로컬 사본으로 실행한다.
# 순서: 키 확인 → 서명할 커밋 확인(태그 이름 입력) → 태그 소스를 깨끗한 작업 폴더에서 빌드
# → minisign 서명 → 태그 소스의 앱 코드로 검증 → 초안 릴리스 → 공개.
#   scripts/release.sh vX.Y.Z   (터미널에서 사람이 직접 실행. 확인을 건너뛰는 옵션은 없다)
# 그 태그의 Release 워크플로 build 작업(테스트·번들 빌드)이 성공했어야 한다. 워크플로가 게시까지 성공했다면
# 릴리스가 이미 있어 이 스크립트는 중단되는 것이 정상이다.
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

# 서명할 커밋을 사람이 확인해야 하므로 터미널에서만 실행한다 (자동화로 건너뛰는 옵션은 없다)
[ -t 0 ] || die "표준입력이 터미널이 아닙니다. 릴리스는 사람이 터미널에서 직접 실행해야 합니다"

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

# ---- c. 같은 커밋에서 Release 워크플로가 끝났고 그 build 작업(테스트·번들 빌드)이 성공했는지 ----
# 비상용이므로 sign·verify·publish 의 실패는 허용한다. 워크플로 전체가 성공했다면 이미 게시되어 아래 릴리스 확인에서 멈춘다
RUN=$(gh run list --repo "$REPO" --workflow Release --branch "$TAG" --limit 1 \
  --json databaseId,status,conclusion,headSha --jq '.[0] | select(.) | "\(.databaseId) \(.status) \(.conclusion) \(.headSha)"') \
  || die "Release 워크플로 실행 기록을 확인하지 못했습니다 (gh auth status 확인)"
[ -n "$RUN" ] || die "태그 $TAG 의 Release 워크플로 실행 기록이 없습니다"
set -- $RUN
RUN_ID=$1
[ "${2:-}" = "completed" ] || die "태그 $TAG 의 Release 워크플로가 아직 끝나지 않았습니다 (${2:-?}). 끝난 뒤 게시됐는지 먼저 확인하세요"
RUN_CONCLUSION=${3:-?}
[ "${4:-}" = "$LOCAL" ] || die "Release 워크플로가 검증한 커밋(${4:-?})이 태그 커밋과 다릅니다"
BUILD=$(gh run view "$RUN_ID" --repo "$REPO" --json jobs --jq '.jobs[] | select(.name == "build") | .conclusion') \
  || die "Release 워크플로의 작업 결과를 확인하지 못했습니다 (gh auth status 확인)"
[ "$BUILD" = "success" ] || die "태그 $TAG 의 Release 워크플로 build 작업이 성공하지 않았습니다 (${BUILD:-없음})"
echo "CI:     Release 워크플로 build 성공 (워크플로 전체: $RUN_CONCLUSION)"

# 같은 태그의 릴리스가 이미 있으면(초안 포함) 멈춘다. 태그 조회(gh release view)는 초안을 최근 것만 찾으므로
# 릴리스 목록 전체에서 tag_name 으로 찾는다
check_no_release() {
  STATE=$(gh api "repos/$REPO/releases" --paginate \
    --jq ".[] | select(.tag_name == \"$TAG\") | if .draft then \"draft\" else \"published\" end") \
    || die "릴리스 목록을 확인하지 못했습니다 (gh auth status 확인)"
  case "$STATE" in
    "") ;;
    *draft*) die "초안이 남아 있습니다 ($TAG). Release 워크플로가 게시 중이 아닌지 확인하고, GitHub 에서 지운 뒤 다시 실행하세요" ;;
    *) die "릴리스 $TAG 가 이미 있습니다 (Release 워크플로가 게시했다면 정상입니다. 이 스크립트는 CI 가 게시하지 못했을 때만 씁니다)" ;;
  esac
}
check_no_release

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

# 릴리스 전용 무시 테스트 하나를 이름 그대로(--exact) 태그 소스에서 실행하고, 실제로 1개가 통과했는지까지 확인한다
# (필터가 틀려 0개가 실행되고 "ok" 로 끝나는 것을 막는다)
run_release_test() {
  OUT="$WORK/test-$1.txt"
  echo "검사:   $1"
  if ! (cd "$SRC" && cargo test --locked -- --ignored --exact "$1") >"$OUT" 2>&1; then
    cat "$OUT" >&2
    die "릴리스 검사 $1 이 실패했습니다"
  fi
  grep -E '^test result: ok\. 1 passed;' "$OUT" || { cat "$OUT" >&2; die "릴리스 검사 $1 이 실행되지 않았습니다 (1 passed 아님)"; }
}

# ---- e. 서명할 키가 그 태그의 앱에 들어 있는지 (빌드 전 빠른 실패용. 주석 줄은 제외. 실제 확인은 i 에서 앱 코드로) ----
KEYS_BLOCK=$(awk '/^const UPDATE_PUBLIC_KEYS/ { p = 1 } p { print } p && /\];/ { exit }' "$SRC/src/main.rs" | grep -v '^[[:space:]]*//' || true)
if ! printf '%s\n' "$KEYS_BLOCK" | grep -Fq -- "\"$PUBKEY\""; then
  die "앱이 모르는 키로 서명하려 합니다: $PUB 의 공개키가 $TAG 의 UPDATE_PUBLIC_KEYS 에 없습니다"
fi

# ---- f. 서명할 커밋을 사람이 확인 (빌드 전) ----
# 서명되는 것은 이 컴퓨터의 로컬 태그가 가리키는 커밋이다. 원격에서 받아 온 커밋에 모르는 변경이 섞였는지 여기서 본다
PREV=$(git describe --tags --abbrev=0 "$TAG^" 2>/dev/null || true)
echo
if [ -n "$PREV" ]; then
  echo "직전 릴리스 태그 $PREV 부터 $TAG 까지의 커밋:"
  RANGE="$PREV..$TAG"
  BASE=$PREV
else
  echo "직전 태그가 없어 $TAG 까지의 전체 커밋:"
  RANGE=$TAG
  BASE=$(git hash-object -t tree /dev/null)
fi
git --no-pager log --format='%h %an <%ae> %ad %s' --date=short "$RANGE"
echo
echo "변경 파일:"
git --no-pager diff --stat "$BASE" "$TAG"
echo
[ -t 0 ] || die "표준입력이 터미널이 아니라 확인을 받을 수 없습니다"
printf '이 커밋들을 서명해 배포합니다. 계속하려면 태그 이름을 입력하세요: '
ANSWER=
IFS= read -r ANSWER || die "입력을 받지 못했습니다"
[ "$ANSWER" = "$TAG" ] || die "입력한 값이 태그 이름($TAG)과 달라 멈춥니다"

# ---- g. 빌드 ----
run_release_test security_tests::release_keys_are_configured
(
  cd "$SRC"
  UNIVERSAL=1 INSTALL=0 ./bundle.sh
)
NAME="URL-Downloader-$TAG-macos-universal.zip"
ZIP="$SRC/dist/$NAME"
[ -f "$ZIP" ] || die "빌드 결과 $NAME 이 없습니다"

# ---- h. 서명 ----
echo "파일:   $NAME"
echo "크기:   $(wc -c < "$ZIP" | tr -d ' ') 바이트"
echo "sha256: $(shasum -a 256 "$ZIP" | cut -d ' ' -f 1)"
# trusted comment 는 서명에 포함되어 위조할 수 없다. 앱은 이 값이 받은 파일명과 같은지 확인한다
minisign -S -s "$KEY" -t "file:$NAME" -m "$ZIP"
minisign -V -p "$PUB" -m "$ZIP"

# ---- i. 태그 소스의 앱 코드로 실제 검증 (UPDATE_PUBLIC_KEYS·trusted comment·파일명·태그 버전·크기 상한) ----
export URL_DOWNLOADER_VERIFY_ZIP="$ZIP"
run_release_test security_tests::release_artifact_verifies
unset URL_DOWNLOADER_VERIFY_ZIP

# ---- j. 초안으로 만들고 두 파일이 다 올라간 것을 확인한 뒤 공개 (zip 만 있고 서명이 없는 상태를 사용자가 보지 않게) ----
check_no_release
gh release create "$TAG" "$ZIP" "$ZIP.minisig" --repo "$REPO" --verify-tag --title "$TAG" --generate-notes --draft

ZIP_SIZE=$(wc -c < "$ZIP" | tr -d ' ')
SIG_SIZE=$(wc -c < "$ZIP.minisig" | tr -d ' ')
ASSETS=$(gh api "repos/$REPO/releases" --paginate \
  --jq ".[] | select(.tag_name == \"$TAG\" and .draft) | .assets[] | \"\\(.name) \\(.size) \\(.state)\"") \
  || die "초안 릴리스의 파일 목록을 확인하지 못했습니다. 초안을 확인하세요"
printf '%s\n' "$ASSETS" | grep -Fxq "$NAME $ZIP_SIZE uploaded" \
  || die "초안 릴리스에 $NAME 이 제대로 올라가지 않았습니다. 초안을 확인하세요: $ASSETS"
printf '%s\n' "$ASSETS" | grep -Fxq "$NAME.minisig $SIG_SIZE uploaded" \
  || die "초안 릴리스에 $NAME.minisig 가 제대로 올라가지 않았습니다. 초안을 확인하세요: $ASSETS"

# ---- k. 공개 ----
gh release edit "$TAG" --repo "$REPO" --draft=false
echo "공개함: $TAG ($NAME, $NAME.minisig)"
