#!/bin/sh
# SPDX-License-Identifier: MIT

set -eu

repo_dir=$(
    unset CDPATH
    cd -- "$(dirname -- "$0")/.."
    pwd
)
cd "$repo_dir"

cargo build --locked --release --example parts_parser_benchmark

if [ "$#" -gt 0 ] && [ "$1" != "build" ]; then
    exec target/release/examples/parts_parser_benchmark "$@"
fi
