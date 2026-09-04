#!/bin/sh
# Materialises the pinned default theme into target/theme/, where rust-embed
# picks it up.
#
# The hub embeds a *built* theme, not theme source. It used to get one out of
# web-theme/, a directory the developer was told to clone by hand -- a build
# input that was undeclared, mutable and never verified, so a hub could be
# built against a stale theme and say nothing at all about it. There is no such
# directory now: the input is derived from web-theme.pin every time, into a
# place nobody works in, so there is no second checkout left to drift.
#
# Called by build.rs, and by CI before cargo runs -- CI so that the download
# happens on the runner rather than inside the cross container, which is not
# guaranteed to carry curl.
set -eu

cd "$(dirname "$0")/.."
# read answers 1 at EOF, which is also what a pin file with no trailing newline
# gives -- the fields are set either way. Unguarded, set -e would exit here with
# nothing printed and build.rs would point at the empty output it got.
read -r TAG SHA <web-theme.pin || true
[ -n "${TAG:-}" ] && [ -n "${SHA:-}" ] ||
  { echo "web-theme.pin must hold '<tag> <sha256>'" >&2; exit 1; }
DEST=target/theme
URL="https://github.com/stqfdyr/monitor-theme-default/releases/download/$TAG/theme.tar.gz"

# Already unpacked at this pin. A theme placed here by hand with a matching
# stamp is left alone too, which is how you build against one that has not been
# released yet.
if [ -f "$DEST/.pin" ] && [ "$(cat "$DEST/.pin")" = "$TAG $SHA" ]; then
  exit 0
fi

mkdir -p target
curl -fsSL --retry 3 -o target/theme.tar.gz "$URL"

GOT=$(sha256sum target/theme.tar.gz | cut -d' ' -f1)
if [ "$GOT" != "$SHA" ]; then
  echo "theme $TAG hashes to $GOT, not the $SHA that web-theme.pin names" >&2
  echo "the release asset was replaced, or the pin is wrong; neither is safe to build" >&2
  exit 1
fi

rm -rf "$DEST"
mkdir -p "$DEST"
tar xzf target/theme.tar.gz -C "$DEST"

# The archive is an installable theme directory: exactly what frontend.rs reads
# a theme from on disk. If that shape is missing, the hub would embed nothing.
if [ ! -f "$DEST/dist/index.html" ] || [ ! -f "$DEST/theme.json" ]; then
  echo "theme $TAG unpacked without dist/index.html and theme.json" >&2
  exit 1
fi

printf '%s %s\n' "$TAG" "$SHA" >"$DEST/.pin"
echo "default theme $TAG unpacked into $DEST"
