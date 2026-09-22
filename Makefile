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
.PHONY: help build release run run-tui run-cli run-core test test-all examples tui-e2e fmt fmt-check clippy \
        lint doc doc-open doc-check check ci clean

help: ## Show this help message
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
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

run-core: ## Run the headless core service (no user interface)
	$(CARGO) run --features $(FEATURES) -- --mode core

test: ## Run the default test suite (every workspace crate)
	$(CARGO) test --workspace

test-all: ## Run the test suite with all features (every workspace crate)
	$(CARGO) test --workspace --all-features

examples: ## Build every runnable example (all features)
	$(CARGO) build --examples --all-features

tui-e2e: ## Drive the full screen interface on a pty (startup, commands, two peers, ui options)
	$(CARGO) build --features $(FEATURES)
	python3 scripts/tui-e2e/phase1_core.py
	python3 scripts/tui-e2e/phase2_network.py
	python3 scripts/tui-e2e/phase4_ui_options.py

fmt: ## Format the sources
	$(CARGO) fmt --all

fmt-check: ## Verify formatting without writing
	$(CARGO) fmt --all -- --check

clippy: ## Lint with clippy (all targets and features; warnings are errors)
	$(CARGO) clippy --all-targets --all-features --workspace -- -D warnings

lint: fmt-check clippy ## Run every lint check

doc: ## Build the API documentation
	$(CARGO) doc --no-deps --all-features --workspace

doc-check: ## Build the documentation with warnings as errors (what CI runs)
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --no-deps --all-features --workspace

doc-open: ## Build and open the API documentation
	$(CARGO) doc --no-deps --all-features --workspace --open

check: ## Fast type check
	$(CARGO) check --all-targets --all-features --workspace

ci: fmt-check clippy test-all doc-check ## Run the same checks as the CI pipeline

clean: ## Remove build artifacts for this crate
	$(CARGO) clean
