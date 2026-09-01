SHELL := /bin/bash
.PHONY: build jailer materialize clean help

CONTAINER_RT := $(shell command -v docker 2>/dev/null || command -v podman 2>/dev/null)
ifeq ($(CONTAINER_RT),)
$(error "Neither docker nor podman found in PATH")
endif

# Same reason as nestri/build/Makefile: the Dockerfile COPYs vmm/,
# virtio-devices/, pci/, tools/ and patches/, so the build context has to be
# the repo root, not this directory.
# ghcr.io/nestrilabs/nesbox/jail — a namespaced package name and a
# fully-qualified one, same reasoning as nestri/build/Makefile's
# IMAGE_NAME: matches how every other image this org publishes is named,
# and resolves correctly for anyone pulling instead of building locally.
# "jail", not "jailer": this builds the tree the jailer chroots INTO. The
# jailer binary itself is a host-side tool built by the repo's own
# `cargo build --release` (`tools/jailer`), and `make jailer` below copies it
# out of this build for convenience — it is deliberately not installed into
# the image, because it runs before the chroot and never inside it.
CONTEXT       := ..
IMAGE_NAME    := ghcr.io/nestrilabs/nesbox/jail
OUTPUT_DIR    := output
FORCE_REBUILD ?=
# Floating by default — see the NESTRI_BASE_IMAGE comment in the Dockerfile
# for why that's fine for local development and not for anything meant to
# pair with a specific guest image. Override with a digest for the latter:
#   make build NESTRI_BASE_IMAGE=ghcr.io/nestrilabs/nestri/base@sha256:...
NESTRI_BASE_IMAGE ?= ghcr.io/nestrilabs/nestri/base:latest

# Needs NESTRI_BASE_IMAGE to already exist — locally (built with `make build`
# in nestri/build/) or pullable from wherever it ends up published. Not
# fetched automatically here: which one you get should be a decision you
# make, not a side effect of this target.
build:
	DOCKER_BUILDKIT=1 $(CONTAINER_RT) build $(if $(FORCE_REBUILD),--no-cache,) \
		--build-arg NESTRI_BASE_IMAGE=$(NESTRI_BASE_IMAGE) \
		-f Dockerfile -t $(IMAGE_NAME):latest $(CONTEXT)

# The host-side jailer, built on the host rather than extracted from the
# image above — it runs before the chroot and links against the *host's*
# libc, so the host is where it has to be built. `cargo build --release` at
# the repo root already produces it beside nesbox; this only copies it here so
# a materialized jail and the binary that enters it sit together.
jailer:
	@mkdir -p $(OUTPUT_DIR)
	cargo build --release --manifest-path $(CONTEXT)/Cargo.toml -p jailer
	install -Dm755 $(CONTEXT)/target/release/jailer $(OUTPUT_DIR)/jailer

# Unpacks the built image into a plain directory the jailer can chroot into
# directly, with the jailer beside it. This is a reference implementation for
# local testing, not the production path — materializing onto shared storage
# in production is a job for whatever launches the box, not this script.
materialize: build jailer
	@mkdir -p $(OUTPUT_DIR)
	bash scripts/materialize.sh $(IMAGE_NAME):latest $(OUTPUT_DIR)/jail

clean:
	rm -rf $(OUTPUT_DIR)

help:
	@echo "Usage:"
	@echo "  make build              Build the $(IMAGE_NAME):latest container image"
	@echo "  make jailer             Build the host-side jailer into output/jailer"
	@echo "  make materialize        Build + unpack into output/jail/, jailer beside it
	@echo "  make clean              Remove output/"
	@echo "  make FORCE_REBUILD=1 ...  Rebuild from scratch, no layer cache"
