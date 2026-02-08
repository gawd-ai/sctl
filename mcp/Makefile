BINARY := mcp-sctl

.PHONY: build dev clean fmt lint doc doc-open check

## Build release binary
build:
	cargo build --release

## Run locally (requires SCTL_URL and SCTL_API_KEY env vars)
dev:
	SCTL_URL=http://localhost:1337 SCTL_API_KEY=dev-key cargo run

## Clean build artifacts
clean:
	cargo clean

## Check formatting
fmt:
	cargo fmt --check

## Run clippy lints
lint:
	cargo clippy -- -D warnings

## Generate documentation
doc:
	cargo doc --no-deps

## Generate and open documentation locally
doc-open:
	cargo doc --no-deps --open

## Run all quality checks
check: fmt lint build
