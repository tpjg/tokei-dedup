#!/usr/bin/env bash
# Fetch known-dirty corpora for end-to-end duplicate-detection runs.
#
# Hand-curated test corpora (small, clean) live in tests/fixtures/. These corpora are
# real-world projects with known organic-growth duplication — useful from milestone 1
# onward, when we have a detector to point at them.
#
# All clones use --depth 1 to keep checkouts small. Total disk: ~1 GB.
#
# Usage: scripts/fetch-corpora.sh [corpus...]
#   Without args: fetches everything below.
#   With args: fetches only the named corpora.
#
# Corpora rationale:
#   - the-algorithms-python/java/javascript: same algorithm implemented in 3+ languages.
#     Cross-language clone detection has guaranteed positives here.
#   - salt: ~700 KLOC Python, known organic duplication across modules.
#   - hadoop: ~3 MLOC Java, cross-module duplication is well-documented.
#   - wordpress: ~600 KLOC PHP, infamous for copy-paste growth.
#   - bigclonebench: academic gold standard. Skipped by default — large download.

set -euo pipefail

CORPORA_DIR="$(dirname "$0")/../tests/corpora"
mkdir -p "$CORPORA_DIR"
cd "$CORPORA_DIR"

declare -A CORPORA=(
    [the-algorithms-python]="https://github.com/TheAlgorithms/Python.git"
    [the-algorithms-java]="https://github.com/TheAlgorithms/Java.git"
    [the-algorithms-javascript]="https://github.com/TheAlgorithms/JavaScript.git"
    [salt]="https://github.com/saltstack/salt.git"
    [hadoop]="https://github.com/apache/hadoop.git"
    [wordpress]="https://github.com/WordPress/WordPress.git"
)

if [ "$#" -gt 0 ]; then
    REQUESTED=("$@")
else
    REQUESTED=("${!CORPORA[@]}")
fi

for name in "${REQUESTED[@]}"; do
    url="${CORPORA[$name]:-}"
    if [ -z "$url" ]; then
        echo "unknown corpus: $name" >&2
        exit 1
    fi
    if [ -d "$name/.git" ]; then
        echo "[skip] $name already cloned"
        continue
    fi
    echo "[fetch] $name <- $url"
    git clone --depth 1 --single-branch "$url" "$name"
done

echo
echo "Corpora at: $CORPORA_DIR"
du -sh */ 2>/dev/null | sort -h
