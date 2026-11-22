.DEFAULT_GOAL := help
.PHONY: help deps linux install run test windows

help: ## List available targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "} {printf "  make %-10s %s\n", $$1, $$2}'

deps: ## Install Linux build dependencies (uses sudo)
	./script/linux
	cargo install cargo-nextest --locked

linux: ## Build a release tarball at target/release/ded-linux-<arch>.tar.gz
	./script/bundle-linux --skip-sentry

install: ## Build and install Ded into ~/.local (~/.local/bin/ded)
	./script/install-linux

run: ## Run a debug build
	cargo run

test: ## Run tests with a 60s per-test timeout (CRATES="a b" to limit)
	cargo nextest run --no-fail-fast $(if $(CRATES),$(foreach crate,$(CRATES),-p $(crate)),--workspace)

windows: ## Build the Windows installer on GitHub Actions and download it to dist/windows
	./script/build-windows-on-github
