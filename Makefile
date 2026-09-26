CARGO          := cargo
ALL_FEATURES   := --all-features
TARGET         := x86_64-unknown-linux-musl
TARGET_MACOS   := aarch64-apple-darwin
BINARY         := dch
BUILD_DIR      := target/$(TARGET)/release
BUILD_DIR_MACOS := target/$(TARGET_MACOS)/release
ZIG            ?= zig
MACOS_SDK_PATH ?= /home/dch/MacOSX-SDKs/MacOSX14.5.sdk

.PHONY: build check test clippy fmt run lint docs examples boundary nodefault release-check smoke ci help static macos strip macos-strip install check-zig check-target check-macos-target check-macos-sdk check-openssl

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

## nodefault: Prove the workspace compiles without default features
nodefault:
	$(CARGO) check --no-default-features

## release-check: Run the automatable acceptance suite against a release build
release-check:
	$(CARGO) build --release --bin dch $(ALL_FEATURES)
	DCH_BIN="$$(d="$${CARGO_TARGET_DIR:-$(CURDIR)/target}"; case "$$d" in /*) ;; *) d="$(CURDIR)/$$d";; esac; printf '%s' "$$d")/release/dch" $(CARGO) test --test release_gate $(ALL_FEATURES)

## smoke: Run the real-provider and PTY-driven smoke cases (needs DCH_E2E=1)
smoke:
	DCH_E2E=1 $(CARGO) test --test smoke $(ALL_FEATURES) -- --ignored --test-threads=1

## static: Build the fully static musl release binary (zig, rustls-only graph)
static: check-zig check-target check-openssl
	RUSTFLAGS="-C target-feature=+crt-static" $(CARGO) zigbuild \
		--release --all-features --target $(TARGET) --bin $(BINARY)
	@file $(BUILD_DIR)/$(BINARY)
	@if file $(BUILD_DIR)/$(BINARY) | grep -q 'dynamically linked'; then \
		echo "❌ $(BUILD_DIR)/$(BINARY) linked dynamically — the static build is broken"; \
		exit 1; \
	fi
	@ldd $(BUILD_DIR)/$(BINARY) 2>&1 || true

## macos: Cross-build the Apple-Silicon release binary (zig + macOS SDK)
macos: check-macos-target check-zig check-macos-sdk
	SDKROOT="$(MACOS_SDK_PATH)" $(CARGO) zigbuild \
		--release --all-features --target $(TARGET_MACOS) --bin $(BINARY)
	@file $(BUILD_DIR_MACOS)/$(BINARY)

## strip: Strip the static musl binary and report its size
strip: static
	strip $(BUILD_DIR)/$(BINARY)
	@echo "✅ Stripped: $(BUILD_DIR)/$(BINARY) ($$(du -h $(BUILD_DIR)/$(BINARY) | cut -f1))"

## macos-strip: Report the profile-stripped macOS binary size
macos-strip: macos
	@echo "✅ macOS binary: $(BUILD_DIR_MACOS)/$(BINARY) ($$(du -h $(BUILD_DIR_MACOS)/$(BINARY) | cut -f1))"

## install: Install the stripped static binary to ~/.local/bin
install: strip
	@mkdir -p $(HOME)/.local/bin
	@cp $(BUILD_DIR)/$(BINARY) $(HOME)/.local/bin/$(BINARY)
	@chmod +x $(HOME)/.local/bin/$(BINARY)
	@echo "✅ Installed to $(HOME)/.local/bin/$(BINARY)"

## check-zig: Verify the zig compiler is available
check-zig:
	@if ! $(ZIG) version >/dev/null 2>&1; then \
		echo "❌ zig not found ($(ZIG))"; \
		exit 1; \
	fi

## check-target: Verify the musl target is installed
check-target:
	@if ! rustup target list --installed | grep -q "$(TARGET)"; then \
		echo "❌ Rust target $(TARGET) not installed — run: rustup target add $(TARGET)"; \
		exit 1; \
	fi

## check-macos-target: Verify the Apple-Silicon target is installed
check-macos-target:
	@if ! rustup target list --installed | grep -q "$(TARGET_MACOS)"; then \
		echo "❌ Rust target $(TARGET_MACOS) not installed — run: rustup target add $(TARGET_MACOS)"; \
		exit 1; \
	fi

## check-macos-sdk: Verify a macOS SDK is present (override with MACOS_SDK_PATH=…)
check-macos-sdk:
	@if [ ! -d "$(MACOS_SDK_PATH)" ]; then \
		echo "❌ macOS SDK not found at $(MACOS_SDK_PATH)"; \
		echo "   Unpack a MacOSX SDK and pass MACOS_SDK_PATH=/path/to/sdk"; \
		exit 1; \
	fi

## check-openssl: Fail if openssl-sys re-enters the dependency graph
check-openssl:
	@$(CARGO) tree --locked --all-features >/dev/null
	@if $(CARGO) tree --locked --all-features -i openssl-sys >/dev/null 2>&1; then \
		echo "❌ openssl-sys is in the graph — the static build is rustls-only"; \
		exit 1; \
	fi
	@echo "✅ no openssl-sys in the dependency graph"

## ci: Run the full local CI gate (fmt, clippy, test, docs, examples, boundary, nodefault, openssl)
ci: fmt clippy test docs examples boundary nodefault check-openssl
	@echo "✅ CI passed locally"

## help: Show this help
help:
	@echo "dch — developer Makefile"
	@echo ""
	@echo "Targets:"
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
