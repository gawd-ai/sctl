TARGET_ARM := armv7-unknown-linux-musleabihf
TARGET_RISCV := riscv64gc-unknown-linux-musl
BINARY := sctl
RELEASE_DIR := target/release
ARM_RELEASE_DIR := target/$(TARGET_ARM)/release
RISCV_RELEASE_DIR := target/$(TARGET_RISCV)/release

.PHONY: build build-arm build-riscv dev clean deploy deploy-riscv upgrade upgrade-riscv fmt lint test doc doc-open check

## Build for local development
build:
	cargo build --release

## Build for ARM (OpenWrt BPI-R2)
build-arm:
	cross build --release --target $(TARGET_ARM)
	@echo "Binary: $(ARM_RELEASE_DIR)/$(BINARY)"
	@ls -lh $(ARM_RELEASE_DIR)/$(BINARY)

## Build for RISC-V 64 (OpenWrt BPI-RV2)
build-riscv:
	cross build --release --target $(TARGET_RISCV)
	@echo "Binary: $(RISCV_RELEASE_DIR)/$(BINARY)"
	@ls -lh $(RISCV_RELEASE_DIR)/$(BINARY)

## Run locally for development
dev:
	SCTL_API_KEY=dev-key RUST_LOG=debug cargo run

## Clean build artifacts
clean:
	cargo clean

## Deploy to ARM device (full: binary + config + init script)
## Usage: make deploy HOST=192.168.1.1
deploy: build-arm
	@test -n "$(HOST)" || (echo "Usage: make deploy HOST=<ip>" && exit 1)
	scp $(ARM_RELEASE_DIR)/$(BINARY) root@$(HOST):/usr/bin/$(BINARY)
	ssh root@$(HOST) "test -f /etc/sctl/sctl.toml" || scp sctl.toml.example root@$(HOST):/etc/sctl/sctl.toml
	scp files/sctl.init root@$(HOST):/etc/init.d/sctl
	ssh root@$(HOST) "sed -i 's/\r//' /etc/init.d/sctl && chmod +x /etc/init.d/sctl && /etc/init.d/sctl enable"
	@echo "Deployed. Start with: ssh root@$(HOST) /etc/init.d/sctl start"

## Deploy to RISC-V device (full: binary + config + init script)
## Usage: make deploy-riscv HOST=10.42.0.192
deploy-riscv: build-riscv
	@test -n "$(HOST)" || (echo "Usage: make deploy-riscv HOST=<ip>" && exit 1)
	scp $(RISCV_RELEASE_DIR)/$(BINARY) root@$(HOST):/usr/bin/$(BINARY)
	ssh root@$(HOST) "test -f /etc/sctl/sctl.toml" || scp sctl.toml.example root@$(HOST):/etc/sctl/sctl.toml
	scp files/sctl.init root@$(HOST):/etc/init.d/sctl
	ssh root@$(HOST) "sed -i 's/\r//' /etc/init.d/sctl && chmod +x /etc/init.d/sctl && /etc/init.d/sctl enable"
	@echo "Deployed. Start with: ssh root@$(HOST) /etc/init.d/sctl start"

## Upgrade ARM device (binary-only, no config changes)
## Usage: make upgrade HOST=192.168.1.1
upgrade: build-arm
	@test -n "$(HOST)" || (echo "Usage: make upgrade HOST=<ip>" && exit 1)
	ssh root@$(HOST) "/etc/init.d/sctl stop; rm -f /usr/bin/$(BINARY)" || true
	scp $(ARM_RELEASE_DIR)/$(BINARY) root@$(HOST):/usr/bin/$(BINARY)
	ssh root@$(HOST) "/etc/init.d/sctl start"
	@echo "Upgraded $(BINARY) on $(HOST)"

## Upgrade RISC-V device (binary-only, no config changes)
## Usage: make upgrade-riscv HOST=10.42.0.192
upgrade-riscv: build-riscv
	@test -n "$(HOST)" || (echo "Usage: make upgrade-riscv HOST=<ip>" && exit 1)
	ssh root@$(HOST) "/etc/init.d/sctl stop; rm -f /usr/bin/$(BINARY)" || true
	scp $(RISCV_RELEASE_DIR)/$(BINARY) root@$(HOST):/usr/bin/$(BINARY)
	ssh root@$(HOST) "/etc/init.d/sctl start"
	@echo "Upgraded $(BINARY) on $(HOST)"

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
