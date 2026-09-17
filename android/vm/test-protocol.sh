#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
test_binary=$(mktemp)
trap 'rm -f "$test_binary"' EXIT
"${CC:-cc}" -D_DEFAULT_SOURCE -std=c11 -Wall -Wextra -Werror -O2 \
    android/vm/test_protocol.c -o "$test_binary"
"$test_binary"
