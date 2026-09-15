set shell := ["bash", "-euo", "pipefail", "-c"]

# Build the release binary into bin/ (what the plugin build hook does offline).
build:
    HERDR_SERVICES_NO_DOWNLOAD=1 bash herdr/install.sh

# Debug build symlinked into bin/ for fast iteration.
dev:
    cargo build
    mkdir -p bin
    ln -sf ../target/debug/herdr-services bin/herdr-services

test:
    cargo test

ci:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Link this checkout into the running herdr as a development plugin.
link: dev
    herdr plugin link "$PWD"

unlink:
    herdr plugin unlink lucidstack.herdr-services

logs:
    herdr plugin log list --plugin lucidstack.herdr-services
