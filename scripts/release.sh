#!/usr/bin/env bash
# Cut a new nogent release.
#
#   ./scripts/release.sh <version>     # e.g. ./scripts/release.sh 0.2.2
#
# What this does:
#   1. Sanity-checks: on `main`, clean tree, tag not already used.
#   2. Bumps both crate `Cargo.toml` versions to <version>.
#   3. Runs `cargo build --workspace` so `Cargo.lock` updates.
#   4. Shows you the resulting diff and asks for confirmation.
#   5. Commits, pushes `main`, creates `v<version>` tag, pushes the tag.
#
# Pushing the tag triggers `.github/workflows/image.yml`, which pauses at the
# `release` environment for your approval before publishing + signing the image.
#
# What this does NOT do (intentionally — separate operational steps):
#   * Bump `deploy/terraform/terraform.tfvars` `image = "...:<version>"`.
#   * Run `terraform apply` to roll the EC2 instance.
# Those happen AFTER the workflow has finished publishing the signed image.

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
say() { printf '\n→ %s\n' "$*"; }

[ $# -eq 1 ] || die "usage: $0 <version>   (e.g. $0 0.2.2)"
VERSION="$1"

# Reject a leading v — we add it for the tag, but the Cargo version is bare.
[[ "$VERSION" =~ ^v ]] && die "pass a bare version like 0.2.2, not v0.2.2"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]] \
  || die "'$VERSION' is not a semver-shaped version"

TAG="v$VERSION"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# Branch + working tree must be clean.
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "main" ] || die "must be on main (currently on '$BRANCH')"
git diff --quiet || die "working tree has unstaged changes — commit or stash first"
git diff --cached --quiet || die "staged changes present — commit or reset first"

# Make sure we're up to date with the remote.
say "fetching origin"
git fetch --tags origin main
LOCAL="$(git rev-parse HEAD)"
REMOTE="$(git rev-parse origin/main)"
[ "$LOCAL" = "$REMOTE" ] || die "local main is not in sync with origin/main"

# Tag must not already exist (locally or on the remote).
git rev-parse --verify --quiet "refs/tags/$TAG" >/dev/null \
  && die "tag $TAG already exists locally — pick the next version"
git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1 \
  && die "tag $TAG already exists on origin — pick the next version"

# Bump both crate versions. We only touch the [package] version line, never a
# [dependencies] one — anchor the match to start-of-line.
say "bumping crates to $VERSION"
for f in crates/nogent-core/Cargo.toml crates/nogent-listener/Cargo.toml; do
  [ -f "$f" ] || die "missing $f"
  # macOS sed and GNU sed disagree on -i; use a portable form via a temp file.
  awk -v v="$VERSION" '
    BEGIN { done = 0 }
    /^version = / && !done { sub(/"[^"]+"/, "\"" v "\""); done = 1 }
    { print }
  ' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
  grep -q "^version = \"$VERSION\"$" "$f" || die "failed to bump $f"
done

# Rebuild so Cargo.lock reflects the new versions.
say "cargo build --workspace (updates Cargo.lock)"
cargo build --workspace --quiet

# Show what's about to be committed and confirm.
say "diff to be committed:"
git --no-pager diff -- crates/nogent-core/Cargo.toml crates/nogent-listener/Cargo.toml Cargo.lock
echo
read -r -p "Commit, push main, tag $TAG, and push the tag? [y/N] " ANSWER
case "$ANSWER" in
  y|Y|yes|YES) ;;
  *) die "aborted — your version bumps are still on disk; \`git checkout -- .\` to undo" ;;
esac

# Commit + push + tag + push tag.
say "committing"
git add crates/nogent-core/Cargo.toml crates/nogent-listener/Cargo.toml Cargo.lock
git commit -m "chore(release): bump to $VERSION"

say "pushing main"
git push origin main

say "tagging $TAG"
git tag -a "$TAG" -m "release $VERSION"

say "pushing $TAG (this triggers .github/workflows/image.yml)"
git push origin "$TAG"

cat <<EOF

Done. Next steps:

1. Watch the publish workflow and approve the \`release\` environment gate:
     gh run watch --repo nolabs-ai/nogent
   The workflow will push and cosign-sign ghcr.io/nolabs-ai/nogent:$VERSION.

2. Once the image is published, roll the EC2 instance:
     # in deploy/terraform/terraform.tfvars
     image = "ghcr.io/nolabs-ai/nogent:$VERSION"

     cd deploy/terraform && terraform apply

   (Or, for an in-place upgrade without replacing the instance, SSM in and
   bump IMAGE_TAG then \`docker compose pull && cosign verify … && up -d\`.)
EOF
