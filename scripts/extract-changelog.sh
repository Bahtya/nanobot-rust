#!/usr/bin/env bash
# Extracts the changelog section for a given version from CHANGELOG.md.
# Usage: extract-changelog.sh <version>
# Example: extract-changelog.sh 0.1.0
set -euo pipefail

VERSION="${1:?Usage: $0 <version>}"

if [ ! -f CHANGELOG.md ]; then
  echo "Error: CHANGELOG.md not found" >&2
  exit 1
fi

# Match heading like: ## [0.1.0] or ## [0.1.0] - 2025-04-20
awk -v ver="$VERSION" '
  tolower($0) ~ "^## \\[" ver "\\]" {
    found = 1
    next
  }
  found && tolower($0) ~ "^## \\[" {
    exit
  }
  found && /^\[.*\]:/ {
    next
  }
  found {
    print
  }
' CHANGELOG.md
