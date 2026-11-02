#!/bin/sh
# The plugin's `bin/atlas` on macOS and Linux: starts the prebuilt binary
# for this machine from the same folder. (On Windows the plugin's command
# `bin/atlas` resolves to `bin/atlas.exe`; only Git Bash runs this script
# there, and it hands over to atlas.exe.)
#
# The release script copies this file to bin/atlas beside binaries named
# atlas-<arch>-<os>, for example atlas-x86_64-unknown-linux-gnu and
# atlas-aarch64-apple-darwin.
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 127
case "$(uname -s)" in
    # Git Bash on Windows finds this script before atlas.exe on PATH.
    MINGW* | MSYS* | CYGWIN*) exec "$dir/atlas.exe" "$@" ;;
    Linux) os=unknown-linux-gnu ;;
    Darwin) os=apple-darwin ;;
    *) os=unknown ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) arch=unknown ;;
esac
exe="$dir/atlas-$arch-$os"
if [ -x "$exe" ]; then
    exec "$exe" "$@"
fi
# Apple Silicon runs the x86_64 build under Rosetta 2 when that is all there is.
if [ "$os" = apple-darwin ] && [ -x "$dir/atlas-x86_64-$os" ]; then
    exec "$dir/atlas-x86_64-$os" "$@"
fi
echo "atlas: this release has no prebuilt binary for $(uname -sm)." >&2
echo "atlas: build it from source with a Rust toolchain:" >&2
echo "atlas:   cargo build --release --locked   (in the plugin folder: $(dirname -- "$dir"))" >&2
echo "atlas:   then copy target/release/atlas to $exe" >&2
exit 127
