#!/usr/bin/env bash
# Refresh generated badge files on main. Commit only those files.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
cd "$root"
mkdir -p .jankurai
jankurai audit . --full --mode standard --no-score-history \
  --json .jankurai/repo-score.json \
  --md .jankurai/repo-score.md
jankurai badge --update-readme
git add agent/jankurai-badge.svg agent/jankurai-badge.json README.md
if git diff --cached --quiet; then
  printf 'badge already current\n'
  exit 0
fi
git config user.name "jankurai-badge"
git config user.email "jankurai-badge@users.noreply.github.com"
git commit -m "$(cat <<'EOF'
jankurai-badge

[skip ci]
EOF
)"
git push
