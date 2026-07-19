CARGO          := cargo
ALL_FEATURES   := --all-features

.PHONY: build check test clippy fmt run lint docs examples boundary ci help

## build: Build the whole workspace (debug, all features)
build:
	$(CARGO) build $(ALL_FEATURES)

## check: cargo check across the workspace (fast, no codegen)
check:
	$(CARGO) check $(ALL_FEATURES)

## test: Run all unit + integration tests (includes doctests)
test:
	$(CARGO) test $(ALL_FEATURES)

## clippy: Lint with -D warnings (must match CI exactly)
clippy:
	$(CARGO) clippy --all-targets $(ALL_FEATURES) -- -D warnings

## fmt: Check formatting (fails if not formatted) — matches CI
fmt:
	$(CARGO) fmt --all -- --check

## lint: Auto-format the code (write, not check)
lint:
	$(CARGO) fmt --all

## run: Build and run the dch binary (pass ARGS="...")
run:
	$(CARGO) run $(ALL_FEATURES) --bin dch -- $(ARGS)

## docs: Build rustdoc, treating warnings as errors (matches CI)
docs:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --no-deps $(ALL_FEATURES)

## examples: Build all examples
examples:
	$(CARGO) build --examples $(ALL_FEATURES)

## boundary: Enforce the crate-boundary rules
boundary:
	$(CARGO) run -p xtask -- check-boundary

## ci: Run the full local CI gate (fmt, clippy, test, docs, examples, boundary)
ci: fmt clippy test docs examples boundary
	@echo "✅ CI passed locally"

## help: Show this help
help:
	@echo "dch — developer Makefile"
	@echo ""
	@echo "Targets:"
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
