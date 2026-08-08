#!/bin/sh
# Install a prebuilt wiff binary from a GitHub release. The release defaults to
# the rolling `continuous` build; set WIFF_VERSION to install from a different
# release. The binary is placed in ~/.local/bin unless WIFF_BIN_DIR is set.
set -eu

repo="wez/wiff"
version="${WIFF_VERSION:-continuous}"
bin_dir="${WIFF_BIN_DIR:-$HOME/.local/bin}"

for tool in curl tar mktemp; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "wiff: $tool is required" >&2
		exit 1
	fi
done

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
Linux)
	case "$arch" in
	x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
	aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
	*) echo "wiff: unsupported architecture: $arch" >&2; exit 1 ;;
	esac
	;;
Darwin)
	case "$arch" in
	x86_64 | amd64) target="x86_64-apple-darwin" ;;
	arm64 | aarch64) target="aarch64-apple-darwin" ;;
	*) echo "wiff: unsupported architecture: $arch" >&2; exit 1 ;;
	esac
	;;
*)
	echo "wiff: unsupported operating system: $os" >&2
	exit 1
	;;
esac

archive="wiff-${target}.tar.gz"
url="https://github.com/${repo}/releases/download/${version}/${archive}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "wiff: downloading $url"
curl -fSL "$url" -o "$tmp/$archive"
curl -fSL "$url.sha256" -o "$tmp/$archive.sha256"

# This sha check guards against a corrupted or truncated download only;
# the checksum shares the release channel with the archive and this script
# so it cannot attest provenance.  For that you should not be using curl|sh!
if command -v sha256sum >/dev/null 2>&1; then
	(cd "$tmp" && sha256sum -c "$archive.sha256")
elif command -v shasum >/dev/null 2>&1; then
	(cd "$tmp" && shasum -a 256 -c "$archive.sha256")
else
	echo "wiff: no sha256sum or shasum found; skipping checksum verification" >&2
fi

# Find the binary by name rather than assuming its path within the archive, so a
# change to the archive layout cannot silently extract nothing.
tar -xzf "$tmp/$archive" -C "$tmp"
binary="$(find "$tmp" -type f -name wiff | head -n1)"
if [ -z "$binary" ]; then
	echo "wiff: no wiff binary found in $archive" >&2
	exit 1
fi

mkdir -p "$bin_dir"
cp "$binary" "$bin_dir/wiff"
chmod +x "$bin_dir/wiff"
echo "wiff: installed $bin_dir/wiff"

case ":$PATH:" in
*":$bin_dir:"*) ;;
*) echo "wiff: note: $bin_dir is not on your PATH" >&2 ;;
esac
