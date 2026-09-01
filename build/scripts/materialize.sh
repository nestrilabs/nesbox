#!/usr/bin/env bash
# Unpack a built jailer image into a plain directory tree.
#
# For local testing only. In production, materializing an OCI image onto
# shared storage is a job for whatever launches the box, not this script —
# this exists so the image can be inspected and the jailer tested against it
# on a single machine.
#
# Unlike nestri/build/scripts/mkimage.sh, nothing here needs root: a chroot
# target is a directory, not a block device, so there is no mkfs/loop mount
# step — just an export and a tar extraction, both of which the invoking
# user can do to their own directory.
set -euo pipefail

IMAGE="${1:?usage: materialize.sh <image-tag> <output-dir>}"
OUT="${2:?usage: materialize.sh <image-tag> <output-dir>}"

CONTAINER_RT="$(command -v docker || command -v podman || true)"
[[ -n "$CONTAINER_RT" ]] || { echo "Neither docker nor podman found in PATH" >&2; exit 1; }

# Extract into a sibling temp directory, then swap it into place with two
# renames rather than `rm -rf "$OUT"; mv "$TMP_OUT" "$OUT"`: that still had a
# real gap where $OUT was already gone before the replacement landed, so an
# interrupted or failed `mv` left neither the old jail nor the new one.
# `mv` between two directories on the same filesystem is a `rename()` — near-
# instant, not a copy — so `$OUT` -> `$OUT.old` -> (new) `$OUT` leaves the
# window where something valid isn't at `$OUT` as short as two syscalls
# rather than as long as an rm -rf of a multi-hundred-MB tree. cid and
# OLD_OUT are both cleaned up on every exit path via the trap.
TMP_OUT="${OUT}.tmp.$$"
OLD_OUT="${OUT}.old.$$"
cid=""
cleanup() {
    [[ -n "$cid" ]] && "$CONTAINER_RT" rm -f "$cid" >/dev/null 2>&1
    rm -rf "$TMP_OUT" "$OLD_OUT"
}
trap cleanup EXIT

rm -rf "$TMP_OUT"
mkdir -p "$TMP_OUT"

echo "Exporting ${IMAGE}..."
cid="$("$CONTAINER_RT" create "$IMAGE")"
"$CONTAINER_RT" export "$cid" | tar -x -C "$TMP_OUT" --exclude='.dockerenv' --exclude='dev/*'
"$CONTAINER_RT" rm -f "$cid" >/dev/null
cid=""

# From here, $OUT is never missing: it's the old tree until the first mv
# lands, then the new one from the moment the second mv lands. A failure
# between the two renames still leaves the old jail recoverable at
# $OLD_OUT rather than gone.
[[ -e "$OUT" ]] && mv "$OUT" "$OLD_OUT"
mv "$TMP_OUT" "$OUT"

echo "Wrote ${OUT}"
