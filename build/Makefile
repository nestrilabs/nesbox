SHELL := /bin/bash
.PHONY: build materialize clean help

CONTAINER_RT := $(shell command -v docker 2>/dev/null || command -v podman 2>/dev/null)
ifeq ($(CONTAINER_RT),)
$(error "Neither docker nor podman found in PATH")
endif

# Same reason as nestri/build/Makefile: the Dockerfile COPYs vmm/,
# virtio-devices/, pci/, tools/ and patches/, so the build context has to be
# the repo root, not this directory.
CONTEXT       := ..
IMAGE_NAME    := nesbox-jail
OUTPUT_DIR    := output
FORCE_REBUILD ?=
# Floating by default — see the NESTRI_BASE_IMAGE comment in the Dockerfile
# for why that's fine for local development and not for anything meant to
# pair with a specific guest image. Override with a digest for the latter:
#   make build NESTRI_BASE_IMAGE=nestrilabs/nestri@sha256:...
NESTRI_BASE_IMAGE ?= nestrilabs/nestri:base

# Needs NESTRI_BASE_IMAGE to already exist — locally (built with `make build`
# in nestri/build/) or pullable from wherever it ends up published. Not
# fetched automatically here: which one you get should be a decision you
# make, not a side effect of this target.
build:
	DOCKER_BUILDKIT=1 $(CONTAINER_RT) build $(if $(FORCE_REBUILD),--no-cache,) \
		--build-arg NESTRI_BASE_IMAGE=$(NESTRI_BASE_IMAGE) \
		-f Dockerfile -t $(IMAGE_NAME):latest $(CONTEXT)

# Unpacks the built image into a plain directory the jailer can chroot into
# directly. This is a reference implementation for local testing, not the
# production path — materializing onto shared storage in production is a job
# for whatever launches the box, not this script.
materialize: build
	@mkdir -p $(OUTPUT_DIR)
	bash scripts/materialize.sh $(IMAGE_NAME):latest $(OUTPUT_DIR)/jail

clean:
	rm -rf $(OUTPUT_DIR)

help:
	@echo "Usage:"
	@echo "  make build              Build the nesbox-jail:latest container image"
	@echo "  make materialize        Build + unpack into output/jail/ (local testing only)"
	@echo "  make clean              Remove output/"
	@echo "  make FORCE_REBUILD=1 ...  Rebuild from scratch, no layer cache"
