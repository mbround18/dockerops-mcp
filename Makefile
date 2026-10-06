# dockerops-mcp — development and installation entry points.
#
# `make install` is the one that matters: the registered MCP server runs the
# installed binary, so every code change needs a reinstall before an agent
# sees it.

CARGO ?= cargo
APP   := apps/dockerops-mcp

.DEFAULT_GOAL := help

.PHONY: help
help: ## List the available targets
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "} {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

.PHONY: install
install: ## Build in release mode and install dockerops-mcp onto PATH
	$(CARGO) install --path $(APP) --locked --force

.PHONY: uninstall
uninstall: ## Remove the installed dockerops-mcp binary
	$(CARGO) uninstall dockerops-mcp

.PHONY: build
build: ## Debug build of the whole workspace
	$(CARGO) build --workspace

.PHONY: test
test: ## Unit and end-to-end tests (no daemon required)
	$(CARGO) test --workspace

.PHONY: lint
lint: ## Clippy across every target, warnings denied
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Format the workspace
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Fail if anything is unformatted
	$(CARGO) fmt --all --check

.PHONY: check
check: fmt-check lint test ## Everything CI would run

.PHONY: clean
clean: ## Remove build artifacts
	$(CARGO) clean
