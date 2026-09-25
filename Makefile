# `make install` rebuilds soundcheck and replaces the copy in /Applications,
# so updating after a change is one command. `make dmg` builds soundcheck.dmg
# in the repo root to hand to someone else, replacing the one before. Both
# first install anything they need that's missing: rustup, the Rust version
# rust-toolchain.toml names, and cargo-packager.
# Windows and Linux packages come from the Release workflow, which builds on
# those platforms' own runners.
PACKAGER_VERSION := 0.11.8
APPS ?= /Applications
# A code-signing identity from your keychain. Without one, every build is a
# new app to macOS, and it asks again for access to Documents and to memory
# cards after each install.
SIGN ?=
# rustup's commands, which the rust target installs when they're missing.
# Named in full because the make macOS ships finds a command by name only in
# the PATH it started with, which a fresh install isn't on. First on the PATH
# too, for the cargo that cargo-packager runs.
CARGO_BIN := $(or $(CARGO_HOME),$(HOME)/.cargo)/bin
export PATH := $(CARGO_BIN):$(PATH)

.PHONY: install dmg check packager rust

install: packager
	$(CARGO_BIN)/cargo packager --release --formats app
	rm -rf "$(APPS)/soundcheck.app"
	ditto target/packages/soundcheck.app "$(APPS)/soundcheck.app"
	$(if $(SIGN),codesign --force --sign "$(SIGN)" "$(APPS)/soundcheck.app")
	@echo "installed $(APPS)/soundcheck.app"

# A plain image around the signed app. cargo-packager's dmg format signs the
# image as well, and macOS blocks a downloaded image with an ad-hoc signature
# outright; unsigned, it opens and macOS checks only the app. hdiutil, not
# diskutil image, because that only exists from macOS 26.
dmg: packager
	$(CARGO_BIN)/cargo packager --release --formats app
	rm -rf target/dmg
	mkdir target/dmg
	ditto target/packages/soundcheck.app target/dmg/soundcheck.app
	ln -s /Applications target/dmg/Applications
	hdiutil create -volname soundcheck -srcfolder target/dmg -fs HFS+ -format UDZO -ov soundcheck.dmg
	@echo "built $(CURDIR)/soundcheck.dmg"

check: rust
	$(CARGO_BIN)/cargo fmt --check
	$(CARGO_BIN)/cargo clippy --all-targets -- -D warnings
	$(CARGO_BIN)/cargo test

# Installed on first use, and again whenever PACKAGER_VERSION changes.
packager: rust
	@$(CARGO_BIN)/cargo packager --version 2>/dev/null | grep -qx "cargo-packager $(PACKAGER_VERSION)" \
		|| $(CARGO_BIN)/cargo install cargo-packager --locked --version $(PACKAGER_VERSION)

# rustup goes into ~/.cargo and ~/.rustup and leaves your shell's startup
# files alone. The first rustc run then fetches the version
# rust-toolchain.toml names. `~/.cargo/bin/rustup self uninstall` removes
# all of it again.
rust:
	@test -x $(CARGO_BIN)/rustup \
		|| curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
	@$(CARGO_BIN)/rustc --version
