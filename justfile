# Bump the patch version, verify it, commit, tag, and publish the refs.
release:
    #!/usr/bin/env bash
    set -euo pipefail
    status=$(git status --porcelain)
    if [[ -n "$status" ]]; then
        echo "error: worktree must be clean before releasing" >&2
        exit 1
    fi
    current=$(sed -n 's/^version = "\([0-9][0-9]*\)\.\([0-9][0-9]*\)\.\([0-9][0-9]*\)"$/\1.\2.\3/p' Cargo.toml | head -n1)
    if [[ -z "$current" ]]; then
        echo "error: could not read the package version from Cargo.toml" >&2
        exit 1
    fi
    IFS=. read -r major minor patch <<< "$current"
    version="$major.$minor.$((patch + 1))"
    tag="v$version"
    if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
        echo "error: tag $tag already exists" >&2
        exit 1
    fi
    just check
    # Keep failed package validation or commits from leaving a version bump.
    backup=$(mktemp -d)
    cp Cargo.toml Cargo.lock "$backup/"
    rollback=true
    cleanup() {
        result=$?
        if [[ "$rollback" == true ]]; then
            cp "$backup/Cargo.toml" Cargo.toml
            cp "$backup/Cargo.lock" Cargo.lock
            git reset --quiet -- Cargo.toml Cargo.lock
        fi
        rm -rf "$backup"
        exit "$result"
    }
    trap cleanup EXIT
    trap 'exit 1' HUP INT TERM
    sed -i "0,/^version = \"$current\"$/s//version = \"$version\"/" Cargo.toml
    cargo check
    cargo publish --dry-run --allow-dirty --locked
    git add Cargo.toml Cargo.lock
    git commit -m "release $version"
    rollback=false
    git tag -a "$tag" -m "$tag"
    git push origin HEAD
    git push origin "$tag"
    echo "released $tag"

# Run local validation before releasing.
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --all-targets --locked
    cargo package --locked
