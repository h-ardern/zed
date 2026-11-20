.DEFAULT_GOAL := help
.PHONY: help deps linux install run windows

help: ## List available targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "} {printf "  make %-10s %s\n", $$1, $$2}'

deps: ## Install Linux build dependencies (uses sudo)
	./script/linux

linux: ## Build a release tarball at target/release/zed-linux-<arch>.tar.gz
	./script/bundle-linux --skip-sentry

install: ## Build and install Zed into ~/.local (~/.local/bin/zed)
	./script/install-linux

run: ## Run a debug build
	cargo run

windows: ## Build the Windows installer on GitHub Actions and download it to dist/windows
	./script/build-windows-on-github
