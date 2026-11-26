#!/bin/sh
set -eu

base_sha=$1
if [ "$base_sha" = 0000000000000000000000000000000000000000 ]; then
    if [ "$GITHUB_REF" != refs/heads/main ] && git rev-parse --verify -q refs/remotes/origin/main >/dev/null; then
        base_sha=refs/remotes/origin/main
    else
        base_sha=-
    fi
fi

python3 -B scripts/data_guard.py range "$base_sha" HEAD
sh scripts/ci-check.sh data-guard-tests python3 -B -m unittest discover -s tests -p test_data_guard.py -v
sh scripts/ci-check.sh http-fixture-isolation python3 -B -m unittest discover -s tests/api -p test_http_isolation.py -v
