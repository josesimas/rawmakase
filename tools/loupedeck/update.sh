#!/usr/bin/env bash
# Bring this fork up to date with the original project (the `upstream` remote):
# fetch it, keep `main` an exact mirror, rebase the fork's branch onto the
# newest release, run the CI checks, and optionally build the app and push.
#
#   tools/loupedeck/update.sh [--dry-run] [--main] [--build] [--push] [--skip-checks]
#
#   --dry-run      only fetch and say what would change and whether it conflicts
#   --main         rebase onto upstream/main instead of the newest release tag
#   --build        build target/release/RAWmakase.app afterwards (macOS)
#   --push         push the branch (force-with-lease) and main to `origin`
#   --skip-checks  skip cargo fmt / clippy / test
#
# A conflict in Cargo.lock alone is resolved automatically (upstream's file,
# then cargo adds this fork's dependencies back). Any other conflict stops the
# update and puts everything back as it was; the branch before the rebase is
# also kept as `backup/before-update`.
set -euo pipefail

BRANCH=loupedeck
DRY=0 MAIN=0 BUILD=0 PUSH=0 CHECKS=1
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY=1 ;;
        --main) MAIN=1 ;;
        --build) BUILD=1 ;;
        --push) PUSH=1 ;;
        --skip-checks) CHECKS=0 ;;
        -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg (see --help)" >&2; exit 2 ;;
    esac
done

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

cd "$(git rev-parse --show-toplevel)"
# Homebrew's rustup is keg-only, so it is not on PATH by default.
command -v cargo >/dev/null 2>&1 || PATH="/opt/homebrew/opt/rustup/bin:$PATH"
command -v cargo >/dev/null 2>&1 || die "cargo not found"
git remote get-url upstream >/dev/null 2>&1 ||
    die "no 'upstream' remote: git remote add upstream https://github.com/pch/rawmakase.git"
[ "$(git branch --show-current)" = "$BRANCH" ] || die "switch to the '$BRANCH' branch first"
[ -z "$(git status --porcelain --untracked-files=no)" ] || die "commit or stash your changes first"

say "Fetching upstream"
git fetch --quiet upstream --tags

if [ "$MAIN" = 1 ]; then
    target=upstream/main
else
    # The newest stable release (no -rc / -beta suffix) on upstream's main.
    target=$(git tag -l 'v[0-9]*' --merged upstream/main | grep -v -- '-' | sort -V | tail -1)
    [ -n "$target" ] || die "no release tag found on upstream"
fi

base=$(git merge-base HEAD "$target")
new=$(git rev-list --count "$base..$target")
if git merge-base --is-ancestor "$target" HEAD; then
    say "Already based on $target; nothing new from upstream"
    UPDATE=0
else
    UPDATE=1
    say "$new new upstream commits up to $target"
    git log --oneline -n 15 "$base..$target"
    [ "$new" -le 15 ] || echo "  ... and $((new - 15)) more"
fi

if [ "$UPDATE" = 1 ]; then
    conflicts=$(git merge-tree --write-tree --name-only HEAD "$target" | sed '1d;/^$/,$d' || true)
    if [ -n "$conflicts" ]; then
        echo "Would conflict in: $(echo "$conflicts" | tr '\n' ' ')"
        echo "(Cargo.lock alone is resolved automatically.)"
    else
        echo "No conflicts expected."
    fi
fi
[ "$DRY" = 0 ] || exit 0

if [ "$UPDATE" = 1 ]; then
    say "Updating main to upstream/main"
    # Refuses to move main unless it is a fast-forward, so local work is safe.
    git fetch --quiet . upstream/main:main || echo "main was not updated (it has local commits)"

    say "Rebasing $BRANCH onto $target"
    git branch -f backup/before-update HEAD
    if ! git rebase "$target" >/dev/null 2>&1; then
        while [ -d "$(git rev-parse --git-path rebase-merge)" ] ||
            [ -d "$(git rev-parse --git-path rebase-apply)" ]; do
            conflicted=$(git diff --name-only --diff-filter=U)
            if [ "$conflicted" != "Cargo.lock" ]; then
                git rebase --abort
                echo "Conflicts in:" >&2
                echo "$conflicted" | sed 's/^/  /' >&2
                die "not resolved automatically; nothing was changed. Resolve by hand with: git rebase $target"
            fi
            echo "  Cargo.lock: taking upstream's and adding this fork's dependencies back"
            git checkout --ours Cargo.lock
            cargo metadata --quiet --format-version 1 >/dev/null 2>&1
            git add Cargo.lock
            GIT_EDITOR=true git rebase --continue >/dev/null 2>&1 || true
        done
    fi
    git merge-base --is-ancestor "$target" HEAD ||
        die "the rebase did not finish; the branch before it is backup/before-update"
fi

if [ "$CHECKS" = 1 ]; then
    say "Checks: fmt, clippy, tests"
    cargo fmt --check || die "cargo fmt failed (the branch before the rebase is backup/before-update)"
    cargo clippy --all-targets --locked -- -D warnings ||
        die "clippy failed (the branch before the rebase is backup/before-update)"
    cargo test --locked || die "tests failed (the branch before the rebase is backup/before-update)"
fi

if [ "$BUILD" = 1 ]; then
    say "Building the app"
    ./packaging/macos/app.sh
    bundle=target/release/RAWmakase.app
    # app.sh needs librsvg for the icon; reuse the installed app's instead.
    installed=/Applications/RAWmakase.app/Contents/Resources
    if [ ! -f "$bundle/Contents/Resources/rawmakase.icns" ] && [ -f "$installed/rawmakase.icns" ]; then
        cp "$installed/rawmakase.icns" "$installed/Assets.car" "$bundle/Contents/Resources/"
        plist=$bundle/Contents/Info.plist
        /usr/libexec/PlistBuddy -c 'Set :CFBundleIconFile rawmakase.icns' "$plist" 2>/dev/null ||
            /usr/libexec/PlistBuddy -c 'Add :CFBundleIconFile string rawmakase.icns' "$plist"
        /usr/libexec/PlistBuddy -c 'Set :CFBundleIconName RAWmakase' "$plist" 2>/dev/null ||
            /usr/libexec/PlistBuddy -c 'Add :CFBundleIconName string RAWmakase' "$plist"
    fi
    # Seals the bundle (Info.plist, icon) so it verifies; ad-hoc, for this Mac.
    codesign --force --deep --sign - "$bundle" 2>/dev/null
    codesign --verify --deep --strict "$bundle"
    say "Built $PWD/$bundle"
fi

if [ "$PUSH" = 1 ]; then
    say "Pushing to origin"
    git push --force-with-lease origin "$BRANCH"
    git push origin main
fi

say "Done: $BRANCH is on $target"
