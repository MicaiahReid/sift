default:
    just check

fmt:
    cargo fmt --all

check:
    cargo check --workspace

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings

install:
    cargo install --path crates/cli --locked --force
