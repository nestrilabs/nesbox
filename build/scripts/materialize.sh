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

# Extract into a sibling temp directory and swap it in only once extraction
# has actually succeeded, rather than wiping $OUT up front: a create/export/
# tar failure partway through used to both lose the last good jail and leave
# a half-populated one in its place. cid is cleaned up on every exit path
# via the trap, not just the one after a successful `rm -f`.
TMP_OUT="${OUT}.tmp.$$"
cid=""
cleanup() {
    [[ -n "$cid" ]] && "$CONTAINER_RT" rm -f "$cid" >/dev/null 2>&1
    rm -rf "$TMP_OUT"
}
trap cleanup EXIT

rm -rf "$TMP_OUT"
mkdir -p "$TMP_OUT"

echo "Exporting ${IMAGE}..."
cid="$("$CONTAINER_RT" create "$IMAGE")"
"$CONTAINER_RT" export "$cid" | tar -x -C "$TMP_OUT" --exclude='.dockerenv' --exclude='dev/*'
"$CONTAINER_RT" rm -f "$cid" >/dev/null
cid=""

rm -rf "$OUT"
mv "$TMP_OUT" "$OUT"

echo "Wrote ${OUT}"
