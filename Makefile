# `make install` rebuilds soundcheck and installs it over the copy before, so
# updating after a change is one command. `make dmg`, `make exe` and `make
# linux` build the packages to hand to someone else in the repo root,
# replacing the ones before: soundcheck.dmg, soundcheck-setup.exe, and
# soundcheck.deb plus soundcheck.AppImage; `make packages` builds all four on
# a Mac, and `make release` raises the version, builds them and publishes
# them as a GitHub release. They first install anything they need that's
# missing: rustup (except on Windows), the Rust version rust-toolchain.toml
# names, and cargo-packager.
#
# `make ci` runs every check: formatting and lints, the tests on each kind of
# machine a Mac reaches (Apple silicon, Intel under Rosetta, and Linux and the
# Windows build's lints in the container), the dependencies against
# deny.toml, the workflows and install.sh. `make hooks` has the git hook run,
# before each push, the ones for what the push changes (lefthook.yml). Each
# check installs what it takes that's missing, the same way: the tools in
# mise.toml through mise, mise too, and on a Mac Rosetta, and Docker Desktop
# started for the container.
#
# The dmg needs a Mac. The exe builds on Windows, Linux or a Mac, and the
# Linux packages on Linux or a Mac. A Mac builds those two in a Linux
# container, so Docker Desktop has to be running for them.
#
# On Windows, set up Git Bash, make, rustup and Microsoft's C++ build tools
# once, then run make in Git Bash:
#   winget install --id Git.Git -e
#   winget install --id ezwinports.make -e
#   winget install --id Rustlang.Rustup -e
#   winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
# On Linux, set up a C compiler, curl, file and ALSA's headers once, and
# MinGW and NSIS for the exe, on Debian or Ubuntu with:
#   sudo apt-get install -y build-essential curl file pkg-config libasound2-dev gcc-mingw-w64-x86-64 nsis
PACKAGER_VERSION := 0.11.8
APPS ?= /Applications

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

.PHONY: check fmt packager rust tools ci ci.rust ci.lint ci.deps ci.workflows ci.scripts ci.committed hooks

ifeq ($(SYSTEM),macOS)
.PHONY: app install dmg exe linux packages builder docker release ci.test.mac ci.test.linux ci.test.windows

# A code-signing identity from your keychain to sign the app with, the same
# for every build: one named soundcheck when the keychain has it. A
# self-signed one is free (Keychain Access, Certificate Assistant, Create a
# Certificate, type Code Signing). macOS then knows each build, a release
# too, as the same app, and the folders and cards it was let into stay open
# to it through updates. Without one each build is signed ad hoc, a new app
# to macOS, which asks again.
SIGN ?= $(shell security find-identity -p codesigning 2>/dev/null | grep -qF '"soundcheck"' && echo soundcheck)

# Signed with SIGN, or ad hoc, which seals the whole app: without it a
# downloaded copy is "damaged". Here rather than by cargo-packager, which then
# warns on every build that it could not notarize the app, and never will.
# Once installed or put in the image, the app built goes: Finder opens
# recordings with the newest soundcheck.app it has seen, which one left in
# target would be, rather than the one installed.
app: packager
	$(CARGO_BIN)cargo packager --release --formats app
	xattr -cr target/packages/soundcheck.app
	codesign --force --sign "$(or $(SIGN),-)" --options runtime target/packages/soundcheck.app

install: app
	rm -rf "$(APPS)/soundcheck.app"
	ditto target/packages/soundcheck.app "$(APPS)/soundcheck.app"
	rm -rf target/packages/soundcheck.app
	@echo "installed $(APPS)/soundcheck.app"

# Rust's name for the other kind of Mac: Intel on Apple silicon, Apple silicon
# on Intel. target/release is built for this Mac's own kind. Worked out only
# when a recipe uses it, once the rust target has made sure rustc is there.
OTHER_MAC_TARGET = $(filter-out $(shell $(CARGO_BIN)rustc -vV | sed -n 's/^host: //p'),aarch64-apple-darwin x86_64-apple-darwin)

# A plain image around the signed app, its binary joined in here with one
# built for the other kind of Mac, so one download runs on Apple silicon and
# Intel alike, while `make install` builds only for this Mac. cargo-packager's
# dmg format signs the image as well, and macOS blocks a downloaded image with
# an ad-hoc signature outright; unsigned, it opens and macOS checks only the
# app. diskutil image where it can name the volume, as from macOS 26 on, which
# deprecates hdiutil; hdiutil elsewhere. The Packages workflow's macOS 15
# runner has diskutil image, but without --volumeName.
dmg: app
	@$(CARGO_BIN)rustup target list --installed | grep -qx $(OTHER_MAC_TARGET) \
		|| $(CARGO_BIN)rustup target add $(OTHER_MAC_TARGET)
	$(CARGO_BIN)cargo build --release --target $(OTHER_MAC_TARGET)
	rm -rf target/dmg
	mkdir target/dmg
	ditto target/packages/soundcheck.app target/dmg/soundcheck.app
	lipo -create -output target/dmg/soundcheck.app/Contents/MacOS/soundcheck \
		target/release/soundcheck target/$(OTHER_MAC_TARGET)/release/soundcheck
	codesign --force --sign "$(or $(SIGN),-)" --options runtime target/dmg/soundcheck.app
	ln -s /Applications target/dmg/Applications
	if /usr/sbin/diskutil image create from --help 2>/dev/null | grep -q -- --volumeName; then \
		/usr/sbin/diskutil image create from --format UDZO --volumeName soundcheck target/dmg soundcheck.dmg; \
	else \
		hdiutil create -volname soundcheck -srcfolder target/dmg -fs HFS+ -format UDZO -ov soundcheck.dmg; \
	fi
	rm -rf target/dmg target/packages/soundcheck.app
	@echo "built $(CURDIR)/soundcheck.dmg"

# cargo-packager builds the Linux packages on Linux only, so these run the
# Linux targets below in a container from packaging/Dockerfile, as do the
# checks on Linux and of the Windows build. Its Rust, cargo-packager and
# build caches stay between runs in the Docker volume soundcheck-build,
# rather than in the repo, where macOS's file times would have part of the
# Windows build redone every time; `docker volume rm soundcheck-build` clears
# them. A cache for each, as they'd rebuild much of a shared one for each
# other.
exe linux ci.test.linux ci.test.windows: builder
	docker run --rm --volume "$(CURDIR)":/src --volume soundcheck-build:/build --workdir /src \
		--env CARGO_HOME=/build/cargo --env RUSTUP_HOME=/build/rustup \
		--env CARGO_TARGET_DIR=/build/$@ --env XDG_CACHE_HOME=/build/cache \
		soundcheck-builder make $@

builder: docker
	docker build --tag soundcheck-builder packaging

# Docker Desktop, started if it isn't running, and waited for.
docker:
	@docker info >/dev/null 2>&1 && exit 0; \
	test -d /Applications/Docker.app \
		|| { echo "Docker Desktop is missing: https://www.docker.com/products/docker-desktop" >&2; exit 1; }; \
	echo "starting Docker Desktop"; open -a Docker; \
	for i in $$(seq 60); do sleep 1; docker info >/dev/null 2>&1 && exit 0; done; \
	echo "Docker Desktop didn't start within a minute" >&2; exit 1

packages: dmg exe linux

# The tests on this Mac, and on Apple silicon the Intel build's too, under
# Rosetta told to show AVX2 and the rest of its generation, which it hides
# otherwise: so the second copies of the hot code (src/cpu.rs) run as well,
# and SOUNDCHECK_V3 has a test fail if they don't.
ci.test.mac: rust
	$(CARGO_BIN)cargo test --locked
ifeq ($(shell uname -m),arm64)
	@arch -x86_64 /usr/bin/true 2>/dev/null || softwareupdate --install-rosetta --agree-to-license \
		|| { echo "Rosetta didn't install: sudo softwareupdate --install-rosetta --agree-to-license" >&2; exit 1; }
	@$(CARGO_BIN)rustup target list --installed | grep -qx x86_64-apple-darwin \
		|| $(CARGO_BIN)rustup target add x86_64-apple-darwin
	env SOUNDCHECK_V3=1 ROSETTA_ADVERTISE_AVX=1 $(CARGO_BIN)cargo test --locked --target x86_64-apple-darwin
endif

ci.rust: ci.lint ci.test.mac ci.test.linux ci.test.windows

# Builds the four packages and publishes them as a GitHub release, which
# install.sh and install.ps1 download from, named after the version in
# Cargo.toml. Once that version is out, it goes up first: the middle number
# by default, 0.2.0 to 0.3.0; BUMP=patch raises the last, to 0.2.1, and
# BUMP=major the first, to 1.0.0. VERSION=1.2.3 sets it, out or not. The new
# version is committed and pushed on its own, as the release tags the commit
# it's built from. One that isn't out yet, set by hand or left by a release
# that stopped partway, goes out as it is. Only from a clean branch that's
# pushed. The notes open with packaging/release-notes.md, ahead of GitHub's
# list of changes. Needs the GitHub CLI, gh, logged in.
BUMP ?= minor
release: rust
	@$(CARGO_BIN)cargo metadata --format-version 1 >/dev/null
	@test -z "$$(git status --porcelain)" \
		|| { echo "commit your changes first, Cargo.lock too" >&2; exit 1; }
	@test -n "$$(git branch -r --contains HEAD)" || { echo "push first" >&2; exit 1; }
	@case "$(BUMP)" in major|minor|patch) ;; *) echo "BUMP is major, minor or patch" >&2; exit 1;; esac; \
	test -z "$(VERSION)" || echo "$(VERSION)" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' \
		|| { echo "VERSION is a version like 1.2.3" >&2; exit 1; }; \
	current=$$(sed -n 's/^version = "\(.*\)"$$/\1/p' Cargo.toml | head -n 1); \
	git ls-remote --exit-code --tags origin "v$$current" >/dev/null; \
	case $$? in 0) out=yes;; 2) out=;; *) echo "cannot see the releases on origin" >&2; exit 1;; esac; \
	next="$(VERSION)"; \
	if test -z "$$out" && { test -z "$$next" || test "$$next" = "$$current"; }; then \
		echo "v$$current isn't out yet, so it goes out as it is"; \
		test "$(origin BUMP)" != "command line" \
			|| echo "BUMP raises only a version that's out, and VERSION=1.2.3 sets one"; \
		exit 0; \
	fi; \
	test -n "$$next" || next=$$(echo "$$current" | awk -F. -v bump="$(BUMP)" \
		'bump == "major" { print $$1 + 1 ".0.0" } bump == "minor" { print $$1 "." $$2 + 1 ".0" } bump == "patch" { print $$1 "." $$2 "." $$3 + 1 }'); \
	! git ls-remote --exit-code --tags origin "v$$next" >/dev/null \
		|| { echo "v$$next is out already" >&2; exit 1; }; \
	echo "v$$current becomes v$$next"; \
	sed -i '' "1,/^version = /s/^version = \".*\"/version = \"$$next\"/" Cargo.toml \
		&& $(CARGO_BIN)cargo metadata --format-version 1 >/dev/null \
		&& git commit --quiet -m "v$$next" Cargo.toml Cargo.lock \
		&& git push --quiet
	$(MAKE) packages
	@version=$$(sed -n 's/^version = "\(.*\)"$$/\1/p' Cargo.toml | head -n 1); \
	gh release create "v$$version" soundcheck.dmg soundcheck-setup.exe soundcheck.deb soundcheck.AppImage \
		--target "$$(git rev-parse HEAD)" --title "v$$version" --notes-file packaging/release-notes.md --generate-notes
endif

ifeq ($(SYSTEM),Windows)
.PHONY: exe install

# cargo-packager puts the version in the installer's name. The copy is named
# the same every time, for `make release` to publish and install.ps1 to
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

.PHONY: ci.test.windows
ci.test.windows: rust
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked

ci.rust: ci.lint ci.test.windows
endif

ifeq ($(SYSTEM),Linux)
.PHONY: exe linux install

# Rust's name for the PCs the Linux packages are for: this one, unless it's
# the container a Mac builds them in, which asks for x86_64.
LINUX_TARGET ?= $(shell uname -m)-unknown-linux-gnu
LINUX_ARCH := $(firstword $(subst -, ,$(LINUX_TARGET)))
BUILD := $(or $(CARGO_TARGET_DIR),target)

# appimagetool, and the runtime every AppImage starts with, pinned and checked
# against these.
APPIMAGETOOL_VERSION := 1.9.1
APPIMAGE_RUNTIME_VERSION := 20251108
sha256_appimagetool-x86_64 := ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
sha256_appimagetool-aarch64 := f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
sha256_runtime-x86_64 := 2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
sha256_runtime-aarch64 := 00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
HOST_ARCH := $(shell uname -m)
APPIMAGETOOL := $(BUILD)/tools/appimagetool-$(APPIMAGETOOL_VERSION)-$(HOST_ARCH).AppImage
APPIMAGE_RUNTIME := $(BUILD)/tools/runtime-$(APPIMAGE_RUNTIME_VERSION)-$(LINUX_ARCH)
# $(call fetch,url,sha256) downloads url to $@, if what arrives has that sha256.
fetch = mkdir -p $(@D) && curl -fsSL -o $@.part $(1) && echo "$(2)  $@.part" | sha256sum -c --quiet && mv $@.part $@

# Cross-built with MinGW, as Microsoft's tools run on Windows only, and
# named as the Windows build names it. cargo-packager warns twice that it can
# sign the exe only on Windows; it isn't signed there either. LAME, which
# writes MP3s, configures itself for the machine it builds on unless
# MP3LAME_SYS_OVERRIDE_HOST names another; so here, and for the Linux
# packages below, it is told what the packages are for.
exe: packager
	@$(CARGO_BIN)rustup target list --installed | grep -qx x86_64-pc-windows-gnu \
		|| $(CARGO_BIN)rustup target add x86_64-pc-windows-gnu
	rm -f target/packages/*-setup.exe
	env CARGO_BUILD_TARGET=x86_64-pc-windows-gnu MP3LAME_SYS_OVERRIDE_HOST=x86_64-w64-mingw32 \
		$(CARGO_BIN)cargo packager --release --formats nsis --target x86_64-pc-windows-gnu
	cp target/packages/*-setup.exe soundcheck-setup.exe
	@echo "built $(CURDIR)/soundcheck-setup.exe"

# The deb from cargo-packager and the AppImage from appimagetool, both under
# names that stay the same, as on Windows. appimagetool because the tools
# cargo-packager builds an AppImage with run only on the kind of PC it's
# for; an AppImage itself, it unpacks itself to run rather than need FUSE.
# Nothing to bundle into it: soundcheck needs only ALSA and the C library,
# which every desktop has, as the deb says. No AppStream listing for
# software centres to check either, and appimagetool's squashfs statistics,
# on stdout, left out.
linux: export APPIMAGE_EXTRACT_AND_RUN := 1
linux: packager $(APPIMAGETOOL) $(APPIMAGE_RUNTIME)
	@$(CARGO_BIN)rustup target list --installed | grep -qx $(LINUX_TARGET) \
		|| $(CARGO_BIN)rustup target add $(LINUX_TARGET)
	rm -f target/packages/*.deb
	env CARGO_BUILD_TARGET=$(LINUX_TARGET) MP3LAME_SYS_OVERRIDE_HOST=$(LINUX_ARCH)-linux-gnu \
		$(CARGO_BIN)cargo packager --release --formats deb --target $(LINUX_TARGET)
	cp target/packages/*.deb soundcheck.deb
	rm -rf $(BUILD)/AppDir
	mkdir -p $(BUILD)/AppDir/usr/bin
	cp $(BUILD)/$(LINUX_TARGET)/release/soundcheck $(BUILD)/AppDir/usr/bin/
	cp packaging/soundcheck.desktop $(BUILD)/AppDir/
	cp packaging/icon.png $(BUILD)/AppDir/soundcheck.png
	ln -s usr/bin/soundcheck $(BUILD)/AppDir/AppRun
	env ARCH=$(LINUX_ARCH) $(APPIMAGETOOL) --no-appstream --runtime-file $(APPIMAGE_RUNTIME) $(BUILD)/AppDir soundcheck.AppImage >/dev/null
	@echo "built $(CURDIR)/soundcheck.deb and $(CURDIR)/soundcheck.AppImage"

$(APPIMAGETOOL):
	$(call fetch,https://github.com/AppImage/appimagetool/releases/download/$(APPIMAGETOOL_VERSION)/appimagetool-$(HOST_ARCH).AppImage,$(sha256_appimagetool-$(HOST_ARCH)))
	chmod +x $@

$(APPIMAGE_RUNTIME):
	$(call fetch,https://github.com/AppImage/type2-runtime/releases/download/$(APPIMAGE_RUNTIME_VERSION)/runtime-$(LINUX_ARCH),$(sha256_runtime-$(LINUX_ARCH)))

# The same install as the one-line one, from the AppImage just built.
install: linux
	sh install.sh soundcheck.AppImage

.PHONY: ci.test.linux ci.test.windows

# The lints and tests on Linux. Under root, as in a Mac's container, the tests
# run as an ordinary user, whom a folder shut to everyone keeps out.
ci.test.linux: rust
	$(CARGO_BIN)cargo clippy --all-targets --locked -- -D warnings
ifeq ($(shell id -u),0)
	@built=$$($(CARGO_BIN)cargo test --locked --no-run --message-format=json) || exit 1; \
	tests=$$(printf '%s\n' "$$built" | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p'); \
	test -n "$$tests" || { echo "no tests were built" >&2; exit 1; }; \
	for t in $$tests; do su nobody -s /bin/sh -c "$$t" || exit 1; done
else
	$(CARGO_BIN)cargo test --locked
endif

# The Windows build's lints, through MinGW as `make exe` builds it. Its tests
# run on Windows only, in the CI workflow.
ci.test.windows: rust
	@$(CARGO_BIN)rustup target list --installed | grep -qx x86_64-pc-windows-gnu \
		|| $(CARGO_BIN)rustup target add x86_64-pc-windows-gnu
	env MP3LAME_SYS_OVERRIDE_HOST=x86_64-w64-mingw32 \
		$(CARGO_BIN)cargo clippy --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings

ci.rust: ci.lint ci.test.linux ci.test.windows
endif

# Each check is a target, run alike three ways: before a push by the git hook,
# the ones for what the push changes (lefthook.yml); all at once by `make ci`;
# and on each platform's own runner by the CI workflow, run by hand
# (.github/workflows/ci.yml).

# mise from the PATH, or where its installer puts it.
MISE := $(or $(shell command -v mise),$(HOME)/.local/bin/mise)

# The tools mise.toml names, mise itself first if it's missing: installed once,
# and again whenever mise.toml changes a version.
tools:
	@command -v $(MISE) >/dev/null || curl -fsSL https://mise.run | sh
	@$(MISE) trust -q
	@$(MISE) install -q

ci: ci.workflows ci.scripts ci.deps ci.rust

ci.lint: rust
	$(CARGO_BIN)cargo fmt --check
	$(CARGO_BIN)cargo clippy --all-targets --locked -- -D warnings

# What the dependencies may be, as deny.toml says, and none left unused.
ci.deps: rust tools
	$(MISE) x -- cargo deny check
	$(MISE) x -- cargo machete

ci.workflows: tools
	$(MISE) x -- actionlint

ci.scripts: tools
	$(MISE) x -- shellcheck install.sh

# A push is checked from the files here, so they have to be what it pushes:
# nothing changed or added since the last commit.
ci.committed:
	@test -z "$$(git status --porcelain)" || { git status --short; \
		echo "commit or stash those first, as the checks would test them rather than what's being pushed (LEFTHOOK=0 git push skips the checks)" >&2; \
		exit 1; }

# The git hook lefthook.yml describes, once per clone.
hooks: tools
	$(MISE) x -- lefthook install

# The quick round while working: formatting, lints and this machine's tests.
check: ci.lint
	$(CARGO_BIN)cargo test --locked

# The Rust formatted, as the git hook does before each commit.
fmt: rust
	$(CARGO_BIN)cargo fmt

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
