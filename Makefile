IMAGE        ?= feos
TARGET_ARM   ?= aarch64-unknown-none
# The custom hard-float target in arch/x86_64/x86_64-feos.json: scripts/cargo-x64.sh
# explains why it isn't x86_64-unknown-none, and holds the flags it takes to build.
TARGET_X64   := x86_64-feos
CARGO_X64    := ./scripts/cargo-x64.sh
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
QEMU_SMP_X64 ?= 4

# PulseAudio/PipeWire socket, used for live sound when it exists.
PULSE_SOCKET ?= /run/user/$(shell id -u)/pulse/native

# QEMU audio backend. ALSA is used when the host exposes `/dev/snd` (Linux),
# otherwise QEMU writes a WAV under target/audio so the guest's output can
# still be verified. The run scripts pin every backend (unless it names its own
# `out.frequency`) to the codec's 48 kHz, so QEMU never resamples the guest's
# sound: its resampler is audibly harsh. Override with e.g.
#   make run AUDIODEV=pa,id=snd0
#   make run AUDIODEV=wav,id=snd0,path=/tmp/qemu.wav
# The backend id must stay `snd0`, because the HDA codec is attached to it.
ifeq ($(wildcard /dev/snd),)
AUDIODEV ?= wav,id=snd0,path=target/audio/qemu.wav,out.frequency=48000,out.channels=2,out.format=s16
else
AUDIODEV ?= alsa,id=snd0
endif

# The docker-run targets use the host's PulseAudio/PipeWire socket when there is
# one, unless AUDIODEV was given: ALSA's hardware device is exclusive, so on a
# desktop it's either busy ("Device or resource busy") or takes the sound card
# away from everything else for as long as QEMU runs.
PULSE_AUDIODEV := pa,id=snd0
DOCKER_AUDIODEV ?= $(if $(and $(wildcard $(PULSE_SOCKET)),$(filter file,$(origin AUDIODEV))),$(PULSE_AUDIODEV),$(AUDIODEV))

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

# User programs for each architecture, built by `make user-arm` and `make user-x64`.
# The kernel's build script puts what it finds in user/out/<arch> into /bin in the
# filesystem image, so these come first. RUSTFLAGS replaces the kernel's rustflags
# from .cargo/config.toml, which are for linking a kernel.
USER_TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),$(CURDIR)/target)/user
USER_BINS       := hello cat vmcheck spin utest wdemo malicious permtest runas
USER_FLAGS      := -C linker=rust-lld -C link-arg=-T$(CURDIR)/user/link.ld -C link-arg=-z -C link-arg=max-page-size=4096

.PHONY: all build build-arm build-x64 build-efi check check-arm check-x64 clippy clippy-arm clippy-x64 run run-arm run-x64 clean \
        test test-firmware test-frames test-irq test-gfx test-gui test-audio test-mixer test-math test-milkdrop test-iconset test-sync \
        test-heap test-sched test-hal test-console test-app test-pci test-i8042 test-apic test-gicv2 test-gicv3 test-uart16550 test-intel-hda \
        test-json test-yaml test-image test-audio-codec test-compress test-archive \
        test-h264 test-source test-opus test-video test-text test-bundle test-init test-fs test-keys \
        test-coreutils test-sh test-abi test-elf test-vm user user-arm user-x64 fuzz \
        test-desktop test-desktop-app test-virtio-input test-virtio-core test-virtio-blk test-virtio-net test-virtio-gpu test-ramfb test-rtc test-net test-xhci test-bootinfo test-dosbox test-dosbox-app test-apps \
        boot-test boot-test-arm boot-test-x64 docker-boot-test \
        bundles games-image games-unlink games-check run-games run-games-x64 audio-demo \
        docker-build docker-run docker-run-arm docker-run-x64 \
        docker-run-vnc docker-run-vnc-arm docker-run-vnc-x64 docker-audio docker-shell

all: build

# `default-members = ["kernel"]` in the workspace manifest means this builds
# the bootable kernel and everything it pulls in. `build` remains the AArch64
# default; use `build-x64` for the QEMU q35 port.
build: build-arm

build-arm: user-arm
	cargo build --profile $(CARGO_PROFILE) --target $(TARGET_ARM)

build-x64: user-x64
	$(CARGO_X64) build --profile $(CARGO_PROFILE)

user: user-arm user-x64

# `cp -p` keeps each program's link time. The boards' build scripts watch
# user/out, so a fresh mtime on an unchanged copy would rebuild and relink the
# kernel on every `make build-*` (and every `make docker-run`).
user-arm:
	cd user && RUSTFLAGS="$(USER_FLAGS) -C relocation-model=static" \
		cargo build --release --target $(TARGET_ARM) --target-dir $(USER_TARGET_DIR)
	mkdir -p user/out/aarch64
	cd $(USER_TARGET_DIR)/$(TARGET_ARM)/release && cp -fp $(USER_BINS) $(CURDIR)/user/out/aarch64/

# A program lives far above 2 GiB, so its code is position-independent, as the x86_64
# small code model would otherwise want it low, and it's linked at a fixed address.
user-x64:
	RUSTFLAGS="$(USER_FLAGS) -C relocation-model=pic -C code-model=small -C link-arg=--no-pie" \
		$(CARGO_X64) build --release --manifest-path user/Cargo.toml --target-dir $(USER_TARGET_DIR)
	mkdir -p user/out/x86_64
	cd $(USER_TARGET_DIR)/$(TARGET_X64)/release && cp -fp $(USER_BINS) $(CURDIR)/user/out/x86_64/

# The UEFI loaders. Each reads \\feos.elf, a kernel as built above, from its own volume.
build-efi:
	./scripts/cargo-efi.sh aarch64 --profile $(CARGO_PROFILE)
	./scripts/cargo-efi.sh x86_64 --profile $(CARGO_PROFILE)

# `--workspace` cannot build both architecture crates in one pass: each
# contains target-specific assembly. Check the port that matches the target.
EXCLUDE_ARM := --exclude feos-efi --exclude feos-arch-x86_64 --exclude feos-board-qemu-x86_64 \
	--exclude feos-driver-apic --exclude feos-driver-i8042 --exclude feos-driver-uart16550
EXCLUDE_X64 := --exclude feos-efi --exclude feos-arch-aarch64 --exclude feos-board-qemu-virt \
	--exclude feos-driver-gicv2 --exclude feos-driver-gicv3

check: check-arm

check-arm:
	cargo check --workspace --target $(TARGET_ARM) $(EXCLUDE_ARM)
	cd user && cargo check --target $(TARGET_ARM) --target-dir $(USER_TARGET_DIR)

check-x64:
	$(CARGO_X64) check --workspace $(EXCLUDE_X64)
	$(CARGO_X64) check --manifest-path user/Cargo.toml --target-dir $(USER_TARGET_DIR)

# Clippy over both ports, with every warning an error: CI runs these. The parser
# crates that read untrusted input add panic lints of their own in their
# Cargo.toml `[lints.clippy]`. The lints allowed here are style ones whose fixes
# would cost more than they'd buy: index loops in DSP code read like the
# specifications they follow, boxing a large enum variant would allocate, and
# `!(x > 0.0)` is meant to catch NaN.
CLIPPY_FLAGS := -D warnings \
	-A clippy::needless_range_loop \
	-A clippy::too_many_arguments \
	-A clippy::large_enum_variant \
	-A clippy::type_complexity \
	-A clippy::neg_cmp_op_on_partial_ord \
	-A clippy::should_implement_trait \
	-A clippy::result_unit_err \
	-A clippy::chunks_exact_to_as_chunks

clippy: clippy-arm clippy-x64

clippy-arm:
	cargo clippy --workspace --target $(TARGET_ARM) $(EXCLUDE_ARM) -- $(CLIPPY_FLAGS)
	cd user && cargo clippy --target $(TARGET_ARM) --target-dir $(USER_TARGET_DIR) -- $(CLIPPY_FLAGS)

clippy-x64:
	$(CARGO_X64) clippy --workspace $(EXCLUDE_X64) -- $(CLIPPY_FLAGS)
	$(CARGO_X64) clippy --manifest-path user/Cargo.toml --target-dir $(USER_TARGET_DIR) -- $(CLIPPY_FLAGS)

run: run-arm

run-arm: build-arm
	$(RUN_ENV_ARM) ./scripts/qemu.sh $(KERNEL_ARM)

run-x64: build-x64
	$(RUN_ENV_X64) ./scripts/qemu-x64.sh $(KERNEL_X64)

# Boots each kernel headless, drives the serial shell and checks the results.
# Needs QEMU on PATH; `make docker-boot-test` runs the same test in the image, with no
# network: slirp's name server is one in the test, and nothing leaves the container.
boot-test: boot-test-arm boot-test-x64

boot-test-arm: build-arm build-efi
	./scripts/boot-test.py arm $(KERNEL_ARM)

boot-test-x64: build-x64 build-efi
	./scripts/boot-test.py x64 $(KERNEL_X64)

docker-boot-test: docker-build
	docker run --rm --network none --entrypoint sh $(IMAGE) \
		-c 'echo nameserver 127.0.0.1 > /etc/resolv.conf; make boot-test'

test: test-firmware test-frames test-irq test-gfx test-gui test-audio test-mixer test-math test-milkdrop test-iconset test-sync test-heap test-sched test-hal test-console test-app test-pci test-i8042 test-apic test-gicv2 test-gicv3 test-uart16550 test-intel-hda test-json test-yaml test-image test-audio-codec test-compress test-archive test-h264 test-source test-opus test-video test-text test-bundle test-init test-fs test-keys test-coreutils test-sh test-abi test-elf test-vm test-desktop test-desktop-app test-virtio-input test-dosbox test-dosbox-app test-apps test-virtio-core test-virtio-blk test-virtio-net test-virtio-gpu test-ramfb test-rtc test-net test-xhci test-bootinfo

# The fuzz-style tests again, under the `fuzz` profile: optimised, with overflow
# checks and debug assertions, on a kernel-sized stack. `make test` runs the same
# tests under the dev profile. FEOS_FUZZ_SCALE=<n> runs n times the inputs.
FUZZ_CRATES := feos-archive feos-audio-codec feos-bundle feos-compress feos-fs feos-image \
	feos-init feos-json feos-sh feos-video feos-yaml

fuzz:
	FEOS_FUZZ_STACK=131072 cargo test --profile fuzz --target $(HOST_TARGET) \
		$(addprefix -p ,$(FUZZ_CRATES)) --test fuzz
	cargo test --profile fuzz --target $(HOST_TARGET) -p feos-h264 --test malformed

# Host-side tests. Architecture and board crates contain AArch64 assembly or
# MMIO, so the test set is explicit rather than `--workspace`.
test-firmware:
	cargo test -p feos-firmware --target $(HOST_TARGET)

test-frames:
	cargo test -p feos-frames --target $(HOST_TARGET)

test-abi:
	cargo test -p feos-abi --target $(HOST_TARGET)

test-elf:
	cargo test -p feos-elf --target $(HOST_TARGET)

test-vm:
	cargo test -p feos-vm --target $(HOST_TARGET)

test-irq:
	cargo test -p feos-irq --target $(HOST_TARGET)

test-gfx:
	cargo test -p feos-gfx --target $(HOST_TARGET)

test-gui:
	cargo test -p feos-gui --target $(HOST_TARGET)

test-audio:
	cargo test -p feos-audio --target $(HOST_TARGET)

test-mixer:
	cargo test -p feos-mixer --target $(HOST_TARGET)

test-math:
	cargo test -p feos-math --target $(HOST_TARGET)

test-milkdrop:
	cargo test -p feos-milkdrop --target $(HOST_TARGET)

test-iconset:
	cargo test -p feos-iconset --target $(HOST_TARGET)

test-sync:
	cargo test -p feos-sync --target $(HOST_TARGET)

test-heap:
	cargo test -p feos-heap --target x86_64-unknown-linux-gnu

test-sched:
	cargo test -p feos-sched --target x86_64-unknown-linux-gnu

test-hal:
	cargo test -p feos-hal --target x86_64-unknown-linux-gnu

test-console:
	cargo test -p feos-console --target x86_64-unknown-linux-gnu

test-app:
	cargo test -p feos-app --target x86_64-unknown-linux-gnu

test-pci:
	cargo test -p feos-driver-pci --target x86_64-unknown-linux-gnu

test-i8042:
	cargo test -p feos-driver-i8042 --target x86_64-unknown-linux-gnu

test-apic:
	cargo test -p feos-driver-apic --target x86_64-unknown-linux-gnu

test-gicv2:
	cargo test -p feos-driver-gicv2 --target x86_64-unknown-linux-gnu

test-gicv3:
	cargo test -p feos-driver-gicv3 --target $(HOST_TARGET)

test-uart16550:
	cargo test -p feos-driver-uart16550 --target x86_64-unknown-linux-gnu

test-intel-hda:
	cargo test -p feos-driver-intel-hda --target x86_64-unknown-linux-gnu

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

test-source:
	cargo test -p feos-source --target $(HOST_TARGET)

test-opus:
	cargo test -p feos-opus --target $(HOST_TARGET)

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

test-virtio-core:
	cargo test -p feos-virtio-core --target $(HOST_TARGET)

test-virtio-blk:
	cargo test -p feos-driver-virtio-blk --target $(HOST_TARGET)

test-virtio-net:
	cargo test -p feos-driver-virtio-net --target $(HOST_TARGET)

test-net:
	cargo test -p feos-net --target $(HOST_TARGET)

test-xhci:
	cargo test -p feos-driver-xhci --target $(HOST_TARGET)

test-bootinfo:
	cargo test -p feos-bootinfo --target $(HOST_TARGET)

test-virtio-gpu:
	cargo test -p feos-driver-virtio-gpu --target $(HOST_TARGET)

test-ramfb:
	cargo test -p feos-driver-ramfb --target $(HOST_TARGET)

test-rtc:
	cargo test -p feos-driver-pl031 -p feos-driver-cmos-rtc --target $(HOST_TARGET)

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

test-dosbox-app:
	cargo test -p feos-dosbox-app --target $(HOST_TARGET)

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
#   1. create apps/desktop/bundle-src/<id>/{manifest.yaml,icon.ico,icon-32.ico,icon-16.ico},
#      copying the icons from assets/icons
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

# The x86_64 guest runs under KVM when this is an x86_64 host and /dev/kvm can be
# opened: scripts/qemu-x64.sh picks KVM when it finds the device, so it's passed in.
# Emulating the CPU (TCG) works too, only much slower. FEOS_ACCEL=tcg forces it.
DOCKER_KVM_ARGS = kvm_args=""; \
	if [ "$(1)" = x64 ] && [ "$$(uname -m)" = x86_64 ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then \
		kvm_args="--device /dev/kvm"; \
	fi; \
	if [ -n "$$FEOS_ACCEL" ]; then kvm_args="$$kvm_args -e FEOS_ACCEL=$$FEOS_ACCEL"; fi;

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
	audiod="$(DOCKER_AUDIODEV)"; \
	audiod="$$(printf '%s' "$$audiod" | sed 's#target/audio#/out#')"; \
	audio_args=""; \
	case "$$audiod" in \
		alsa*) audio_args="--device /dev/snd";; \
		pa*) audio_args="-v $(PULSE_SOCKET):$(PULSE_SOCKET) -e PULSE_SERVER=unix:$(PULSE_SOCKET)"; \
			case "$$audiod" in *server=*) ;; *) audiod="$$audiod,server=unix:$(PULSE_SOCKET)";; esac;; \
		wav*) mkdir -p target/audio; audio_args="-v $(CURDIR)/target/audio:/out";; \
	esac; \
	$(DOCKER_KVM_ARGS) \
	docker run --rm -it $$kvm_args \
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

docker-run-x64: docker-build
	$(call DOCKER_RUN_BODY,x64)

# Headless / remote: serve the display over VNC on localhost:5900. Audio follows
# the same DOCKER_AUDIODEV rules as `make docker-run`, so WAV output lands in
# target/audio and ALSA/PulseAudio play live. QEMU in the image ignores
# PULSE_SERVER, so a `pa` backend is given the socket as `server=`.
define DOCKER_RUN_VNC_BODY
	@set -e; \
	audiod="$(DOCKER_AUDIODEV)"; \
	audiod="$$(printf '%s' "$$audiod" | sed 's#target/audio#/out#')"; \
	audio_args=""; \
	case "$$audiod" in \
		alsa*) audio_args="--device /dev/snd";; \
		pa*) audio_args="-v $(PULSE_SOCKET):$(PULSE_SOCKET) -e PULSE_SERVER=unix:$(PULSE_SOCKET)"; \
			case "$$audiod" in *server=*) ;; *) audiod="$$audiod,server=unix:$(PULSE_SOCKET)";; esac;; \
		wav*) mkdir -p target/audio; audio_args="-v $(CURDIR)/target/audio:/out";; \
	esac; \
	$(DOCKER_KVM_ARGS) \
	docker run --rm -it -p 127.0.0.1:5900:5900 $$kvm_args \
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

docker-run-vnc-x64: docker-build
	$(call DOCKER_RUN_VNC_BODY,x64)

docker-shell:
	docker run --rm -it -v "$(CURDIR)":/src -v feos-target:/src/target -w /src --entrypoint bash $(IMAGE)
