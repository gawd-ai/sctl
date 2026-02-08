TARGET_ARM := armv7-unknown-linux-musleabihf
BINARY := sctl
RELEASE_DIR := target/release
ARM_RELEASE_DIR := target/$(TARGET_ARM)/release

.PHONY: build build-arm dev clean deploy fmt lint test doc doc-open check

## Build for local development
build:
	cargo build --release

## Build for ARM (OpenWrt BPI-R2)
build-arm:
	cross build --release --target $(TARGET_ARM)
	@echo "Binary: $(ARM_RELEASE_DIR)/$(BINARY)"
	@ls -lh $(ARM_RELEASE_DIR)/$(BINARY)

## Run locally for development
dev:
	SCTL_API_KEY=dev-key RUST_LOG=debug cargo run

## Clean build artifacts
clean:
	cargo clean

## Deploy to device (usage: make deploy HOST=192.168.1.1)
deploy: build-arm
	@test -n "$(HOST)" || (echo "Usage: make deploy HOST=<ip>" && exit 1)
	scp $(ARM_RELEASE_DIR)/$(BINARY) root@$(HOST):/usr/bin/$(BINARY)
	scp sctl.toml.example root@$(HOST):/etc/sctl/sctl.toml
	scp files/sctl.init root@$(HOST):/etc/init.d/sctl
	ssh root@$(HOST) "chmod +x /etc/init.d/sctl && /etc/init.d/sctl enable"
	@echo "Deployed. Start with: ssh root@$(HOST) /etc/init.d/sctl start"

## Check formatting
fmt:
	cargo fmt --check

## Run clippy lints
lint:
	cargo clippy -- -D warnings

## Run tests
test:
	cargo test

## Generate documentation
doc:
	cargo doc --no-deps

## Generate and open documentation locally
doc-open:
	cargo doc --no-deps --open

## Run all quality checks
check: fmt lint test build
