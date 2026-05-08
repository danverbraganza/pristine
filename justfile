# Default: show available targets
default:
    @just --list

# Install development tool dependencies
[group('development')] 
install-dev-deps:
    cargo install ratchets@0.2.6

# Build and install the local version of pristine
[group('development')] 
install:
    cargo install --path="."

# Run tests via cargo-nextest
[group('test')] 
test:
    cargo nextest run
