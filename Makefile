.PHONY: llama build install dev check selftest testbed eval release snapshot clean

BIN_DIR ?= $(HOME)/.local/bin
LLAMA := vendor/llama-build/TAG
VERSION_IN_CARGO := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# llama.cpp at its pinned release, static, with Metal (a minute or two, once).
$(LLAMA):
	scripts/build-llama.sh

llama: $(LLAMA)

build: $(LLAMA)
	cargo build --release

install: build
	install -d $(BIN_DIR)
	install -m 0755 target/release/sift-it $(BIN_DIR)/sift-it
	@echo "installed sift-it to $(BIN_DIR)/sift-it"
	@echo "ensure $(BIN_DIR) is on your PATH"

dev: install

check: $(LLAMA)
	cargo fmt --check
	cargo clippy --release -- -D warnings
	cargo test --release

# The vectors this build makes, against the reference (needs the model: sift-it model download).
selftest: build
	target/release/sift-it selftest

testbed:
	scripts/make-testbed.sh

# Search quality on the testbed, in an index of its own (needs the model).
eval: build testbed
	SIFT_IT_HOME=$(CURDIR)/testbed/.sift SIFT_IT_MODELS="$(HOME)/Library/Application Support/Sift It/models" \
	  sh -c 'target/release/sift-it folders add testbed >/dev/null && target/release/sift-it config include-code true >/dev/null \
	  && target/release/sift-it index >/dev/null 2>&1 && target/release/sift-it eval evals/evals.json'

# A release, from this Mac: built, signed and notarized with its keychain (scripts/codesign.sh says what it needs),
# published as a GitHub release, and the cask committed to jwa91/tap, both with your gh login.
# Requires Cargo.toml at $(VERSION) and the v$(VERSION) tag at HEAD, pushed. Nothing is built or published until every
# check passes.
release:
	@test -n "$(VERSION)" || (echo "usage: make release VERSION=X.Y.Z" && exit 2)
	@test "$(VERSION)" = "$(VERSION_IN_CARGO)" || (echo "Cargo.toml says $(VERSION_IN_CARGO), not $(VERSION)" && exit 2)
	@gh auth status >/dev/null 2>&1 || (echo "gh not authenticated: gh auth login" && exit 1)
	@scripts/codesign.sh --check
	@existing=$$(git rev-parse -q --verify "v$(VERSION)^{commit}" 2>/dev/null); \
	head=$$(git rev-parse HEAD); \
	test -n "$$existing" && test "$$existing" = "$$head" || \
	  (echo "v$(VERSION) must exist and point at HEAD before release"; exit 3)
	scripts/release.sh $(VERSION)

# What a release would make, in dist/, unsigned and unpublished (CI runs this too).
snapshot:
	scripts/release.sh --snapshot

clean:
	cargo clean
	rm -rf dist
