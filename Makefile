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

# Local release, as in the rest of the jwa91 family: CI has no signing credentials, so every release runs here.
# Requires 1Password signed in, gh authenticated, a notarization login and a Developer ID certificate (each from
# 1Password through .env.template, or from this Mac's keychain: see scripts/codesign.sh), Cargo.toml at $(VERSION),
# and the v$(VERSION) tag at HEAD and pushed.
# Secrets come from .env.template through jwa-harden (or op run, where jwa-harden isn't installed).
# scripts/release.sh does what GoReleaser does for the family (see CLAUDE.md, Exceptions).
HARDEN := $(shell command -v jwa-harden >/dev/null 2>&1 && echo "jwa-harden run --" || echo "op run --env-file=.env.template --")

release:
	@test -n "$(VERSION)" || (echo "usage: make release VERSION=X.Y.Z" && exit 2)
	@test "$(VERSION)" = "$(VERSION_IN_CARGO)" || (echo "Cargo.toml says $(VERSION_IN_CARGO), not $(VERSION)" && exit 2)
	@op whoami >/dev/null || (echo "1Password not signed in: eval \$$(op signin)" && exit 1)
	@gh auth status >/dev/null 2>&1 || (echo "gh not authenticated: gh auth login" && exit 1)
	@grep -q '^NOTARY_PASSWORD=' .env.template || xcrun notarytool history --keychain-profile notarytool >/dev/null 2>&1 || \
	  (echo "no notarization login: add NOTARY_* to .env.template, or a keychain profile 'notarytool' (scripts/codesign.sh)"; exit 1)
	@grep -q '^MACOS_CERTIFICATE_P12_BASE64=' .env.template || \
	  security find-identity -v -p codesigning | grep -q "Developer ID Application" || \
	  (echo "no Developer ID Application certificate in this Mac's keychain, and none in .env.template (scripts/codesign.sh)"; exit 1)
	@existing=$$(git rev-parse -q --verify "v$(VERSION)^{commit}" 2>/dev/null); \
	head=$$(git rev-parse HEAD); \
	test -n "$$existing" && test "$$existing" = "$$head" || \
	  (echo "v$(VERSION) must exist and point at HEAD before release"; exit 3)
	# Build (llama.cpp, then cargo) + codesign/notarize + archive + publish + commit the Cask to the tap.
	GITHUB_TOKEN="$$(gh auth token)" $(HARDEN) scripts/release.sh $(VERSION)

# What a release would make, in dist/, without publishing (signed only when MACOS_SIGN_IDENTITY is set).
snapshot:
	scripts/release.sh --snapshot

clean:
	cargo clean
	rm -rf dist
