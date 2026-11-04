IMAGE        ?= feos
TARGET_ARM   ?= aarch64-unknown-none
TARGET_X64   ?= x86_64-unknown-none
# Host-side tests build for this machine, whatever it is.
HOST_TARGET  ?= $(shell rustc -vV | sed -n 's/^host: //p')
# `PROFILE=debug` is accepted as a spelling of Cargo's `dev` profile, whose
# output directory is `debug`.
PROFILE      ?= release
CARGO_PROFILE := $(if $(filter debug,$(PROFILE)),dev,$(PROFILE))
PROFILE_DIR  := $(if $(filter dev debug,$(PROFILE)),debug,$(PROFILE))
KERNEL_ARM   := target/$(TARGET_ARM)/$(PROFILE_DIR)/feos
KERNEL_X64   := target/$(TARGET_X64)/$(PROFILE_DIR)/feos
QEMU_ARM     ?= qemu-system-aarch64
QEMU_X64     ?= qemu-system-x86_64
QEMU_DISPLAY ?= vnc=127.0.0.1:0
QEMU_MEM     ?= 512M
QEMU_SMP     ?= 4
# The x86_64 port currently runs the scheduler on the boot CPU.
QEMU_SMP_X64 ?= 1

# PulseAudio/PipeWire socket, used for live sound when it exists.
PULSE_SOCKET ?= /run/user/$(shell id -u)/pulse/native

# QEMU audio backend. ALSA is used when the host exposes `/dev/snd` (Linux),
# otherwise QEMU writes a WAV under target/audio so the guest's output can
# still be verified. The WAV backend is pinned to the codec's native format so
# the capture is not resampled. Override with e.g.
#   make run AUDIODEV=pa,id=snd0
#   make run AUDIODEV=wav,id=snd0,path=/tmp/qemu.wav
# The backend id must stay `snd0`, because the HDA codec is attached to it.
ifeq ($(wildcard /dev/snd),)
AUDIODEV ?= wav,id=snd0,path=target/audio/qemu.wav,out.frequency=48000,out.channels=2,out.format=s16
else
AUDIODEV ?= alsa,id=snd0
endif

# Passed through to QEMU by the run scripts on both architectures.
POINTER_FLAGS ?= -device virtio-tablet-device
KEYBOARD_FLAGS ?= -device virtio-keyboard-device
QEMU_EXTRA   ?=

# Everything the run scripts read. Both architectures use the same variables;
# the scripts hold the rest of the QEMU command line. The x86_64 machine has
# PS/2 input, so POINTER_FLAGS and KEYBOARD_FLAGS only apply to AArch64.
RUN_ENV_ARM = FEOS_QEMU="$(QEMU_ARM)" FEOS_DISPLAY="$(QEMU_DISPLAY)" FEOS_SMP="$(QEMU_SMP)" \
	FEOS_MEM="$(QEMU_MEM)" FEOS_AUDIODEV="$(AUDIODEV)" \
	FEOS_QEMU_ARGS="$(POINTER_FLAGS) $(KEYBOARD_FLAGS) $(QEMU_EXTRA)"
RUN_ENV_X64 = FEOS_QEMU="$(QEMU_X64)" FEOS_DISPLAY="$(QEMU_DISPLAY)" FEOS_SMP="$(QEMU_SMP_X64)" \
	FEOS_MEM="$(QEMU_MEM)" FEOS_AUDIODEV="$(AUDIODEV)" FEOS_QEMU_ARGS="$(QEMU_EXTRA)"

# Display backend used by `make docker-run`. "gtk" opens a QEMU window on the
# host through X11/XWayland; "vnc=0.0.0.0:0" serves the display over VNC.
# `make run` and `make run-x64` use QEMU_DISPLAY instead.
DOCKER_DISPLAY ?= gtk

.PHONY: all build build-arm build-x64 check check-arm check-x64 run run-arm run-x64 clean \
        test test-gfx test-gui test-audio test-sync \
        test-json test-yaml test-image test-audio-codec test-compress test-archive \
        test-h264 test-video test-text test-bundle test-init test-fs test-keys \
        test-coreutils test-sh \
        test-desktop test-desktop-app test-virtio-input test-dosbox test-apps \
        boot-test boot-test-arm boot-test-x64 docker-boot-test \
        bundles games-image games-unlink games-check run-games run-games-x64 audio-demo \
        docker-build docker-run docker-run-arm docker-run-x64 \
        docker-run-vnc docker-run-vnc-arm docker-run-vnc-x64 docker-audio docker-shell

all: build

# `default-members = ["kernel"]` in the workspace manifest means this builds
# the bootable kernel and everything it pulls in. `build` remains the AArch64
# default; use `build-x64` for the QEMU q35 port.
build: build-arm

build-arm:
	cargo build --profile $(CARGO_PROFILE) --target $(TARGET_ARM)

build-x64:
	cargo build --profile $(CARGO_PROFILE) --target $(TARGET_X64)

# `--workspace` cannot build both architecture crates in one pass: each
# contains target-specific assembly. Check the port that matches the target.
check: check-arm

check-arm:
	cargo check --workspace --target $(TARGET_ARM) \
		--exclude feos-arch-x86_64 \
		--exclude feos-board-qemu-x86_64 \
		--exclude feos-driver-apic \
		--exclude feos-driver-i8042 \
		--exclude feos-driver-uart16550

check-x64:
	cargo check --workspace --target $(TARGET_X64) \
		--exclude feos-arch-aarch64 \
		--exclude feos-board-qemu-virt \
		--exclude feos-driver-gicv2

run: run-arm

run-arm: build-arm
	$(RUN_ENV_ARM) ./scripts/qemu.sh $(KERNEL_ARM)

run-x64: build-x64
	$(RUN_ENV_X64) ./scripts/qemu-x64.sh $(KERNEL_X64)

# Boots each kernel headless, drives the serial shell and checks the results.
# Needs QEMU on PATH; `make docker-boot-test` runs the same test in the image.
boot-test: boot-test-arm boot-test-x64

boot-test-arm: build-arm
	./scripts/boot-test.py arm $(KERNEL_ARM)

boot-test-x64: build-x64
	./scripts/boot-test.py x64 $(KERNEL_X64)

docker-boot-test: docker-build
	docker run --rm --entrypoint sh $(IMAGE) -c 'make boot-test'

test: test-gfx test-gui test-audio test-sync test-json test-yaml test-image test-audio-codec test-compress test-archive test-h264 test-video test-text test-bundle test-init test-fs test-keys test-coreutils test-sh test-desktop test-desktop-app test-virtio-input test-dosbox test-apps

# Host-side tests. Architecture and board crates contain AArch64 assembly or
# MMIO, so the test set is explicit rather than `--workspace`.
test-gfx:
	cargo test -p feos-gfx --target $(HOST_TARGET)

test-gui:
	cargo test -p feos-gui --target $(HOST_TARGET)

test-audio:
	cargo test -p feos-audio --target $(HOST_TARGET)

test-sync:
	cargo test -p feos-sync --target $(HOST_TARGET)

test-json:
	cargo test -p feos-json --target $(HOST_TARGET)

test-yaml:
	cargo test -p feos-yaml --target $(HOST_TARGET)

test-image:
	cargo test -p feos-image --target $(HOST_TARGET)

test-audio-codec:
	cargo test -p feos-audio-codec --target $(HOST_TARGET)

test-compress:
	cargo test -p feos-compress --target $(HOST_TARGET)

test-archive:
	cargo test -p feos-archive --target $(HOST_TARGET)

test-h264:
	cargo test -p feos-h264 --target $(HOST_TARGET)

test-video:
	cargo test -p feos-video --target $(HOST_TARGET)

test-text:
	cargo test -p feos-text --target $(HOST_TARGET)

test-bundle:
	cargo test -p feos-bundle --target $(HOST_TARGET)

test-init:
	cargo test -p feos-init --target $(HOST_TARGET)

test-fs:
	cargo test -p feos-fs --target $(HOST_TARGET)

test-keys:
	cargo test -p feos-keys --target $(HOST_TARGET)

test-coreutils:
	cargo test -p feos-coreutils --target $(HOST_TARGET)

test-sh:
	cargo test -p feos-sh --target $(HOST_TARGET)

test-desktop:
	cargo test -p feos-desktop --target $(HOST_TARGET)

test-desktop-app:
	cargo test -p feos-desktop-app --target $(HOST_TARGET)

test-virtio-input:
	cargo test -p feos-driver-virtio-input --target $(HOST_TARGET)

test-apps:
	cargo test -p feos-apps --target $(HOST_TARGET)

test-dosbox:
	cargo test -p feos-dosbox --target $(HOST_TARGET)

# Links the 64 MiB exFAT game image as the board's optional disk. The board's
# build script embeds it, so every later kernel on both architectures includes
# it until `make games-unlink` (or `make clean`) removes the link. Importing a
# game into C: at run time is `MOUNT <dir>` inside FEOS-DOS.
GAMES_IMAGE ?= data/games/dos.img
games-image:
	ln -sfn $(abspath $(GAMES_IMAGE)) boards/qemu-virt/disk-exfat.img
	@echo "linked boards/qemu-virt/disk-exfat.img -> $(GAMES_IMAGE); run make build"

games-unlink:
	@if [ -L boards/qemu-virt/disk-exfat.img ]; then rm boards/qemu-virt/disk-exfat.img; fi

games-check:
	fsck.exfat -n $(GAMES_IMAGE)

# The image is linked into the kernel, so the guest needs RAM for it on top of
# the kernel's own 76 MiB. The recursive make keeps the link ahead of the build
# under `make -j`.
run-games: games-image
	$(MAKE) run-arm QEMU_MEM=512M

run-games-x64: games-image
	$(MAKE) run-x64 QEMU_MEM=512M

# Repacks every bundle source directory into apps/desktop/apps. The desktop's
# build script picks up whatever is in that folder, so adding an app is:
#   1. create apps/desktop/bundle-src/<id>/{manifest.yaml,icon.bmp}
#   2. make bundles
#   3. rebuild the kernel
bundles:
	@for dir in apps/desktop/bundle-src/*/; do \
		name=$$(basename "$$dir"); \
		python3 scripts/pack-app.py "$$dir" "apps/desktop/apps/$$name.fapp"; \
	done

# Host-side audio demo: renders the same library's PCM to a WAV on the host
# without booting QEMU. The kernel itself plays through the HDA driver with the
# `tone` command; this target is handy for comparing against the library alone.
AUDIO_OUT ?= target/audio/feos-tone.wav

audio-demo:
	mkdir -p $(dir $(AUDIO_OUT))
	cargo run -p feos-audio --example tone --target $(HOST_TARGET) -- $(AUDIO_OUT)
	@echo "play it with: aplay $(AUDIO_OUT)"

# Same demo inside the container. The WAV is streamed back over stdout rather
# than written through a bind mount, so it works with rootless Podman too and
# the file always ends up owned by the invoking user.
docker-audio: docker-build
	mkdir -p target/audio
	docker run --rm \
		--user "$$(id -u):$$(id -g)" \
		-e HOME=/tmp \
		-e CARGO_TARGET_DIR=/tmp/target \
		--entrypoint sh $(IMAGE) -c \
		'cargo run -p feos-audio --example tone --target $(HOST_TARGET) -- /tmp/feos-tone.wav 1>&2 && cat /tmp/feos-tone.wav' \
		> target/audio/feos-tone.wav.tmp
	mv target/audio/feos-tone.wav.tmp target/audio/feos-tone.wav
	@echo "wrote target/audio/feos-tone.wav"
	@echo "play it with: aplay target/audio/feos-tone.wav  (or paplay/ffplay/afplay)"

clean: games-unlink
	cargo clean

docker-build:
	docker build -t $(IMAGE) .

# Opens a real QEMU window. The serial shell stays in this terminal.
#
# `docker-run` is an alias for the AArch64 target. Both targets use the same
# image; the entrypoint selects the architecture from FEOS_ARCH.
define DOCKER_RUN_BODY
	@set -e; \
	if [ "$(DOCKER_DISPLAY)" = "gtk" ] && [ -z "$$DISPLAY" ]; then \
		echo "DISPLAY is not set. Use 'make docker-run-vnc' for a VNC display."; \
		exit 1; \
	fi; \
	user=$$(id -un); \
	if [ "$(DOCKER_DISPLAY)" = "gtk" ]; then \
		xhost +si:localuser:$$user +si:localuser:root >/dev/null 2>&1 || true; \
		trap 'xhost -si:localuser:$$user -si:localuser:root >/dev/null 2>&1 || true' EXIT INT TERM; \
	fi; \
	audiod="$(AUDIODEV)"; \
	audiod="$$(printf '%s' "$$audiod" | sed 's#target/audio#/out#')"; \
	audio_args=""; \
	case "$$audiod" in \
		alsa*) audio_args="--device /dev/snd";; \
		pa*) audio_args="-v $(PULSE_SOCKET):$(PULSE_SOCKET) -e PULSE_SERVER=unix:$(PULSE_SOCKET)";; \
		wav*) mkdir -p target/audio; audio_args="-v $(CURDIR)/target/audio:/out";; \
	esac; \
	docker run --rm -it \
		-e DISPLAY \
		-e GDK_BACKEND=x11 \
		-e NO_AT_BRIDGE=1 \
		-e FEOS_ARCH=$(1) \
		-e FEOS_DISPLAY="$(DOCKER_DISPLAY)" \
		-e FEOS_AUDIODEV="$$audiod" \
		-e FEOS_SMP="$(QEMU_SMP)" \
		-e FEOS_MEM="$(QEMU_MEM)" \
		-e FEOS_TEST_DISKS=1 \
		$$audio_args \
		-v /tmp/.X11-unix:/tmp/.X11-unix \
		$(IMAGE)
endef

docker-run: docker-run-arm

docker-run-arm: docker-build
	$(call DOCKER_RUN_BODY,arm)

docker-run-x64: QEMU_SMP = 1
docker-run-x64: docker-build
	$(call DOCKER_RUN_BODY,x64)

# Headless / remote: serve the display over VNC on localhost:5900. Audio follows
# the same AUDIODEV rules as `make docker-run`, so WAV output lands in
# target/audio and ALSA/PulseAudio play live.
define DOCKER_RUN_VNC_BODY
	@set -e; \
	audiod="$(AUDIODEV)"; \
	audiod="$$(printf '%s' "$$audiod" | sed 's#target/audio#/out#')"; \
	audio_args=""; \
	case "$$audiod" in \
		alsa*) audio_args="--device /dev/snd";; \
		pa*) audio_args="-v $(PULSE_SOCKET):$(PULSE_SOCKET) -e PULSE_SERVER=unix:$(PULSE_SOCKET)";; \
		wav*) mkdir -p target/audio; audio_args="-v $(CURDIR)/target/audio:/out";; \
	esac; \
	docker run --rm -it -p 127.0.0.1:5900:5900 \
		-e FEOS_ARCH=$(1) \
		-e FEOS_DISPLAY=vnc=0.0.0.0:0 \
		-e FEOS_AUDIODEV="$$audiod" \
		-e FEOS_SMP="$(QEMU_SMP)" \
		-e FEOS_MEM="$(QEMU_MEM)" \
		-e FEOS_TEST_DISKS=1 \
		$$audio_args \
		$(IMAGE)
endef

docker-run-vnc: docker-run-vnc-arm

docker-run-vnc-arm: docker-build
	$(call DOCKER_RUN_VNC_BODY,arm)

docker-run-vnc-x64: QEMU_SMP = 1
docker-run-vnc-x64: docker-build
	$(call DOCKER_RUN_VNC_BODY,x64)

docker-shell:
	docker run --rm -it -v "$(CURDIR)":/src -v feos-target:/src/target -w /src --entrypoint bash $(IMAGE)
