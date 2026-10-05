#!/usr/bin/env bash
# Bump the workspace version (single source of truth: [workspace.package]
# version in the root Cargo.toml) and commit + tag it, replicating the
# commit+tag behavior the old `npm version` flow provided.
#
# Usage:
#   scripts/bump-version.sh <major|minor|patch|X.Y.Z> [--dry-run]
#
# Environment: none required. Requires git, cargo, sed.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
MANIFEST="$ROOT/Cargo.toml"

usage() {
  echo "usage: $0 <major|minor|patch|X.Y.Z> [--dry-run]" >&2
  exit 2
}

[ $# -ge 1 ] || usage
BUMP="$1"
DRY_RUN=0
[ $# -le 2 ] || usage
[ $# -eq 2 ] && { [ "$2" = "--dry-run" ] || usage; DRY_RUN=1; }

# Validate the argument: keyword or explicit X.Y.Z.
case "$BUMP" in
  major|minor|patch) ;;
  *)
    if ! echo "$BUMP" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
      usage
    fi
    ;;
esac

# Guard: refuse on dirty tree (npm version parity).
if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  echo "error: working tree is not clean — commit or stash first" >&2
  exit 1
fi

# Read the current version: the single `^version = "…"` line under
# [workspace.package] in the root manifest.
CURRENT="$(sed -n '/^\[workspace\.package\]/,/^\[/ { /^version = "\(.*\)"$/ { s//\1/p; q } }' "$MANIFEST")"
if [ -z "$CURRENT" ]; then
  echo "error: could not read [workspace.package] version from Cargo.toml" >&2
  exit 1
fi

# Compute the new version (plain integer math with carry).
if echo "$BUMP" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  NEW="$BUMP"
else
  MAJOR="$(echo "$CURRENT" | cut -d. -f1)"
  MINOR="$(echo "$CURRENT" | cut -d. -f2)"
  PATCH="$(echo "$CURRENT" | cut -d. -f3)"
  case "$BUMP" in
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    patch) PATCH=$((PATCH + 1)) ;;
  esac
  NEW="$MAJOR.$MINOR.$PATCH"
fi

COMMIT_MSG="chore(release): bump version to $NEW"
TAG="v$NEW"

echo "plan:"
echo "  version: $CURRENT -> $NEW"
echo "  files:   Cargo.toml, Cargo.lock"
echo "  commit:  $COMMIT_MSG"
echo "  tag:     $TAG"
if [ "$DRY_RUN" = 1 ]; then
  echo "(dry run — nothing applied)"
  exit 0
fi

# Apply: sed the root manifest, refresh the lock's member versions.
TMP="$(mktemp)"
sed "s/^version = \"${CURRENT}\"$/version = \"${NEW}\"/" "$MANIFEST" > "$TMP"
# Post-check: the sed touched exactly one line (the SoT version line).
CHANGED="$(diff "$MANIFEST" "$TMP" | grep -c '^> version = ' || true)"
[ "$CHANGED" = 1 ] || {
  echo "error: expected to change exactly one version line, changed $CHANGED — aborting" >&2
  rm -f "$TMP"
  exit 1
}
mv "$TMP" "$MANIFEST"

( cd "$ROOT" && cargo update --workspace >/dev/null )

# Verify the lock now shows the new version for all four members.
MISS=0
for member in audio-core gui-engine ltc-gui ltc-slint; do
  if ! grep -A1 "name = \"$member\"" "$ROOT/Cargo.lock" | grep -q "version = \"$NEW\""; then
    echo "error: Cargo.lock does not show $NEW for $member" >&2
    MISS=1
  fi
done
[ "$MISS" = 0 ] || exit 1

git -C "$ROOT" add Cargo.toml Cargo.lock
git -C "$ROOT" commit -m "$COMMIT_MSG"
# Annotated tag: `git push --follow-tags` (and tools with the same
# convention) only push annotated tag objects — a lightweight tag here
# would be silently skipped and the release workflow never fires.
git -C "$ROOT" tag -a "$TAG" -m "$TAG"
echo "done: $COMMIT_MSG ($TAG)"
