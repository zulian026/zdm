#!/usr/bin/env bash
# User-space ClamAV install for the laptop (no sudo needed).
#
# Background: `sudo pacman -S clamav` needs a password this agent
# does not have, so the system install is blocked. Arch packages are
# plain tarballs, so this script downloads the official Arch `clamav`
# package and extracts it under ~/opt/clamav, then fetches the
# signature database into ~/.local/share/clamav with a user
# freshclam.conf. Wrapper scripts in ~/opt/clamav/bin/ add the
# --database / --config-file flags, so the binaries behave like a
# normal install.
#
# Result:
#   ~/opt/clamav/bin/clamscan   — wrapper, usable as ZDM_CLAMSCAN
#   ~/opt/clamav/bin/freshclam  — wrapper, refreshes the user DB
#   ~/.local/share/clamav/*.cvd — signature database
#
# Usage (on the laptop):
#   bash .taste-skill/e2e-clamav/install-clamav-userspace.sh
set -u

PREFIX="$HOME/opt/clamav"
DBDIR="$HOME/.local/share/clamav"
WORK="$HOME/zdm-e2e-clamav/clamav-pkg"
URL="https://archlinux.org/packages/extra/x86_64/clamav/download/"

mkdir -p "$PREFIX" "$DBDIR" "$WORK"
cd "$WORK" || exit 1

PKG=$(ls clamav-*.pkg.tar.zst 2>/dev/null | head -n 1)
if [ -z "$PKG" ]; then
    echo "downloading Arch clamav package from $URL ..."
    curl -L -o clamav.pkg.tar.zst "$URL" || exit 1
    PKG="clamav.pkg.tar.zst"
fi
echo "package: $PKG ($(du -h "$PKG" | cut -f1))"

if command -v bsdtar >/dev/null 2>&1; then
    bsdtar -xf "$PKG" -C "$PREFIX" || exit 1
elif command -v unzstd >/dev/null 2>&1; then
    tar --use-compress-program=unzstd -xf "$PKG" -C "$PREFIX" || exit 1
else
    echo "need bsdtar or unzstd to extract .pkg.tar.zst — aborting"
    exit 1
fi

BIN="$PREFIX/usr/bin/clamscan"
[ -x "$BIN" ] || { echo "clamscan not found after extract — aborting"; exit 1; }

# The laptop has no libmspack (clamscan links it): fetch the Arch
# libmspack package too and extract it into the same prefix, so the
# binaries resolve all their libraries from $PREFIX/usr/lib.
MSPACK="$WORK/libmspack.pkg.tar.zst"
if [ ! -f "$MSPACK" ]; then
    echo "downloading Arch libmspack package ..."
    curl -L -o "$MSPACK" "https://archlinux.org/packages/extra/x86_64/libmspack/download/" || exit 1
fi
if command -v bsdtar >/dev/null 2>&1; then
    bsdtar -xf "$MSPACK" -C "$PREFIX" || exit 1
elif command -v unzstd >/dev/null 2>&1; then
    tar --use-compress-program=unzstd -xf "$MSPACK" -C "$PREFIX" || exit 1
else
    echo "need bsdtar or unzstd to extract .pkg.tar.zst — aborting"
    exit 1
fi

echo "--- missing shared libraries (empty is good) ---"
MISSING=$(LD_LIBRARY_PATH="$PREFIX/usr/lib" ldd "$BIN" 2>/dev/null | grep "not found" || true)
[ -n "$MISSING" ] && { echo "$MISSING"; echo "missing libraries — the user-space install cannot run here"; exit 1; }
echo "(none)"

# Wrappers: point the binaries at the user database directory, and at
# the prefix's lib dir (for libmspack, which the system lacks).
# Both wrappers export CVD_CERTS_DIR: libclamav reads the env var for
# the CVD signature-verification certs (default is the compile-time
# /etc/clamav/certs, unwritable here). Without it, freshclam's own init
# passes via CVDCertsDirectory in freshclam.conf, but the database
# test load (and clamscan's database load) still fail with
# "Invalid certs directory '/etc/clamav/certs'".
mkdir -p "$PREFIX/bin"
cat > "$PREFIX/bin/clamscan" <<EOF
#!/bin/sh
CVD_CERTS_DIR="$PREFIX/etc/clamav/certs" LD_LIBRARY_PATH="$PREFIX/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}" exec "$PREFIX/usr/bin/clamscan" --database="$DBDIR" "\$@"
EOF
cat > "$PREFIX/bin/freshclam" <<EOF
#!/bin/sh
CVD_CERTS_DIR="$PREFIX/etc/clamav/certs" LD_LIBRARY_PATH="$PREFIX/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}" exec "$PREFIX/usr/bin/freshclam" --config-file="$DBDIR/freshclam.conf" "\$@"
EOF
chmod 755 "$PREFIX/bin/clamscan" "$PREFIX/bin/freshclam"

cat > "$DBDIR/freshclam.conf" <<EOF
DatabaseDirectory $DBDIR
UpdateLogFile $DBDIR/freshclam.log
DatabaseMirror database.clamav.net
# freshclam 1.5 verifies CVD signatures against a certs directory that
# defaults to the compile-time /etc/clamav/certs (unwritable here).
# The Arch package ships the CA cert at etc/clamav/certs/clamav.crt,
# so point the config at the extracted copy instead.
CVDCertsDirectory $PREFIX/etc/clamav/certs
EOF

echo "--- fetching signature database (main.cvd + daily.cvd, ~300 MB) ---"
"$PREFIX/bin/freshclam" || { echo "freshclam failed — check $DBDIR/freshclam.log"; exit 1; }
ls -la "$DBDIR"
echo "--- self-test ---"
printf 'X5O!P%%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*' > "$WORK/eicar.com"
"$PREFIX/bin/clamscan" --no-summary "$WORK/eicar.com"; [ "$?" = 1 ] || { echo "self-test FAILED: EICAR not flagged"; exit 1; }
echo "user-space ClamAV OK: $PREFIX/bin/clamscan"
echo "use it as: ZDM_CLAMSCAN=$PREFIX/bin/clamscan"
