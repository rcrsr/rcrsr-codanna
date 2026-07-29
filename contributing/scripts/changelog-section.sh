#!/usr/bin/env bash
# Print the body of CHANGELOG.md's "## [<version>]" section — everything
# between that heading and the next "^## " heading (both exclusive).
#
# Matching is an EXACT string compare of the bracket contents, never a
# regex: a version like "0.12.0+rcrsr.1" contains '.' and '+', both regex
# metacharacters, and a loose match could select "## [0.12.0]" (which
# already exists in this file) or "## [Unreleased]" instead of the
# intended section.
#
# Usage: changelog-section.sh <version>
set -euo pipefail

version="${1:-}"
if [[ -z "$version" ]]; then
  echo "Error: usage: changelog-section.sh <version>" >&2
  exit 1
fi

changelog="CHANGELOG.md"
if [[ ! -f "$changelog" ]]; then
  echo "Error: $changelog not found." >&2
  echo "  expected to run from the repository root." >&2
  exit 1
fi

if ! body=$(awk -v want="[${version}]" '
  /^## / {
    # Exact string compare of the bracket contents only (from the first
    # "[" through the matching "]"), never a substring/regex match against
    # the whole heading line, which also carries a trailing " - <date>".
    line = $0
    sub(/^## /, "", line)
    start = index(line, "[")
    stop = index(line, "]")
    bracket = (start > 0 && stop > start) ? substr(line, start, stop - start + 1) : ""

    if (in_section) exit
    if (bracket == want) {
      in_section = 1
      found = 1
      next
    }
    next
  }
  in_section { print }
  END { exit(found ? 0 : 1) }
' "$changelog"); then
  echo "Error: CHANGELOG.md has no section for version '${version}'." >&2
  echo "  expected a heading exactly matching: ## [${version}]" >&2
  echo "  Rename '## [Unreleased]' to '## [${version}] - <date>' before tagging." >&2
  exit 1
fi

printf '%s\n' "$body"
