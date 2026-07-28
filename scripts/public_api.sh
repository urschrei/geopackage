#!/usr/bin/env bash
#
# Regenerate the recorded public API surface of each crate, or check it for
# drift.
#
#     scripts/public_api.sh          # rewrite public-api/*.txt
#     scripts/public_api.sh --check  # fail if the recorded surface is stale
#
# The recorded files are the freeze's evidence. Any change to what a crate
# exports shows up as a diff in them, so an accidental addition or removal is
# visible in review rather than only to whoever it breaks later. Until 1.0 that
# is visibility rather than enforcement: a diff is a prompt to check the change
# was meant, not a refusal.
#
# Needs `cargo install cargo-public-api`. It drives rustdoc's JSON output, which
# is why it is a separate script and a separate CI job rather than a test: it
# rebuilds documentation for every crate and is far too slow to sit in the
# ordinary test run.

set -euo pipefail

cd "$(dirname "$0")/.."

crates=(geopackage-core geopackage geopackage-ffi)
check=false
if [ "${1:-}" = "--check" ]; then
    check=true
fi

status=0
for crate in "${crates[@]}"; do
    recorded="public-api/${crate}.txt"
    generated=$(mktemp)
    trap 'rm -f "$generated"' EXIT

    # `--simplified` drops blanket and auto-trait implementations, which are
    # noise here: they change with the compiler rather than with this code.
    cargo public-api --simplified --all-features -p "$crate" > "$generated" 2>/dev/null

    if [ "$check" = true ]; then
        if ! diff -u "$recorded" "$generated" > /dev/null 2>&1; then
            echo "public API drift in ${crate}:"
            diff -u "$recorded" "$generated" || true
            echo
            status=1
        else
            echo "${crate}: unchanged ($(wc -l < "$recorded" | tr -d ' ') items)"
        fi
    else
        cp "$generated" "$recorded"
        echo "${crate}: recorded $(wc -l < "$recorded" | tr -d ' ') items"
    fi
done

if [ "$check" = true ] && [ "$status" -ne 0 ]; then
    echo "The recorded public API is out of date. If the change was intended,"
    echo "regenerate with scripts/public_api.sh and commit the result."
fi
exit "$status"
