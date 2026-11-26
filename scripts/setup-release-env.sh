#!/bin/sh
# Release 워크플로의 sign 작업이 쓰는 GitHub 환경 `release` 와 그 시크릿을 만든다 (리포 주인이 한 번 실행).
#   scripts/setup-release-env.sh [비밀키 파일]
#   비밀키 파일 기본값: URL_DOWNLOADER_SIGN_KEY 또는 $HOME/.config/url-downloader/release-main.key
# 하는 일 (각 단계 전에 무엇을 바꾸는지 보여 주고 확인을 받는다):
#   1. 환경 `release` 생성·갱신: 배포 가능한 ref 를 이름 패턴으로 제한 (deployment_branch_policy.custom_branch_policies)
#   2. 그 환경의 배포 정책에 태그 패턴 `v*` 추가 (이미 있으면 건너뜀). 브랜치에서는 서명 작업이 돌지 않는다
#   3. 환경 시크릿 MINISIGN_KEY = 비밀키 파일 내용 전체 (파일에서 바로 넘긴다)
#   4. 환경 시크릿 MINISIGN_PASSWORD = 비밀키 비밀번호 (gh 가 터미널에서 직접 묻는다. 이 스크립트는 읽지 않는다)
# 예비 키는 같은 환경에 MINISIGN_SPARE_KEY / MINISIGN_SPARE_PASSWORD 로 따로 둔다 (워크플로는 쓰지 않음). 필요하면 환경 설정 화면에서 승인자(required reviewers)를
# 추가해 서명 전에 사람이 승인하게 할 수 있다.
# 주의: 승인자를 추가한 뒤 이 스크립트를 다시 실행하면 환경 설정을 다시 쓰면서 승인자가 지워질 수 있다. 다시 실행했다면 환경 설정 화면을 확인한다.
set -eu

REPO=bluelang1/url-downloader
ENV_NAME=release
KEY="${1:-${URL_DOWNLOADER_SIGN_KEY:-$HOME/.config/url-downloader/release-main.key}}"

die() {
  echo "중단: $*" >&2
  exit 1
}

confirm() {
  printf '%s 계속할까요? [y/N] ' "$1"
  ANSWER=
  IFS= read -r ANSWER || die "입력을 받지 못했습니다"
  [ "$ANSWER" = "y" ] || die "취소했습니다"
}

[ "$#" -le 1 ] || { echo "사용법: $0 [비밀키 파일]" >&2; exit 2; }
# 비밀번호 입력과 확인이 필요하므로 터미널에서만 실행한다
[ -t 0 ] || die "표준입력이 터미널이 아닙니다. 터미널에서 직접 실행하세요"
command -v gh >/dev/null 2>&1 || die "gh 명령이 필요합니다"
gh auth status >/dev/null 2>&1 || die "gh 로그인이 필요합니다 (gh auth login)"

[ -f "$KEY" ] || die "비밀키 파일이 없습니다: $KEY"
# 비밀번호로 암호화된 minisign 비밀키만 올린다 (sign 작업은 비밀번호를 표준입력으로 넘긴다)
FIRST=$(head -n 1 "$KEY")
[ "$FIRST" = "untrusted comment: minisign encrypted secret key" ] \
  || die "비밀번호로 암호화된 minisign 비밀키가 아닙니다 (첫 줄: $FIRST): $KEY"
# 비밀번호 없이 만든(-W) 키도 첫 줄은 같다. 둘째 줄의 KDF 표시(3~4번째 바이트)가 Sc(scrypt)여야 비밀번호로 잠긴 키다
KDF=$(sed -n 2p "$KEY" | base64 -d 2>/dev/null | head -c 4 | cut -c3-4)
[ "$KDF" = "Sc" ] || die "비밀번호 없이 만든 키입니다. 비밀번호를 설정해 다시 만드세요: $KEY"

echo "리포:   $REPO"
echo "환경:   $ENV_NAME"
echo "비밀키: $KEY"
echo

# ---- 1. 환경 생성·갱신 (배포 ref 를 이름 패턴으로 제한) ----
confirm "환경 $ENV_NAME 을 만들거나 갱신하고, 배포 가능한 ref 를 이름 패턴으로 제한합니다."
gh api -X PUT "repos/$REPO/environments/$ENV_NAME" \
  -F 'deployment_branch_policy[protected_branches]=false' \
  -F 'deployment_branch_policy[custom_branch_policies]=true' >/dev/null
echo "환경 $ENV_NAME 준비됨"

# ---- 2. 배포 정책: 태그 v* ----
EXISTING=$(gh api "repos/$REPO/environments/$ENV_NAME/deployment-branch-policies" \
  --jq '.branch_policies[] | "\(.type) \(.name)"')
echo "현재 배포 정책:"
printf '%s\n' "${EXISTING:-  (없음)}"
if printf '%s\n' "$EXISTING" | grep -Fxq 'tag v*'; then
  echo "태그 패턴 v* 가 이미 있습니다"
else
  confirm "환경 $ENV_NAME 의 배포 정책에 태그 패턴 v* 를 추가합니다."
  gh api -X POST "repos/$REPO/environments/$ENV_NAME/deployment-branch-policies" \
    -f name='v*' -f type=tag >/dev/null
  echo "태그 패턴 v* 추가됨"
fi
if printf '%s\n' "$EXISTING" | grep -v '^tag v\*$' | grep -q .; then
  echo "주의: v* 태그 외의 배포 정책이 있습니다. 필요 없으면 GitHub 환경 설정에서 지우세요" >&2
fi

# ---- 3·4. 시크릿 ----
confirm "환경 $ENV_NAME 의 시크릿 MINISIGN_KEY 를 $KEY 의 내용으로 설정합니다."
gh secret set MINISIGN_KEY --repo "$REPO" --env "$ENV_NAME" < "$KEY"
echo "MINISIGN_PASSWORD 를 설정합니다. 비밀키 비밀번호를 입력하세요 (화면에 보이지 않습니다)."
gh secret set MINISIGN_PASSWORD --repo "$REPO" --env "$ENV_NAME"

echo
echo "완료. 확인:"
echo "  gh api repos/$REPO/environments/$ENV_NAME/deployment-branch-policies"
echo "  gh secret list --repo $REPO --env $ENV_NAME"
