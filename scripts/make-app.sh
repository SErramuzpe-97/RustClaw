#!/usr/bin/env bash
# Build RustClaw.app: a double-clickable macOS bundle that starts the server and
# opens the UI. The binary lives inside the bundle, so the .app is self-contained
# and can be moved to /Applications or another machine.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="${1:-$ROOT/RustClaw.app}"
BIN="$ROOT/target/release/rustclaw"

[ -x "$BIN" ] || { echo "build it first: cargo build --release" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# The launcher is CFBundleExecutable "RustClaw"; the binary must NOT be called
# "rustclaw". macOS filesystems are case-insensitive by default, so the two
# names are the same file — writing the launcher would overwrite the binary and
# the script would then exec itself in an unbounded fork loop.
cp "$BIN" "$APP/Contents/MacOS/rustclaw-bin"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>RustClaw</string>
  <key>CFBundleDisplayName</key>       <string>RustClaw</string>
  <key>CFBundleIdentifier</key>        <string>ai.rustclaw.app</string>
  <key>CFBundleVersion</key>           <string>$(grep '^version' "$ROOT/Cargo.toml" | head -1 | cut -d'"' -f2)</string>
  <key>CFBundleShortVersionString</key><string>$(grep '^version' "$ROOT/Cargo.toml" | head -1 | cut -d'"' -f2)</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleExecutable</key>        <string>RustClaw</string>
  <key>CFBundleIconFile</key>          <string>icon</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <!-- The launcher only starts a server and opens a browser; no window of its own. -->
  <key>LSUIElement</key>               <true/>
</dict>
</plist>
PLIST

cat > "$APP/Contents/MacOS/RustClaw" <<'LAUNCHER'
#!/bin/bash
# A .app is launched by launchd, not by your shell: it inherits neither ~/.zshrc
# (read only by interactive shells) nor anything on your PATH. Everything here
# is therefore absolute and the environment is loaded explicitly — skipping this
# is what makes a bundled app answer 401 while the same binary works in a
# terminal.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/rustclaw-bin"

# Refuse to run if BIN is anything but a real executable. Without this, a build
# mistake that leaves BIN pointing at this script turns every launch into an
# unbounded fork loop that exhausts the process table.
if [ ! -x "$BIN" ] || ! /usr/bin/file -b "$BIN" | /usr/bin/grep -q "Mach-O"; then
  # `display alert` is modal and waits for a click, which hangs forever when
  # nobody is at the screen. A notification cannot block.
  /usr/bin/osascript -e 'display notification "Bundled binary missing or invalid — rebuild with scripts/make-app.sh" with title "RustClaw is damaged"' >/dev/null 2>&1
  echo "rustclaw: $BIN is not an executable binary; refusing to run" >&2
  exit 1
fi
HOME_DIR="${RUSTCLAW_HOME:-$HOME/.rustclaw}"
LOG="$HOME_DIR/serve.log"
mkdir -p "$HOME_DIR"

# API keys live in a 600 file beside the config, not in the shell profile.
[ -f "$HOME_DIR/secrets.env" ] && . "$HOME_DIR/secrets.env"

port() {
  local p
  p=$(sed -n 's/^[[:space:]]*port[[:space:]]*=[[:space:]]*\([0-9]\{1,\}\).*/\1/p' \
        "$HOME_DIR/config.toml" 2>/dev/null | tail -1)
  echo "${p:-8080}"
}
PORT="$(port)"
# Follow whatever the config binds to: a Tailscale address is not reachable on
# 127.0.0.1, so hardcoding loopback would open a dead page.
HOST=$(sed -n 's/^[[:space:]]*bind[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
        "$HOME_DIR/config.toml" 2>/dev/null | tail -1)
[ -z "$HOST" ] || [ "$HOST" = "0.0.0.0" ] && HOST=127.0.0.1
URL="http://$HOST:$PORT"
# A token-protected server answers 401 without it, so the opened link carries it.
[ -n "${RUSTCLAW_TOKEN:-}" ] && OPEN_URL="$URL/?token=$RUSTCLAW_TOKEN" || OPEN_URL="$URL"

note() {  # the app has no window, so problems have to reach the user somehow
  /usr/bin/osascript -e "display notification \"$1\" with title \"RustClaw\"" >/dev/null 2>&1
}

if ! "$BIN" config >/dev/null 2>&1; then
  "$BIN" config --init >>"$LOG" 2>&1 || true
fi

# Already serving? Just focus it — double-clicking twice must not start a second
# process fighting for the same port.
if /usr/bin/curl -fsS -m 2 -o /dev/null "$OPEN_URL" 2>/dev/null; then
  /usr/bin/open "$OPEN_URL"
  exit 0
fi

"$BIN" serve >>"$LOG" 2>&1 &
SERVER=$!

for _ in $(seq 1 40); do
  if /usr/bin/curl -fsS -m 1 -o /dev/null "$OPEN_URL" 2>/dev/null; then
    /usr/bin/open "$OPEN_URL"
    wait "$SERVER"
    exit $?
  fi
  kill -0 "$SERVER" 2>/dev/null || { note "Server exited. See $LOG"; exit 1; }
  sleep 0.25
done

note "Server did not come up on port $PORT. See $LOG"
kill "$SERVER" 2>/dev/null
exit 1
LAUNCHER
chmod +x "$APP/Contents/MacOS/RustClaw"

# Icon: a claw-orange rounded square with a "R". Generated so the repo carries no binary.
ICONSET="$(mktemp -d)/icon.iconset"; mkdir -p "$ICONSET"
SVG="$(mktemp).svg"
cat > "$SVG" <<'SVGEOF'
<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024">
  <rect width="1024" height="1024" rx="230" fill="#b7410e"/>
  <text x="512" y="700" font-family="Helvetica,Arial,sans-serif" font-size="620"
        font-weight="bold" fill="#fff" text-anchor="middle">R</text>
</svg>
SVGEOF
if /usr/bin/qlmanage -t -s 1024 -o "$(dirname "$SVG")" "$SVG" >/dev/null 2>&1; then
  BASE="$(dirname "$SVG")/$(basename "$SVG").png"
  for s in 16 32 64 128 256 512; do
    /usr/bin/sips -z $s $s "$BASE" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1
    /usr/bin/sips -z $((s*2)) $((s*2)) "$BASE" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null 2>&1
  done
  /usr/bin/iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/icon.icns" 2>/dev/null || true
fi

# Locally built and never downloaded, so there is no quarantine flag to clear;
# strip any inherited one anyway so Gatekeeper does not prompt.
/usr/bin/xattr -cr "$APP" 2>/dev/null || true

# Guard the invariant rather than trusting the comment above it.
if [ ! -x "$APP/Contents/MacOS/rustclaw-bin" ] \
   || ! file "$APP/Contents/MacOS/rustclaw-bin" | grep -q "Mach-O"; then
  echo "error: the binary inside the bundle is not an executable — a name" >&2
  echo "       collision with the launcher would fork-bomb on launch." >&2
  exit 1
fi
if [ "$(ls "$APP/Contents/MacOS" | wc -l | tr -d ' ')" != "2" ]; then
  echo "error: expected exactly launcher + binary in MacOS/" >&2
  exit 1
fi

echo "built $APP"
[ -f "$APP/Contents/Resources/icon.icns" ] && echo "  icon: yes" || echo "  icon: skipped (qlmanage unavailable)"
echo "  double-click it, or: open '$APP'"
