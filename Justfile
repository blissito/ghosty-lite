# ghosty-lite — tareas de desarrollo

default:
    @just --list

check:
    cargo check --workspace --all-targets

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

release:
    cargo build --release -p goose-cli --bin ghosty

# Binario estático para VMs Linux
release-musl target="x86_64-unknown-linux-musl":
    cargo build --release -p goose-cli --bin ghosty --target {{target}}
