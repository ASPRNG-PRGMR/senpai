#!/usr/bin/env bash
#
# Packages the Windows build into a single zip for the Releases page.
#
# The DLL set is *derived*, not hand-maintained: we read the PE import
# table of senpai.exe with objdump and walk it transitively against the
# mingw64 sysroot, the same way fetch-mingw64-sysroot.sh derives the
# package set from MSYS2's own database instead of browsing package pages.
# A hand-written list is exactly what the roadmap flags as unverified --
# the previous one came from deleting DLLs one at a time against a single
# test document, which under-reports anything that document happened not
# to exercise (non-embedded fonts, uncommon image codecs).
#
# Anything the import walk resolves to a system DLL (kernel32, user32,
# d3d12, vulkan-1, ...) is deliberately left out: those ship with Windows
# or with the GPU driver.
#
# Dynamic linking, not static -- also the standard LGPL-compliant
# distribution pattern for poppler and its LGPL dependencies.
#
# usage: ./package-windows.sh [--debug]

set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(pwd)"
SYSROOT="${SENPAI_MINGW_SYSROOT:-$ROOT/mingw64-sysroot}"
DLL_DIR="$SYSROOT/mingw64/bin"
TARGET="x86_64-pc-windows-gnu"
PROFILE="release"
CARGO_FLAGS=(--release)

if [[ "${1:-}" == "--debug" ]]; then
    PROFILE="debug"
    CARGO_FLAGS=()
fi

OBJDUMP="${OBJDUMP:-x86_64-w64-mingw32-objdump}"
command -v "$OBJDUMP" >/dev/null || {
    echo "error: $OBJDUMP not found (install mingw64 binutils)" >&2
    exit 1
}
command -v zip >/dev/null || { echo "error: zip not found" >&2; exit 1; }
[[ -d "$DLL_DIR" ]] || {
    echo "error: no mingw64 sysroot at $DLL_DIR -- run ./fetch-mingw64-sysroot.sh first" >&2
    exit 1
}

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
STAGE="$ROOT/target/package/senpai-$VERSION-windows-x64"
ZIP="$ROOT/target/package/senpai-$VERSION-windows-x64.zip"

echo "==> building senpai-ui ($PROFILE) for $TARGET"
SENPAI_MINGW_SYSROOT="$SYSROOT" cargo build -p senpai-ui --target "$TARGET" "${CARGO_FLAGS[@]}"

EXE="$ROOT/target/$TARGET/$PROFILE/senpai-ui.exe"
[[ -f "$EXE" ]] || { echo "error: $EXE not produced" >&2; exit 1; }

rm -rf "$STAGE" "$ZIP"
mkdir -p "$STAGE"
# Shipped as senpai.exe: that's the name installer.nsi and the Start-menu
# shortcut already expect.
cp "$EXE" "$STAGE/senpai.exe"
[[ -f apps/senpai-ui/senpai.ico ]] && cp apps/senpai-ui/senpai.ico "$STAGE/senpai.ico"

# --- transitive PE import walk -------------------------------------------
# Imports are reported by objdump as "DLL Name: foo.dll". We only chase
# names that actually exist in the sysroot; everything else is a Windows
# or driver-provided DLL and must not be bundled.
imports() {
    "$OBJDUMP" -p "$1" 2>/dev/null \
        | sed -n 's/^\s*DLL Name:\s*\(.*\)$/\1/p' \
        | tr -d '\r'
}

declare -A seen=()
queue=("$STAGE/senpai.exe")

while ((${#queue[@]})); do
    current="${queue[0]}"
    queue=("${queue[@]:1}")
    while read -r dll; do
        [[ -n "$dll" ]] || continue
        key="${dll,,}"                       # DLL names are case-insensitive
        [[ -n "${seen[$key]:-}" ]] && continue
        candidate="$DLL_DIR/$dll"
        if [[ -f "$candidate" ]]; then
            seen[$key]="$candidate"
            queue+=("$candidate")
        else
            seen[$key]=""                    # system/driver DLL: skip, don't re-check
        fi
    done < <(imports "$current")
done

count=0
for key in "${!seen[@]}"; do
    src="${seen[$key]}"
    [[ -n "$src" ]] || continue
    cp "$src" "$STAGE/"
    count=$((count + 1))
done

cat > "$STAGE/README.txt" <<'TXT'
SENPAI -- a fast, minimal PDF reader.

Run senpai.exe. Every DLL in this folder is required; keep them beside
the executable. Open a document with Files -> Open (or Ctrl+O), or pass a
path on the command line.

Poppler and several of its dependencies are LGPL; they are linked
dynamically and shipped as separate DLLs here so they remain replaceable,
which is what the LGPL requires.
TXT

echo "==> bundled $count DLL(s) from $DLL_DIR"
(cd "$(dirname "$STAGE")" && zip -q -r "$ZIP" "$(basename "$STAGE")")

echo "==> $ZIP"
du -h "$ZIP" | cut -f1 | sed 's/^/    size: /'
echo "    contents:"
(cd "$STAGE" && ls) | sed 's/^/      /'
