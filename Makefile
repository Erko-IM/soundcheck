# `make install` rebuilds soundcheck and replaces the copy in /Applications,
# so updating after a change is one command. `make dmg` builds an image to
# hand to someone else. Windows and Linux packages come from the Release
# workflow, which builds on those platforms' own runners.
PACKAGER_VERSION := 0.11.8
APPS ?= /Applications
# A code-signing identity from your keychain. Without one, every build is a
# new app to macOS, and it asks again for access to Documents and to memory
# cards after each install.
SIGN ?=

.PHONY: install dmg check packager

install: packager
	cargo packager --release --formats app
	rm -rf "$(APPS)/soundcheck.app"
	ditto target/packages/soundcheck.app "$(APPS)/soundcheck.app"
	$(if $(SIGN),codesign --force --sign "$(SIGN)" "$(APPS)/soundcheck.app")
	@echo "installed $(APPS)/soundcheck.app"

dmg: packager
	cargo packager --release --formats dmg
	@ls -1 target/packages/*.dmg

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

# Installed on first use, and again whenever PACKAGER_VERSION changes.
packager:
	@cargo packager --version 2>/dev/null | grep -qx "cargo-packager $(PACKAGER_VERSION)" \
		|| cargo install cargo-packager --locked --version $(PACKAGER_VERSION)
