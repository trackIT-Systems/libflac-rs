#!/bin/sh
# Fetch the C reference sources the oracle compiles: libFLAC 1.5.0 and libogg 1.3.5,
# shallow-cloned from the Xiph release tags into oracle/vendor/ (gitignored). The
# build script reads them from there, or from LIBFLAC_SRC / LIBOGG_SRC if set.
#
# Each checkout must be the commit the tag pointed to when the oracle was set up,
# so a moved or tampered tag is refused rather than silently compiled.
set -eu

dir=$(cd "$(dirname "$0")" && pwd)/vendor
mkdir -p "$dir"

fetch() { # <name> <tag> <commit> <url>
    if [ ! -d "$dir/$1/.git" ]; then
        git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$2" "$4" "$dir/$1"
    fi
    actual=$(git -C "$dir/$1" rev-parse HEAD)
    if [ "$actual" != "$3" ]; then
        echo "$1: $dir/$1 is at $actual, expected $3 ($2)" >&2
        exit 1
    fi
    echo "$1: $2 ($3) at $dir/$1"
}

fetch flac 1.5.0 1507800de4b70e21be71f38caa0d9079d0bc6e45 https://github.com/xiph/flac.git
fetch ogg v1.3.5 e1774cd77f471443541596e09078e78fdc342e4f https://github.com/xiph/ogg.git
