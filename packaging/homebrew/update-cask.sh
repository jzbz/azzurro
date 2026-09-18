#!/bin/sh
# Write the cask for a published release, with the real hash filled in.
#
#   ./packaging/homebrew/update-cask.sh v0.1.0 ~/zx/dev/homebrew-tap/Casks/azzurro.rb
#
# Run after the release is published, the notarized macOS zip is attached and
# SHA256SUMS is signed — the hash has to be of the artifact users will actually
# download, which is the stapled one, not the zip CI produced.
#
# The output is a path rather than stdout on purpose. `> Casks/azzurro.rb`
# truncates the cask before this script has run a line, so any failure below
# would leave an empty file for the commit that follows to publish. Here the
# cask is built in a temporary file beside the real one and renamed over it
# only after every check has passed; a failed run leaves it untouched.
#
# By hand rather than from CI: updating the tap from here would need a token
# with write access to another repository, and a two-line change once a release
# does not justify holding one. The tap is a git repo; commit and push it.
set -eu

# The key that signs every release's SHA256SUMS, 249738C8641C3359. Compared as
# the whole fingerprint, since a 64-bit key ID is short enough for someone to
# generate a key that shares it.
FPR=252B901C88853CF9F9392559249738C8641C3359

TAG="${1:-}"
OUT="${2:-}"
[ -n "$TAG" ] && [ -n "$OUT" ] || {
    echo "usage: $0 <tag> <cask>   e.g. $0 v0.1.0 ~/zx/dev/homebrew-tap/Casks/azzurro.rb" >&2
    exit 1
}
case "$TAG" in v*) ;; *) echo "error: tag should start with v, got '$TAG'" >&2; exit 1 ;; esac
VERSION="${TAG#v}"
OUT_DIR=$(dirname "$OUT")
[ -d "$OUT_DIR" ] || { echo "error: no directory $OUT_DIR to write the cask into" >&2; exit 1; }
command -v gpg >/dev/null 2>&1 || { echo "error: gpg is needed to check the signature on SHA256SUMS" >&2; exit 1; }

NAME="azzurro-$TAG-macos-universal.zip"
BASE="https://github.com/jzbz/azzurro/releases/download/$TAG"

WORK=$(mktemp -d)
NEW=""
cleanup() {
    rm -rf "$WORK"
    [ -z "$NEW" ] || rm -f "$NEW"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# All three are required. A release missing SHA256SUMS or its signature is one
# whose checksums have not been through the signing step yet, and that is
# exactly the release that must not reach a Homebrew user.
fetch() {
    echo "fetching $BASE/$1" >&2
    if ! curl -sSLf --max-time "$2" -o "$WORK/$1" "$BASE/$1"; then
        echo "error: could not download $BASE/$1" >&2
        echo "  A draft release's assets are not public — publish it first, and" >&2
        echo "  sign SHA256SUMS and upload SHA256SUMS.asc before updating the cask." >&2
        exit 1
    fi
}
fetch "$NAME" 300
fetch SHA256SUMS 60
fetch SHA256SUMS.asc 60

# sha256sum on Linux, shasum on macOS: this runs on whichever machine is to hand.
if command -v sha256sum >/dev/null 2>&1; then
    SUM=$(sha256sum "$WORK/$NAME" | awk '{print $1}')
else
    SUM=$(shasum -a 256 "$WORK/$NAME" | awk '{print $1}')
fi
echo "  sha256 $SUM" >&2

# The signature first, so the checksum file is trusted before a line of it is
# read. gpg's exit status alone would accept a good signature from any key in
# the keyring, so the machine-readable status is checked for this one.
#
# Checking that a GOODSIG exists and that a VALIDSIG names FPR is not enough on
# its own, because an .asc can hold more than one signature and nothing ties
# those two lines to the same one. gpg writes VALIDSIG for a signature from a
# revoked or expired key too, labelled REVKEYSIG or EXPKEYSIG instead of
# GOODSIG, and still exits 0 if another signature in the file is good. So a
# stolen, revoked key plus any other key in the keyring would pass. Instead the
# file must hold exactly one signature, its result must be GOODSIG, and any
# other result (BADSIG, ERRSIG, EXPSIG, EXPKEYSIG, REVKEYSIG) refuses outright.
# That one GOODSIG's key ID has to be the key that VALIDSIG says made the
# signature, and VALIDSIG's last field, the primary key's fingerprint, has to be
# FPR, which holds even if a subkey signed. Nothing is fetched into the keyring.
if ! gpg --batch --no-auto-key-retrieve --status-fd 1 \
        --verify "$WORK/SHA256SUMS.asc" "$WORK/SHA256SUMS" \
        >"$WORK/status" 2>"$WORK/gpg.err"; then
    echo "error: SHA256SUMS.asc does not verify against SHA256SUMS" >&2
    sed 's/^/  /' "$WORK/gpg.err" >&2
    echo "  If gpg reports no public key, key $FPR is not in the keyring." >&2
    exit 1
fi
if ! awk -v fpr="$FPR" '
        $1 != "[GNUPG:]" { next }
        $2 ~ /^(GOODSIG|BADSIG|ERRSIG|EXPSIG|EXPKEYSIG|REVKEYSIG)$/ { results++ }
        $2 ~ /^(BADSIG|ERRSIG|EXPSIG|EXPKEYSIG|REVKEYSIG)$/ { refused = 1 }
        $2 == "GOODSIG" { goods++; keyid = toupper($3) }
        $2 == "VALIDSIG" {
            valids++
            signer = toupper($3)
            primary = toupper((NF >= 12) ? $12 : $3)
        }
        END {
            if (refused || results != 1 || goods != 1 || valids != 1) exit 1
            if (primary != fpr) exit 1
            # GOODSIG carries a 64-bit key ID or a whole fingerprint depending
            # on the gpg version; either way it is the tail of the signer.
            n = length(keyid)
            if (n < 16 || substr(signer, length(signer) - n + 1) != keyid) exit 1
            exit 0
        }' "$WORK/status"; then
    echo "error: SHA256SUMS.asc is not exactly one good signature by $FPR" >&2
    sed 's/^/  /' "$WORK/gpg.err" >&2
    grep -E '^\[GNUPG:\] (GOODSIG|BADSIG|ERRSIG|EXPSIG|EXPKEYSIG|REVKEYSIG|VALIDSIG) ' \
        "$WORK/status" | sed 's/^/  /' >&2 || true
    exit 1
fi
echo "  SHA256SUMS signed by $FPR" >&2

# Exactly one line for the zip, and it has to agree. No line at all is a
# failure rather than a pass: a check that skips itself when the thing it checks
# is absent has checked nothing.
WANT=$(awk -v n="$NAME" '$2 == n || $2 == "*" n {print $1}' "$WORK/SHA256SUMS")
LINES=$(printf '%s' "$WANT" | grep -c . || true)
if [ "$LINES" -ne 1 ]; then
    echo "error: SHA256SUMS has $LINES lines for $NAME, expected exactly one" >&2
    exit 1
fi
if [ "$WANT" != "$SUM" ]; then
    echo "error: hash does not match SHA256SUMS for $NAME" >&2
    echo "  downloaded: $SUM" >&2
    echo "  SHA256SUMS: $WANT" >&2
    exit 1
fi
echo "  matches SHA256SUMS" >&2

NEW=$(mktemp "$OUT_DIR/.$(basename "$OUT").XXXXXX")
sed -e "s/^  version \".*\"$/  version \"$VERSION\"/" \
    -e "s/^  sha256 \".*\"$/  sha256 \"$SUM\"/" \
    "$(dirname "$0")/azzurro.rb" >"$NEW"

# sed reports success whether or not a pattern matched, so a template whose
# stanzas were reformatted would otherwise pass through with the old values.
if ! grep -qx "  version \"$VERSION\"" "$NEW" || ! grep -qx "  sha256 \"$SUM\"" "$NEW"; then
    echo "error: the template's version or sha256 line was not rewritten" >&2
    exit 1
fi

# mktemp makes the file private; a cask is meant to be read.
chmod 644 "$NEW"
mv -f "$NEW" "$OUT"
NEW=""
echo "wrote $OUT" >&2
