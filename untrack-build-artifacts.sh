#!/usr/bin/env bash
#
# Stops tracking the Windows build artifacts -- senpai-release-staging/
# (47 files: DLLs copied verbatim from the mingw64 sysroot, plus a built
# senpai.exe) and SenpaiSetup.exe (makensis output). Together ~57MB.
#
# Both are generated: package-windows.sh rebuilds the staging directory
# with a DLL set derived from the exe's PE import table, and
# `makensis installer.nsi` rebuilds the installer from it. Neither is a
# source input. Git stores binaries as whole blobs rather than deltas, so
# each rebuild that gets committed adds its full size permanently.
#
# Two modes:
#
#   --untrack  (default)  Untracks the files in the working tree, so the
#                         NEXT commit removes them. History keeps the
#                         blobs. Always safe; use this if the commit that
#                         added them has already been pushed.
#
#   --amend               Rewrites the HEAD commit so it never contained
#                         them at all, keeping the blobs out of the remote
#                         entirely. Refuses to run if HEAD is already
#                         reachable from a remote ref, since rewriting
#                         published history would then require a
#                         force-push. Tags the original commit first.
#
# Neither mode deletes anything from disk unless you pass --delete-local.
#
# usage: ./untrack-build-artifacts.sh [--untrack|--amend] [--delete-local] [--dry-run]

set -euo pipefail
cd "$(dirname "$0")"

PATHS=(senpai-release-staging SenpaiSetup.exe)
IGNORE_LINES=("/senpai-release-staging/" "/SenpaiSetup.exe")

MODE="untrack"
DELETE_LOCAL=0
DRY_RUN=0

while (($#)); do
    case "$1" in
        --untrack) MODE="untrack" ;;
        --amend) MODE="amend" ;;
        --delete-local) DELETE_LOCAL=1 ;;
        --dry-run | -n) DRY_RUN=1 ;;
        -h | --help)
            sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; $d'
            exit 0
            ;;
        *)
            echo "unknown option: $1 (try --help)" >&2
            exit 1
            ;;
    esac
    shift
done

run() {
    if ((DRY_RUN)); then
        printf '  would run: %s\n' "$*"
    else
        "$@"
    fi
}

git rev-parse --git-dir >/dev/null 2>&1 || {
    echo "error: not a git repository" >&2
    exit 1
}

# --- what is actually still tracked ---------------------------------------
tracked=()
for p in "${PATHS[@]}"; do
    if [[ -n "$(git ls-files -- "$p")" ]]; then
        tracked+=("$p")
    else
        echo "note: $p is already untracked, skipping"
    fi
done

if ((${#tracked[@]} == 0)); then
    echo "nothing tracked to untrack."
else
    bytes=$(git ls-files -z -- "${tracked[@]}" | xargs -0 --no-run-if-empty du -cb 2>/dev/null | tail -1 | cut -f1)
    files=$(git ls-files -- "${tracked[@]}" | wc -l)
    printf '==> untracking %s file(s), %s MiB: %s\n' \
        "$files" "$((${bytes:-0} / 1048576))" "${tracked[*]}"
fi

# --- mode guard -----------------------------------------------------------
if [[ "$MODE" == "amend" ]]; then
    head_sha="$(git rev-parse HEAD)"

    # Is HEAD reachable from any remote-tracking ref? If so it has been
    # published and rewriting it would need a force-push -- refuse, and
    # point at the mode that doesn't.
    published=""
    while read -r ref; do
        [[ -n "$ref" ]] || continue
        if git merge-base --is-ancestor "$head_sha" "$ref" 2>/dev/null; then
            published="$ref"
            break
        fi
    done < <(git for-each-ref --format='%(refname)' refs/remotes/)

    if [[ -n "$published" ]]; then
        cat >&2 <<MSG
error: HEAD ($(git rev-parse --short HEAD)) is already reachable from $published.

Rewriting it would require a force-push over published history. Use the
default mode instead, which removes the files in a new commit:

    ./untrack-build-artifacts.sh --untrack
MSG
        exit 1
    fi

    if [[ -n "$(git diff --cached --name-only)" ]]; then
        echo "error: you have staged changes; --amend would fold them into the rewritten commit." >&2
        echo "       unstage them first (git restore --staged .) or use --untrack." >&2
        exit 1
    fi

    echo "==> HEAD is unpushed, so it can be rewritten without a force-push"
fi

# --- untrack --------------------------------------------------------------
if ((${#tracked[@]} > 0)); then
    run git rm -r --cached --quiet -- "${tracked[@]}"
fi

# --- .gitignore (idempotent) ---------------------------------------------
for line in "${IGNORE_LINES[@]}"; do
    if grep -qxF "$line" .gitignore 2>/dev/null; then
        echo "note: .gitignore already has $line"
    else
        echo "==> adding $line to .gitignore"
        if ((DRY_RUN)); then
            printf '  would append: %s\n' "$line"
        else
            printf '%s\n' "$line" >>.gitignore
        fi
    fi
done
# In --amend mode .gitignore is deliberately left UNstaged: the rewritten
# commit should only lose the binaries, while the ignore rules belong to
# the commit you are about to make. Staging it here would also drag any
# other pending .gitignore edits backwards into the old commit.
if ((DRY_RUN == 0)) && [[ "$MODE" != "amend" ]] &&
    [[ -n "$(git ls-files --modified -- .gitignore)" ]]; then
    git add .gitignore
fi

# --- rewrite HEAD, if asked ----------------------------------------------
if [[ "$MODE" == "amend" ]] && [[ -z "$(git diff --cached --name-only)" ]]; then
    echo "==> nothing staged, so there is nothing to amend out of HEAD"
    MODE="noop"
fi

if [[ "$MODE" == "amend" ]]; then
    backup="refs/tags/pre-untrack-$(date +%Y%m%d-%H%M%S)"
    echo "==> tagging the original commit as ${backup#refs/tags/}"
    run git update-ref "$backup" HEAD

    # Only the removals (and .gitignore) are staged, so the amend drops
    # those paths from the commit and leaves every other working-tree
    # change exactly as unstaged as it was.
    echo "==> amending HEAD to drop the artifacts"
    run git commit --amend --no-edit --quiet
fi

# --- optionally clear them off disk too ----------------------------------
if ((DELETE_LOCAL)); then
    # senpai-release-staging is genuinely dead now: installer.nsi reads
    # package-windows.sh's output instead, and its senpai.exe predates
    # the UTF-8 crash fix, so leaving it around is a stale-binary trap.
    echo "==> removing local copies (regenerate with ./package-windows.sh && makensis installer.nsi)"
    for p in "${PATHS[@]}"; do
        [[ -e "$p" ]] && run rm -rf -- "$p"
    done
fi

# --- report ---------------------------------------------------------------
echo
if ((DRY_RUN)); then
    echo "dry run only; nothing changed."
    exit 0
fi

echo "done. staged:"
git diff --cached --stat | tail -5

if [[ "$MODE" == "amend" ]]; then
    cat <<MSG

HEAD rewritten: $(git log --oneline -1 | cat)
The blobs are now unreachable except via the backup tag. Once you are
satisfied, reclaim the space with:

    git tag -d $(git tag --list 'pre-untrack-*' | tail -1)
    git reflog expire --expire-unreachable=now --all
    git gc --prune=now
MSG
elif [[ "$MODE" == "noop" ]]; then
    echo
    echo "nothing left to do."
else
    cat <<'MSG'

The removal is staged for your next commit. History still carries the
blobs, so .git will not shrink -- that is expected for this mode.
MSG
fi
