# Bump the version (patch/minor/major or X.Y.Z), commit, tag and push.
# Run `just check` first; cargo-release refuses a dirty tree.
# GitHub CI publishes the crate and release when the version tag is pushed.
release level="patch":
    cargo release -p mkfs-mfs {{level}} --execute --no-confirm

# Run local validation before releasing.
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --all-targets --locked
    cargo package --locked
