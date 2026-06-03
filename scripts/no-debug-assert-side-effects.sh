#!/usr/bin/env sh
set -eu

# debug_assert! is compiled out of release builds. Keep it free of obvious
# state-changing calls so release behavior cannot diverge from debug tests.
pattern='debug_assert!\s*\([^;\n]*\.(append|clear|drain|extend|insert|pop|push|remove|replace|retain|set_\w*|swap|take|truncate)\s*\('

if ! command -v rg >/dev/null 2>&1; then
  echo "ripgrep (rg) is required for debug_assert! side-effect checks." >&2
  exit 127
fi

set +e
rg -n --pcre2 "$pattern" src crates \
  --glob '*.rs' \
  --glob '!**/target/**' \
  --glob '!**/tests/**' \
  --glob '!**/*tests*.rs'
status=$?
set -e

case "$status" in
  0)
    echo "debug_assert! must not contain state-changing calls; release builds remove them." >&2
    exit 1
    ;;
  1)
    ;;
  *)
    echo "debug_assert! side-effect scan failed while running rg (exit $status)." >&2
    exit "$status"
    ;;
esac

echo "debug_assert! side-effect check passed."
