#!/usr/bin/env bash
#
# Release script for imap-mcp-rs: version bump + quality gates + commit + tag.
# Follows the deploy.sh/set-version.sh convention of the sibling Rust repos,
# with this repo's gates: the exact six jobs CI runs, so a red pipeline after
# a release is impossible. The version lives only in Cargo.toml — flake.nix
# reads it from there.
#
# Usage:
#   ./deploy.sh current          Show current version
#   ./deploy.sh <version>        Release the given version (e.g. ./deploy.sh 1.1.0)
#
# Flags:
#   --no-push                    Skip pushing main + tag at the end

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

error() { echo -e "${RED}Error: $1${NC}" >&2; }
info() { echo -e "${BLUE}$1${NC}"; }
success() { echo -e "${GREEN}$1${NC}"; }

if [ ! -f "Cargo.toml" ] || [ ! -f "flake.nix" ]; then
    error "Run this script from the imap-mcp-rs project root."
    exit 1
fi

get_current_version() {
    grep '^version = ' Cargo.toml | head -n1 | sed 's/version = "\(.*\)"/\1/'
}

PUSH=true
VERSION=""
for arg in "$@"; do
    case "$arg" in
        --no-push) PUSH=false ;;
        current)
            get_current_version
            exit 0
            ;;
        -h|--help)
            sed -n '3,14p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        -*) error "Unknown flag: $arg"; exit 1 ;;
        *) VERSION="$arg" ;;
    esac
done

if [ -z "$VERSION" ]; then
    error "Usage: $0 <version> [--no-push]   (or: $0 current)"
    exit 1
fi
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    error "Invalid version '$VERSION' — expected MAJOR.MINOR.PATCH (e.g. 1.1.0)."
    exit 1
fi

CURRENT_VERSION=$(get_current_version)
info "Current version: $CURRENT_VERSION"
info "New version:     $VERSION"

if git rev-parse "v$VERSION" >/dev/null 2>&1; then
    error "Tag v$VERSION already exists."
    exit 1
fi
if ! git diff-index --quiet HEAD --; then
    error "Working directory is not clean. Commit or stash your changes first."
    git status --short
    exit 1
fi
# A release without release notes is not a release.
if ! grep -q "^## $VERSION" CHANGELOG.md; then
    error "CHANGELOG.md has no '## $VERSION' section. Write the release notes first."
    exit 1
fi

info "Updating Cargo.toml (flake.nix follows automatically)..."
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml

echo ""
info "Quality gates — the same six jobs CI runs..."
info "  1/6 cargo fmt --all --check"
cargo fmt --all --check
info "  2/6 cargo clippy (pedantic + nursery, warnings denied)"
cargo clippy --release --all-targets --quiet -- \
    -D warnings -W clippy::pedantic -W clippy::nursery
info "  3/6 cargo test --release --all-targets"
cargo test --release --all-targets --quiet
info "  4/6 cargo build --release (also refreshes Cargo.lock)"
cargo build --release --quiet
info "  5/6 nix build"
nix build
info "  6/6 nix flake check"
nix flake check

# The binary must agree with what we are about to tag; a stale build here
# would ship a version string that contradicts the tag.
BUILT_VERSION=$(./target/release/imap-mcp-rs --version | awk '{print $2}')
if [ "$BUILT_VERSION" != "$VERSION" ]; then
    error "Built binary reports $BUILT_VERSION, expected $VERSION."
    exit 1
fi

echo ""
info "Changes to be committed:"
git --no-pager diff --stat Cargo.toml Cargo.lock CHANGELOG.md

info "Creating release commit + tag..."
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: v$VERSION"
git tag -a "v$VERSION" -m "imap-mcp-rs $VERSION"

if [ "$PUSH" = true ]; then
    info "Pushing main + tag..."
    git push origin main --follow-tags
else
    echo ""
    info "Next steps (skipped due to --no-push):"
    echo "  git push origin main --follow-tags"
fi

echo ""
success "✓ Released imap-mcp-rs $VERSION"
info "Installed copies do not update themselves:"
echo "  nix profile upgrade imap-mcp-rs      # then restart your MCP client"
