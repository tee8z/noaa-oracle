# NOAA Oracle - Development & Deployment Commands
# Run 'just' or 'just --list' to see available commands

set dotenv-load := true

# Default recipe - show help
default:
    @just --list

# =============================================================================
# Development
# =============================================================================

# Enter development shell with all dependencies
dev:
    nix develop

# Build all crates
build:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo build --workspace
    else
        nix develop -c cargo build --workspace
    fi

# Build in release mode
build-release:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo build --workspace --release
    else
        nix develop -c cargo build --workspace --release
    fi

# Build oracle only
build-oracle:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo build -p oracle
    else
        nix develop -c cargo build -p oracle
    fi

# Build daemon only
build-daemon:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo build -p daemon
    else
        nix develop -c cargo build -p daemon
    fi

# =============================================================================
# Testing & Linting
# =============================================================================

# Run all tests (unit tests only, use test-e2e for integration tests)
test:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo test --workspace --lib
    else
        nix develop -c cargo test --workspace --lib
    fi

# Run e2e/integration tests (requires test environment setup)
test-e2e:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo test --package oracle --test api -- --test-threads=1
    else
        nix develop -c cargo test --package oracle --test api -- --test-threads=1
    fi

# Run all tests including e2e
test-all: test test-e2e

# Run clippy lints
clippy:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo clippy --workspace --all-targets -- -D warnings
    else
        nix develop -c cargo clippy --workspace --all-targets -- -D warnings
    fi

# Check formatting
fmt-check:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo fmt --all -- --check
    else
        nix develop -c cargo fmt --all -- --check
    fi

# Format code
fmt:
    #!/usr/bin/env bash
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo fmt --all
    else
        nix develop -c cargo fmt --all
    fi

# Run all checks (fmt, clippy, test)
check: fmt-check clippy test

# =============================================================================
# Running Services
# =============================================================================

# Run oracle server (development mode)
run-oracle *ARGS:
    #!/usr/bin/env bash
    export RUST_LOG="${RUST_LOG:-info,oracle=debug}"
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo run -p oracle -- {{ARGS}}
    else
        nix develop -c cargo run -p oracle -- {{ARGS}}
    fi

# Run daemon (development mode)
run-daemon *ARGS:
    #!/usr/bin/env bash
    export RUST_LOG="${RUST_LOG:-info,daemon=debug}"
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo run -p daemon -- {{ARGS}}
    else
        nix develop -c cargo run -p daemon -- {{ARGS}}
    fi

# Run oracle with existing weather data (standalone mode)
run-oracle-standalone DATA_DIR *ARGS:
    #!/usr/bin/env bash
    export RUST_LOG="${RUST_LOG:-info,oracle=debug}"
    export NOAA_ORACLE_DATA_DIR="{{DATA_DIR}}"
    if [ -n "$IN_NIX_SHELL" ]; then
        cargo run -p oracle -- {{ARGS}}
    else
        nix develop -c cargo run -p oracle -- {{ARGS}}
    fi

# =============================================================================
# Nix Builds (Production)
# =============================================================================

# Build oracle using Nix
nix-oracle:
    nix build .#oracle

# Build daemon using Nix
nix-daemon:
    nix build .#daemon

# Build all packages
nix-all:
    nix build .#oracle
    nix build .#daemon

# Run Nix flake check
nix-check:
    nix flake check

# Run oracle from Nix build
nix-run-oracle *ARGS:
    nix run .#oracle -- {{ARGS}}

# Run daemon from Nix build
nix-run-daemon *ARGS:
    nix run .#daemon -- {{ARGS}}

# =============================================================================
# Setup & Configuration
# =============================================================================

# Setup local development environment
setup:
    mkdir -p weather_data event_data data logs
    @if [ ! -f "oracle_private_key.pem" ]; then \
        echo "Generating oracle signing key..."; \
        openssl ecparam -genkey -name secp256k1 -out oracle_private_key.pem; \
        chmod 600 oracle_private_key.pem; \
        echo "Key generated: oracle_private_key.pem"; \
    else \
        echo "Key already exists: oracle_private_key.pem"; \
    fi
    @echo ""
    @echo "Setup complete! You can now run:"
    @echo "  just run-oracle    # Start the oracle server"
    @echo "  just run-daemon    # Start the data fetcher"

# Copy example configs to current directory
init-config:
    @if [ ! -f "oracle.toml" ]; then \
        cp config/oracle.example.toml oracle.toml; \
        echo "Created oracle.toml"; \
    else \
        echo "oracle.toml already exists"; \
    fi
    @if [ ! -f "daemon.toml" ]; then \
        cp config/daemon.example.toml daemon.toml; \
        echo "Created daemon.toml"; \
    else \
        echo "daemon.toml already exists"; \
    fi

# =============================================================================
# Cleanup
# =============================================================================

# Clean build artifacts
clean:
    cargo clean

# Clean test data
clean-test:
    rm -rf test_data/

# Clean all data (weather, events, logs)
clean-data:
    rm -rf weather_data/* event_data/* data/* logs/*

# Clean everything (build, test data, runtime data)
clean-all: clean clean-test clean-data
    rm -rf result .direnv

# =============================================================================
# Release
# =============================================================================

# Update version in all Cargo.toml files
set-version VERSION:
    #!/usr/bin/env bash
    if ! command -v cargo-set-version &> /dev/null; then
        echo "Installing cargo-edit..."
        cargo install cargo-edit --locked
    fi
    cargo set-version --workspace {{VERSION}}
    echo "Version updated to {{VERSION}}"

# Create a release tag
release VERSION:
    just set-version {{VERSION}}
    git add Cargo.toml Cargo.lock crates/*/Cargo.toml
    git commit -m "chore: release v{{VERSION}}"
    git tag -a "v{{VERSION}}" -m "Release v{{VERSION}}"
    @echo ""
    @echo "Created tag v{{VERSION}}"
    @echo "Run: git push origin master --tags"

# =============================================================================
# Help
# =============================================================================

# Show project info
info:
    @echo "NOAA Oracle - Decentralized Weather Data Oracle"
    @echo ""
    @echo "Two independent services:"
    @echo ""
    @echo "  Oracle - REST API server that:"
    @echo "    - Serves weather data (parquet files)"
    @echo "    - Hosts browser UI for querying data"
    @echo "    - Manages DLC prediction events"
    @echo "    - Signs attestations for dlctix contracts"
    @echo "    - Can run standalone with pre-existing data"
    @echo ""
    @echo "  Daemon - Background service that:"
    @echo "    - Fetches weather data from NOAA APIs"
    @echo "    - Creates parquet files"
    @echo "    - Uploads to oracle server"
    @echo "    - Optional: oracle can run without it"
    @echo ""
    @echo "Quick start:"
    @echo "  just setup         # Create dirs and signing key"
    @echo "  just run-oracle    # Start oracle (port 9800)"
    @echo "  just run-daemon    # Start daemon (optional)"
    @echo ""
    @echo "Using existing data:"
    @echo "  just run-oracle-standalone /path/to/weather_data"
