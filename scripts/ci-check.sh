#!/bin/sh
set -eu

check=$1
shift
if "$@" >/dev/null 2>&1; then
    printf '%s: passed\n' "$check"
else
    status=$?
    printf '%s: failed (exit %s)\n' "$check" "$status" >&2
    exit "$status"
fi
