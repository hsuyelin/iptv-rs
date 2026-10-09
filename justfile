# Verification recipes mirror .agents/AGENTS.md of the iptv-vod repository.
default:
    @just --list

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --all-features --frozen -- -D warnings

test:
    cargo test --workspace --all-features --frozen

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --frozen

bench:
    cargo bench --workspace

# Advisories, licenses, bans and sources (see deny.toml).
audit:
    cargo deny check

# The media crate must stay free of network, async and WASM dependencies.
deps:
    ! cargo tree -p iptv-media -e normal --prefix none | grep -E '^(axum|reqwest|wasmtime|tokio|hyper) '

# No Service Locator vocabulary in Rust sources.
names:
    ! grep -RniE --include='*.rs' -e 'resolv[a-z_]*|provider|locator|registry|container' crates

# Optimised binary at target/release/iptv-rs.
build:
    cargo build --release --locked -p iptv-server

# Rewrite assets/manifest.json after replacing an asset.
assets-manifest:
    cargo run -q -p iptv-wasm --example manifest -- assets

all: fmt lint test doc deps names audit
