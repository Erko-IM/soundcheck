# `make install` rebuilds soundcheck and replaces the copy in /Applications,
# so updating after a change is one command. `make dmg` builds soundcheck.dmg
# in the repo root to hand to someone else, replacing the one before.
# Windows and Linux packages come from the Release workflow, which builds on
# those platforms' own runners.
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

# CI=true skips the step where Finder opens the half-built image to arrange
# its window, which looks like an install. The image still holds the app and
# a link to Applications, shown in Finder's default layout.
dmg: packager
	rm -f target/packages/*.dmg
	CI=true cargo packager --release --formats dmg
	mv target/packages/*.dmg soundcheck.dmg
	@echo "built $(CURDIR)/soundcheck.dmg"

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

# Installed on first use, and again whenever PACKAGER_VERSION changes.
packager:
	@cargo packager --version 2>/dev/null | grep -qx "cargo-packager $(PACKAGER_VERSION)" \
		|| cargo install cargo-packager --locked --version $(PACKAGER_VERSION)
