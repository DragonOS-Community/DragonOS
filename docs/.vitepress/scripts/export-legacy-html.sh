#!/usr/bin/env bash
# Build the last Sphinx multiversion site from a clean git commit that still
# has conf.py, then pack the 14 frozen V* directories.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
OUT="$ROOT/docs/.vitepress/archives"
WORKTREE="${TMPDIR:-/tmp}/dragonos-docs-legacy-export"

mkdir -p "$OUT"

if [[ -f "$ROOT/docs/conf.py" ]]; then
  SRC_COMMIT="HEAD"
else
  SRC_COMMIT="$(git -C "$ROOT" log -1 --format=%H -- docs/conf.py || true)"
fi

if [[ -z "${SRC_COMMIT}" ]]; then
  echo "Cannot find a commit that still contains docs/conf.py" >&2
  exit 1
fi

rm -rf "$WORKTREE"
git -C "$ROOT" worktree add --detach "$WORKTREE" "$SRC_COMMIT"
cleanup() {
  git -C "$ROOT" worktree remove --force "$WORKTREE" || true
}
trap cleanup EXIT

python3 -m pip install -r "$WORKTREE/docs/requirements.txt"
(
  cd "$WORKTREE/docs"
  CURRENT_GIT_COMMIT_DIRTY=0 sphinx-multiversion -D language=zh_CN . "$OUT/_sphinx_build/html"
)

PACK="$OUT/legacy-html"
rm -rf "$PACK"
mkdir -p "$PACK"
for tag in V0.4.0 V0.3.0 V0.2.0 V0.1.10 V0.1.9 V0.1.8 V0.1.7 V0.1.6 V0.1.5 V0.1.4 V0.1.3 V0.1.2 V0.1.1 V0.1.0; do
  if [[ -d "$OUT/_sphinx_build/html/$tag" ]]; then
    cp -a "$OUT/_sphinx_build/html/$tag" "$PACK/$tag"
  else
    echo "warning: missing $tag in sphinx-multiversion output" >&2
  fi
done

tar -czf "$OUT/legacy-sphinx-html.tar.gz" -C "$PACK" .
echo "Wrote $OUT/legacy-sphinx-html.tar.gz"
echo "Upload this tarball as a GitHub Release asset or keep it in docs/.vitepress/archives/ (gitignored)."
echo "Then set DOCS_LEGACY_ARCHIVE or secrets.DOCS_LEGACY_ARCHIVE_URL for CI assemble."
