#!/bin/sh
# Fetch the C reference sources the oracle compiles: libFLAC 1.4.3 and libogg 1.3.5,
# shallow-cloned from the Xiph release tags into oracle/vendor/ (gitignored). The
# build script reads them from there, or from LIBFLAC_SRC / LIBOGG_SRC if set.
set -eu

dir=$(cd "$(dirname "$0")" && pwd)/vendor
mkdir -p "$dir"

fetch() { # <name> <tag> <url>
    if [ -d "$dir/$1/.git" ]; then
        echo "$1: already present at $dir/$1"
        return
    fi
    git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$2" "$3" "$dir/$1"
    echo "$1: $2 -> $dir/$1"
}

fetch flac 1.4.3 https://github.com/xiph/flac.git
fetch ogg v1.3.5 https://github.com/xiph/ogg.git
