#!/bin/sh
set -eu

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
command -v python3 >/dev/null
if git -C "$root" config --get core.hooksPath >/dev/null; then
    echo 'data-guard: hook installation requires the default hook directory' >&2
    exit 1
fi
hooks=$(git -C "$root" rev-parse --path-format=absolute --git-path hooks)
mkdir -p "$hooks"

# Check both destinations before making any changes.
for name in pre-commit pre-push; do
    destination=$hooks/$name
    if [ -L "$destination" ] || { [ -e "$destination" ] &&
            ! { [ -f "$destination" ] && [ -x "$destination" ] &&
                cmp -s "$root/.githooks/$name" "$destination"; }; }; then
        echo "data-guard: existing $name hook requires manual integration" >&2
        exit 1
    fi
done

for name in pre-commit pre-push; do
    destination=$hooks/$name
    if [ -e "$destination" ]; then
        continue
    fi
    tmp=$(mktemp "$hooks/data-guard.XXXXXX")
    trap 'rm -f "$tmp"' EXIT
    cp "$root/.githooks/$name" "$tmp"
    chmod 755 "$tmp"
    ln "$tmp" "$destination"
    rm -f "$tmp"
    trap - EXIT
done
echo 'data-guard: pre-commit and pre-push installed'
