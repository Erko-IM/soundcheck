#!/bin/sh
# Installs soundcheck from the latest release, or updates it:
#   curl -fsSL https://raw.githubusercontent.com/Erko-IM/soundcheck/main/install.sh | sh
# `sh install.sh <package>` installs a .dmg or .AppImage you have instead,
# as `make install` does on Linux.
#
# A Mac marks only what a browser or the like downloads as from the
# internet, so the app this puts in /Applications opens without the Privacy
# & Security detour. On Linux the AppImage is unpacked into ~/.local rather
# than run as it is, which would need FUSE.
set -eu

releases=https://github.com/Erko-IM/soundcheck/releases/latest/download

die() {
	echo "install.sh: $*" >&2
	exit 1
}

cleanup() {
	if [ -n "${mounted:-}" ]; then
		hdiutil detach -quiet "$mounted" || true
	fi
	rm -rf "$tmp"
}

# Puts the latest release's $1 in $tmp, or the package given instead.
fetch() {
	if [ -n "$given" ]; then
		cp "$given" "$tmp/$1"
	else
		curl -fL --progress-bar -o "$tmp/$1" "$releases/$1" || die "couldn't download $releases/$1"
	fi
}

mac() {
	fetch soundcheck.dmg
	hdiutil attach -quiet -nobrowse -readonly -mountpoint "$tmp/volume" "$tmp/soundcheck.dmg"
	mounted=$tmp/volume
	if [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" != 1 ] \
		&& ! file "$mounted/soundcheck.app/Contents/MacOS/soundcheck" | grep -q x86_64; then
		die "this soundcheck is for Apple silicon Macs only"
	fi
	apps=/Applications
	[ -w "$apps" ] || apps=$HOME/Applications
	mkdir -p "$apps"
	rm -rf "$apps/soundcheck.app"
	ditto "$mounted/soundcheck.app" "$apps/soundcheck.app"
	echo "installed $apps/soundcheck.app"
}

linux() {
	if [ -z "$given" ] && [ "$(uname -m)" != x86_64 ]; then
		die "the releases have soundcheck for x86_64 Linux only"
	fi
	fetch soundcheck.AppImage
	chmod +x "$tmp/soundcheck.AppImage"
	(cd "$tmp" && ./soundcheck.AppImage --appimage-extract >/dev/null)
	data=${XDG_DATA_HOME:-$HOME/.local/share}
	app=$data/soundcheck
	rm -rf "$app"
	mkdir -p "$data" "$data/applications" "$HOME/.local/bin"
	mv "$tmp/squashfs-root" "$app"
	ln -sf "$app/AppRun" "$HOME/.local/bin/soundcheck"
	sed -e "s|^Exec=[^ ]*|Exec=\"$app/AppRun\"|" -e "s|^Icon=.*|Icon=$app/soundcheck.png|" \
		"$app/soundcheck.desktop" >"$data/applications/soundcheck.desktop"
	if command -v update-desktop-database >/dev/null; then
		update-desktop-database -q "$data/applications" || true
	fi
	echo "installed $app, it's among your apps and ~/.local/bin/soundcheck starts it too"
}

# Everything runs from here, called on the last line, so a download cut
# short runs nothing.
main() {
	given=${1:-}
	tmp=$(mktemp -d)
	trap cleanup EXIT
	trap 'exit 1' INT TERM
	case $(uname -s) in
	Darwin) mac ;;
	Linux) linux ;;
	*) die "no soundcheck for $(uname -s) here; on Windows, install.ps1 does this" ;;
	esac
}

main "$@"
