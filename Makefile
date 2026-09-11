# =============================================================================
# metaText development tasks
#
# The Rust crate is built with Cargo; this Makefile only wraps the common
# formatting, linting, testing and documentation commands into short targets.
# Run `make` or `make help` to list everything.
# =============================================================================

CARGO ?= cargo
FEATURES ?= sqlite,terminal-ui

.DEFAULT_GOAL := help
.PHONY: help build release run run-tui run-cli test test-all fmt fmt-check clippy \
        lint doc doc-open check ci clean

help: ## Show this help message
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Build the debug binary (all features)
	$(CARGO) build --features $(FEATURES)

release: ## Build the optimized release binary (all features)
	$(CARGO) build --release --features $(FEATURES)

run: ## Run the default interface
	$(CARGO) run --features $(FEATURES)

run-tui: ## Run the full screen terminal interface
	$(CARGO) run --features $(FEATURES) -- --mode tui

run-cli: ## Run the scriptable command line interface
	$(CARGO) run --features $(FEATURES) -- --mode cli

test: ## Run the default test suite
	$(CARGO) test

test-all: ## Run the test suite with all features
	$(CARGO) test --all-features

fmt: ## Format the sources
	$(CARGO) fmt --all

fmt-check: ## Verify formatting without writing
	$(CARGO) fmt --all -- --check

clippy: ## Lint with clippy (all targets and features)
	$(CARGO) clippy --all-targets --all-features --workspace

lint: fmt-check clippy ## Run every lint check

doc: ## Build the API documentation
	$(CARGO) doc --no-deps --all-features --workspace

doc-open: ## Build and open the API documentation
	$(CARGO) doc --no-deps --all-features --workspace --open

check: ## Fast type check
	$(CARGO) check --all-targets --all-features --workspace

ci: fmt-check clippy test-all ## Run the same checks as the CI pipeline

clean: ## Remove build artifacts for this crate
	$(CARGO) clean
