#!/usr/bin/env bash
# fetch-mingw64-sysroot.sh
#
# Resolves poppler's full mingw64 dependency tree automatically from
# MSYS2's package database, downloads every needed .pkg.tar.zst, and
# extracts them all into a sysroot directory. Rerun any time poppler
# (or the mirror) updates -- this replaces manually browsing MSYS2
# package pages and hand-maintaining a dependency list.
#
# Usage:
#   ./fetch-mingw64-sysroot.sh
#   SENPAI_MINGW_SYSROOT=/custom/path ./fetch-mingw64-sysroot.sh
#
# Requires: bash, curl, tar, zstd (apt install zstd)

set -euo pipefail

# Resolve the script's own directory to an ABSOLUTE path immediately,
# before anything below this point does `cd` and silently breaks any
# relative path built from it.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSROOT="${SENPAI_MINGW_SYSROOT:-$SCRIPT_DIR/mingw64-sysroot}"

MIRROR="https://mirror.msys2.org/mingw/mingw64"
WORKDIR="$(mktemp -d)"
ROOT_PACKAGE="mingw-w64-x86_64-poppler"

mkdir -p "$SYSROOT"
cd "$WORKDIR"

echo "Fetching package database..."
curl -sSL --fail --retry 3 --retry-delay 2 -o mingw64.db "$MIRROR/mingw64.db"

echo "Extracting database..."
mkdir -p db
tar -xf mingw64.db -C db

# Build a name -> (filename, depends[]) map from the desc files.
declare -A PKG_FILENAME
declare -A PKG_DEPENDS

for descdir in db/*/; do
    desc="$descdir/desc"
    [ -f "$desc" ] || continue
    name=$(awk '/%NAME%/{getline; print; exit}' "$desc")
    filename=$(awk '/%FILENAME%/{getline; print; exit}' "$desc")
    depends=$(awk '/%DEPENDS%/{flag=1; next} /^%/{flag=0} flag' "$desc" \
        | sed -E 's/[<>=].*$//') # strip version constraints like glib2>=2.70
    PKG_FILENAME["$name"]="$filename"
    PKG_DEPENDS["$name"]="$depends"
done

echo "Resolving dependency closure for $ROOT_PACKAGE..."
declare -A NEEDED
QUEUE=("$ROOT_PACKAGE")

while [ ${#QUEUE[@]} -gt 0 ]; do
    pkg="${QUEUE[0]}"
    QUEUE=("${QUEUE[@]:1}")

    [ -n "${NEEDED[$pkg]+x}" ] && continue          # already handled
    [ -z "${PKG_FILENAME[$pkg]+x}" ] && continue    # not a package in this repo -- skip

    NEEDED["$pkg"]=1

    for dep in ${PKG_DEPENDS[$pkg]:-}; do
        QUEUE+=("$dep")
    done
done

echo "Resolved ${#NEEDED[@]} packages. Downloading..."
mkdir -p "$SYSROOT/downloads"
for pkg in "${!NEEDED[@]}"; do
    filename="${PKG_FILENAME[$pkg]}"
    dest="$SYSROOT/downloads/$filename"
    if [ -f "$dest" ]; then
        continue
    fi
    echo "  -> $filename"

    attempt=0
    until curl -sSL --fail --retry 3 --retry-delay 2 -C - "$MIRROR/$filename" -o "$dest"; do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 5 ]; then
            echo "FAILED after $attempt attempts: $filename" >&2
            exit 1
        fi
        echo "  retrying ($attempt)..." >&2
        sleep 3
    done
done

echo "Extracting into $SYSROOT..."
for filename in "$SYSROOT"/downloads/*.pkg.tar.zst; do
    tar --use-compress-program=unzstd -xf "$filename" -C "$SYSROOT"
done

rm -rf "$WORKDIR"
echo "Done. Sysroot ready at: $SYSROOT"
echo "Poppler headers: $(find "$SYSROOT" -iname 'poppler-document.h' -path '*cpp*' | head -1)"
