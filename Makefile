# Makefile - front door for building, testing and installing cronie-rs
#
# 20261111 - Created by Claude (claude.ai, Anthropic) - scottfk
#
#   make              release build (target/release/)
#   make reference    build the C reference at tools/REFERENCE_TAG into ../cronie
#   make check        build the reference if needed, then run every test
#   make clippy       cargo clippy --all-targets -D warnings
#   make fmt          cargo fmt

CARGO     ?= cargo
REFERENCE ?= ../cronie

.PHONY: all build reference check test clippy fmt clean

all: build

build:
	$(CARGO) build --release

reference:
	@if [ ! -x $(REFERENCE)/src/crond ]; then tools/build-reference.sh $(REFERENCE); fi

check: reference
	CRONIE_REFERENCE=$(abspath $(REFERENCE)) $(CARGO) test

test: check

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt

clean:
	$(CARGO) clean
