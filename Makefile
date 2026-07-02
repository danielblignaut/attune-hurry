SCCACHE_BUCKET ?= tilde-sccache-prod
SCCACHE_REGION ?= us-east-1
HURRY_ARTIFACT_PREFIX ?= hurry/releases/latest
HURRY_LINUX_TARGET ?= x86_64-unknown-linux-gnu
HURRY_MAC_TARGET ?= aarch64-apple-darwin
HURRY_MAC_SDKROOT ?= /opt/MacOSX11.3.sdk
HURRY_LINUX_BIN := target/$(HURRY_LINUX_TARGET)/release/hurry
HURRY_MAC_BIN := target/$(HURRY_MAC_TARGET)/release/hurry
HURRY_LINUX_S3_URI := s3://$(SCCACHE_BUCKET)/$(HURRY_ARTIFACT_PREFIX)/hurry-$(HURRY_LINUX_TARGET)
HURRY_MAC_S3_URI := s3://$(SCCACHE_BUCKET)/$(HURRY_ARTIFACT_PREFIX)/hurry-$(HURRY_MAC_TARGET)

.PHONY: help format check check-fix autoinherit machete machete-fix cargo-sort precommit dev release sqlx-prepare install install-dev reset-local-cache courier-local-auth hurry-build-linux hurry-build-mac _hurry-build-mac-with-sdk hurry-upload-linux hurry-upload-mac hurry-upload-all

.DEFAULT_GOAL := help

help:
	@echo "Available commands:"
	@echo "  make format             - Format code with cargo +nightly fmt"
	@echo "  make check              - Run clippy linter"
	@echo "  make check-fix          - Run clippy with automatic fixes"
	@echo "  make cargo-sort         - Sort dependencies in Cargo.toml files"
	@echo "  make precommit          - Run checks and automated fixes before committing"
	@echo "  make dev                - Build in debug mode"
	@echo "  make release            - Build in release mode"
	@echo "  make sqlx-prepare       - Prepare sqlx metadata for courier and hurry"
	@echo "  make install            - Install hurry locally"
	@echo "  make install-dev        - Install hurry locally, renaming to 'hurry-dev'"
	@echo "  make hurry-build-linux  - Build hurry for Linux ($(HURRY_LINUX_TARGET))"
	@echo "  make hurry-build-mac    - Build hurry for macOS ($(HURRY_MAC_TARGET))"
	@echo "  make hurry-upload-linux - Build and upload Linux hurry binary to S3"
	@echo "  make hurry-upload-mac   - Build and upload macOS hurry binary to S3"
	@echo "  make hurry-upload-all   - Build and upload Linux + macOS hurry binaries to S3"
	@echo "  make reset-local-cache  - Reset local courier instance (docker down, clear data, migrate)"
	@echo "  make courier-local-auth - Load auth fixture data into local courier database"

format:
	cargo +nightly fmt

check:
	cargo clippy

check-fix:
	cargo clippy --fix --allow-dirty --allow-staged

autoinherit:
	cargo autoinherit

machete:
	cargo machete

machete-fix:
	cargo machete --fix || true

cargo-sort:
	cargo sort --workspace

precommit: machete-fix autoinherit cargo-sort check-fix format sqlx-prepare

dev:
	cargo build

release:
	cargo build --release

hurry-build-linux:
	rustup target add $(HURRY_LINUX_TARGET)
	cargo build --release -p hurry --bin hurry --target $(HURRY_LINUX_TARGET)
	@chmod +x "$(HURRY_LINUX_BIN)"
	@"$(HURRY_LINUX_BIN)" --version

hurry-build-mac:
	@rustup target add $(HURRY_MAC_TARGET)
	@if [ -z "$${SDKROOT:-}" ] && [ -d "$(HURRY_MAC_SDKROOT)" ]; then \
		export SDKROOT="$(HURRY_MAC_SDKROOT)"; \
		echo "Using SDKROOT=$$SDKROOT"; \
		$(MAKE) _hurry-build-mac-with-sdk SDKROOT="$$SDKROOT"; \
	else \
		$(MAKE) _hurry-build-mac-with-sdk; \
	fi

_hurry-build-mac-with-sdk:
	@if command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1; then \
		cargo zigbuild --release -p hurry --bin hurry --target $(HURRY_MAC_TARGET); \
	elif command -v cross >/dev/null 2>&1; then \
		cross build --release -p hurry --bin hurry --target $(HURRY_MAC_TARGET); \
	else \
		echo "ERROR: macOS cross build requires cargo-zigbuild + zig, or cross."; \
		echo "Install prerequisites, then rerun: make hurry-build-mac"; \
		exit 1; \
	fi
	@chmod +x "$(HURRY_MAC_BIN)"
	@file "$(HURRY_MAC_BIN)"

hurry-upload-linux: hurry-build-linux
	aws s3 cp "$(HURRY_LINUX_BIN)" "$(HURRY_LINUX_S3_URI)" --region "$(SCCACHE_REGION)"
	aws s3 ls "$(HURRY_LINUX_S3_URI)" --region "$(SCCACHE_REGION)"

hurry-upload-mac: hurry-build-mac
	aws s3 cp "$(HURRY_MAC_BIN)" "$(HURRY_MAC_S3_URI)" --region "$(SCCACHE_REGION)"
	aws s3 ls "$(HURRY_MAC_S3_URI)" --region "$(SCCACHE_REGION)"

hurry-upload-all: hurry-upload-linux hurry-upload-mac

sqlx-prepare:
	cargo sqlx prepare --database-url $(COURIER_DATABASE_URL) --workspace

install:
	@CARGO_HOME=$${CARGO_HOME:-$$HOME/.cargo} && \
		EXISTING_HURRY=$$(which hurry 2>/dev/null || echo "") && \
		if [ -n "$$EXISTING_HURRY" ] && [ "$$EXISTING_HURRY" != "$$CARGO_HOME/bin/hurry" ]; then \
			EXISTING_VERSION=$$($$EXISTING_HURRY --version 2>/dev/null || echo "unknown version"); \
			echo "WARNING: Found existing '$$EXISTING_VERSION' at $$EXISTING_HURRY"; \
			echo "This may conflict with the cargo-installed version at $$CARGO_HOME/bin/hurry"; \
			echo "Consider using 'make install-dev' instead to install as hurry-dev"; \
			echo ""; \
		fi
	@cargo install --path packages/hurry --locked --force
	@CARGO_HOME=$${CARGO_HOME:-$$HOME/.cargo} && \
		VERSION=$$($$CARGO_HOME/bin/hurry --version) && \
		echo "Installed '$$VERSION' to $$CARGO_HOME/bin/hurry"

install-dev:
	@cargo install --path packages/hurry --locked --force
	@CARGO_HOME=$${CARGO_HOME:-$$HOME/.cargo} && \
		mv "$$CARGO_HOME/bin/hurry" "$$CARGO_HOME/bin/hurry-dev" && \
		VERSION=$$($$CARGO_HOME/bin/hurry-dev --version) && \
		echo "Installed '$$VERSION' to $$CARGO_HOME/bin/hurry-dev"

courier-local-auth:
	@echo "Loading auth fixtures..."
	@psql -h localhost -d courier -U courier -f packages/courier/schema/fixtures/auth.sql > /dev/null
	@echo ""
	@echo "Local auth fixture loaded. Available tokens:"
	@echo "  acme-alice-token-001         (alice@acme.com, Acme Corp)"
	@echo "  acme-bob-token-001           (bob@acme.com, Acme Corp)"
	@echo "  widget-charlie-token-001     (charlie@widget.com, Widget Inc)"
	@echo ""

reset-local-cache:
	@echo "Stopping containers..."
	@docker compose down
	@echo "Clearing local data..."
	@rm -rf .hurrydata
	@echo "Starting postgres..."
	@docker compose up -d postgres
	@echo "Waiting for postgres to be ready..."
	@until docker compose exec -T postgres pg_isready -U courier > /dev/null 2>&1; do \
		sleep 0.5; \
	done
	@echo "Running migrations..."
	@cargo sqlx migrate run --source packages/courier/schema/migrations --database-url $(COURIER_DATABASE_URL)
	@$(MAKE) courier-local-auth
