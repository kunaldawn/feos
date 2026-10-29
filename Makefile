IMAGE        ?= feos
TARGET       ?= aarch64-unknown-none
PROFILE      ?= release
KERNEL       := target/$(TARGET)/$(PROFILE)/feos
QEMU         ?= qemu-system-aarch64
QEMU_DISPLAY ?= vnc=127.0.0.1:0
QEMU_FLAGS   ?= -machine virt -cpu cortex-a53 -smp 1 -m 128M -net none -device ramfb -serial mon:stdio

# Display backend used by `make docker-run`. "gtk" opens a QEMU window on the
# host through X11/XWayland; "vnc=0.0.0.0:0" serves the display over VNC.
FEOS_DISPLAY ?= gtk

.PHONY: all build run clean test test-gfx docker-build docker-run docker-run-vnc docker-shell

all: build

build:
	cargo build --$(PROFILE)

run: build
	$(QEMU) $(QEMU_FLAGS) -display $(QEMU_DISPLAY) -kernel $(KERNEL)

test: test-gfx

test-gfx:
	cargo test -p feos-gfx --target x86_64-unknown-linux-gnu

clean:
	cargo clean

docker-build:
	docker build -t $(IMAGE) .

# Opens a real QEMU window. The serial shell stays in this terminal.
docker-run: docker-build
	@set -e; \
	if [ "$(FEOS_DISPLAY)" = "gtk" ] && [ -z "$$DISPLAY" ]; then \
		echo "DISPLAY is not set. Use 'make docker-run-vnc' for a VNC display."; \
		exit 1; \
	fi; \
	user=$$(id -un); \
	if [ "$(FEOS_DISPLAY)" = "gtk" ]; then \
		xhost +si:localuser:$$user >/dev/null 2>&1 || true; \
		trap 'xhost -si:localuser:$$user >/dev/null 2>&1 || true' EXIT INT TERM; \
	fi; \
	docker run --rm -it \
		-e DISPLAY \
		-e GDK_BACKEND=x11 \
		-e NO_AT_BRIDGE=1 \
		-e FEOS_DISPLAY="$(FEOS_DISPLAY)" \
		-v /tmp/.X11-unix:/tmp/.X11-unix \
		-p 5900:5900 \
		$(IMAGE)

# Headless / remote: serve the display over VNC on localhost:5900.
docker-run-vnc: docker-build
	docker run --rm -it -p 5900:5900 -e FEOS_DISPLAY=vnc=0.0.0.0:0 $(IMAGE)

docker-shell:
	docker run --rm -it -v "$(CURDIR)":/src -v feos-target:/src/target -w /src $(IMAGE) bash
