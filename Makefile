# `make install` rebuilds soundcheck and installs it over the copy before, so
# updating after a change is one command. `make dmg` on a Mac, `make exe` on
# Windows and `make linux` on Linux build the packages to hand to someone
# else in the repo root, replacing the ones before: soundcheck.dmg,
# soundcheck-setup.exe, or soundcheck.deb and soundcheck.AppImage. All of
# them first install anything they need that's missing: rustup (except on
# Windows), the Rust version rust-toolchain.toml names, and cargo-packager.
#
# On Windows, set up Git Bash, make, rustup and Microsoft's C++ build tools
# once, then run make in Git Bash:
#   winget install --id Git.Git -e
#   winget install --id ezwinports.make -e
#   winget install --id Rustlang.Rustup -e
#   winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
# On Linux, set up a C compiler, curl, file and ALSA's headers once, on
# Debian or Ubuntu with:
#   sudo apt-get install -y build-essential curl file pkg-config libasound2-dev
PACKAGER_VERSION := 0.11.8
APPS ?= /Applications
# A code-signing identity from your keychain. Without one, every build is a
# new app to macOS, and it asks again for access to Documents and to memory
# cards after each install.
SIGN ?=

ifeq ($(OS),Windows_NT)
SYSTEM := Windows
else ifeq ($(shell uname -s),Darwin)
SYSTEM := macOS
else
SYSTEM := Linux
endif

# Where rustup's commands are, which the rust target installs when they're
# missing. Named in full because the make macOS ships finds a command by name
# only in the PATH it started with, which a fresh install isn't on. First on
# the PATH too, for the cargo that cargo-packager runs. Empty on Windows,
# where rustup's installer puts them on the PATH itself, and where make gets
# the PATH in Windows form, which adding to it here would break.
ifneq ($(SYSTEM),Windows)
CARGO_BIN := $(or $(CARGO_HOME),$(HOME)/.cargo)/bin/
export PATH := $(CARGO_BIN):$(PATH)
endif

.PHONY: check packager rust

ifeq ($(SYSTEM),macOS)
.PHONY: app install dmg

# Signed ad hoc, which seals the whole app: without it a downloaded copy is
# "damaged". Here rather than by cargo-packager, which then warns on every
# build that it could not notarize the app, and never will.
app: packager
	$(CARGO_BIN)cargo packager --release --formats app
	xattr -cr target/packages/soundcheck.app
	codesign --force --sign - --options runtime target/packages/soundcheck.app

install: app
	rm -rf "$(APPS)/soundcheck.app"
	ditto target/packages/soundcheck.app "$(APPS)/soundcheck.app"
	$(if $(SIGN),codesign --force --sign "$(SIGN)" "$(APPS)/soundcheck.app")
	@echo "installed $(APPS)/soundcheck.app"

# A plain image around the signed app. cargo-packager's dmg format signs the
# image as well, and macOS blocks a downloaded image with an ad-hoc signature
# outright; unsigned, it opens and macOS checks only the app. diskutil image
# from macOS 26 on, which deprecates hdiutil; hdiutil before that, as on the
# Release workflow's runner.
dmg: app
	rm -rf target/dmg
	mkdir target/dmg
	ditto target/packages/soundcheck.app target/dmg/soundcheck.app
	ln -s /Applications target/dmg/Applications
	if /usr/sbin/diskutil image create --help >/dev/null 2>&1; then \
		/usr/sbin/diskutil image create from --format UDZO --volumeName soundcheck target/dmg soundcheck.dmg; \
	else \
		hdiutil create -volname soundcheck -srcfolder target/dmg -fs HFS+ -format UDZO -ov soundcheck.dmg; \
	fi
	@echo "built $(CURDIR)/soundcheck.dmg"
endif

ifeq ($(SYSTEM),Windows)
.PHONY: exe install

# cargo-packager puts the version in the installer's name. The copy is named
# the same every time, for the Release workflow to publish and install.ps1 to
# fetch; older versions' installers go first so the copy finds only this one.
exe: packager
	rm -f target/packages/*-setup.exe
	$(CARGO_BIN)cargo packager --release --formats nsis
	cp target/packages/*-setup.exe soundcheck-setup.exe
	@echo "built $(CURDIR)/soundcheck-setup.exe"

# The installer run silently: for you alone, so without asking for an
# administrator, into AppData\Local\soundcheck with a Start menu shortcut,
# closing soundcheck first if it's open. Settings, Apps removes it again.
# MSYS_NO_PATHCONV stops Git Bash from turning /S into a path.
install: export MSYS_NO_PATHCONV := 1
install: exe
	./soundcheck-setup.exe /S
	@echo "installed soundcheck, it's in the Start menu"
endif

ifeq ($(SYSTEM),Linux)
.PHONY: linux install

# Copied to names that stay the same, as on Windows, for install.sh. The
# AppImage tools cargo-packager fetches are AppImages themselves, which this
# has unpack themselves to run rather than need FUSE.
linux: export APPIMAGE_EXTRACT_AND_RUN := 1
linux: packager
	rm -f target/packages/*.deb target/packages/*.AppImage
	$(CARGO_BIN)cargo packager --release --formats deb,appimage
	cp target/packages/*.deb soundcheck.deb
	cp target/packages/*.AppImage soundcheck.AppImage
	@echo "built $(CURDIR)/soundcheck.deb and $(CURDIR)/soundcheck.AppImage"

# The same install as the one-line one, from the AppImage just built.
install: linux
	sh install.sh soundcheck.AppImage
endif

check: rust
	$(CARGO_BIN)cargo fmt --check
	$(CARGO_BIN)cargo clippy --all-targets -- -D warnings
	$(CARGO_BIN)cargo test

# Installed on first use, and again whenever PACKAGER_VERSION changes.
packager: rust
	@$(CARGO_BIN)cargo packager --version 2>/dev/null | grep -qx "cargo-packager $(PACKAGER_VERSION)" \
		|| $(CARGO_BIN)cargo install cargo-packager --locked --version $(PACKAGER_VERSION)

# rustup goes into ~/.cargo and ~/.rustup and leaves your shell's startup
# files alone. The first rustc run then fetches the version
# rust-toolchain.toml names. `~/.cargo/bin/rustup self uninstall` removes
# all of it again. Not on Windows, where the rustup-init.sh this fetches
# would set Rust up for MinGW rather than Microsoft's build tools.
rust:
ifeq ($(SYSTEM),Windows)
	@command -v rustup >/dev/null \
		|| { echo "rustup is missing: winget install --id Rustlang.Rustup -e, then open a new Git Bash" >&2; exit 1; }
else
	@test -x $(CARGO_BIN)rustup \
		|| curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
endif
	@$(CARGO_BIN)rustc --version
