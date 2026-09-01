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

rm -rf "$OUT"
mkdir -p "$OUT"

echo "Exporting ${IMAGE} into ${OUT}..."
cid="$("$CONTAINER_RT" create "$IMAGE")"
"$CONTAINER_RT" export "$cid" | tar -x -C "$OUT" --exclude='.dockerenv' --exclude='dev/*'
"$CONTAINER_RT" rm -f "$cid" >/dev/null

echo "Wrote ${OUT}"
