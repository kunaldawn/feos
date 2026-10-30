IMAGE        ?= feos
TARGET       ?= aarch64-unknown-none
PROFILE      ?= release
KERNEL       := target/$(TARGET)/$(PROFILE)/feos
QEMU         ?= qemu-system-aarch64
QEMU_DISPLAY ?= vnc=127.0.0.1:0

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

AUDIO_FLAGS  ?= -audiodev $(AUDIODEV) -device intel-hda -device hda-output,audiodev=snd0
QEMU_FLAGS   ?= -machine virt -cpu cortex-a53 -smp 1 -m 128M -net none -device ramfb $(AUDIO_FLAGS) -serial mon:stdio

# Display backend used by `make docker-run`. "gtk" opens a QEMU window on the
# host through X11/XWayland; "vnc=0.0.0.0:0" serves the display over VNC.
FEOS_DISPLAY ?= gtk

.PHONY: all build check run clean test test-gfx test-gui test-audio test-sync \
        test-json test-yaml test-image test-audio-codec test-compress test-archive \
        audio-demo docker-build docker-run docker-run-vnc docker-audio docker-shell

all: build

# `default-members = ["kernel"]` in the workspace manifest means this builds
# the bootable kernel and everything it pulls in.
build:
	cargo build --$(PROFILE)

check:
	cargo check --workspace

run: build
	mkdir -p target/audio
	$(QEMU) $(QEMU_FLAGS) -display $(QEMU_DISPLAY) -kernel $(KERNEL)

test: test-gfx test-gui test-audio test-sync test-json test-yaml test-image test-audio-codec test-compress test-archive

# Host-side tests. Architecture and board crates contain AArch64 assembly or
# MMIO, so the test set is explicit rather than `--workspace`.
test-gfx:
	cargo test -p feos-gfx --target x86_64-unknown-linux-gnu

test-gui:
	cargo test -p feos-gui --target x86_64-unknown-linux-gnu

test-audio:
	cargo test -p feos-audio --target x86_64-unknown-linux-gnu

test-sync:
	cargo test -p feos-sync --target x86_64-unknown-linux-gnu

test-json:
	cargo test -p feos-json --target x86_64-unknown-linux-gnu

test-yaml:
	cargo test -p feos-yaml --target x86_64-unknown-linux-gnu

test-image:
	cargo test -p feos-image --target x86_64-unknown-linux-gnu

test-audio-codec:
	cargo test -p feos-audio-codec --target x86_64-unknown-linux-gnu

test-compress:
	cargo test -p feos-compress --target x86_64-unknown-linux-gnu

test-archive:
	cargo test -p feos-archive --target x86_64-unknown-linux-gnu

# Host-side audio demo: renders the same library's PCM to a WAV on the host
# without booting QEMU. The kernel itself plays through the HDA driver with the
# `tone` command; this target is handy for comparing against the library alone.
AUDIO_OUT ?= target/audio/feos-tone.wav

audio-demo:
	mkdir -p $(dir $(AUDIO_OUT))
	cargo run -p feos-audio --example tone --target x86_64-unknown-linux-gnu -- $(AUDIO_OUT)
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
		'cargo run -p feos-audio --example tone --target x86_64-unknown-linux-gnu -- /tmp/feos-tone.wav 1>&2 && cat /tmp/feos-tone.wav' \
		> target/audio/feos-tone.wav.tmp
	mv target/audio/feos-tone.wav.tmp target/audio/feos-tone.wav
	@echo "wrote target/audio/feos-tone.wav"
	@echo "play it with: aplay target/audio/feos-tone.wav  (or paplay/ffplay/afplay)"

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
		-e FEOS_DISPLAY="$(FEOS_DISPLAY)" \
		-e FEOS_AUDIODEV="$$audiod" \
		$$audio_args \
		-v /tmp/.X11-unix:/tmp/.X11-unix \
		-p 5900:5900 \
		$(IMAGE)

# Headless / remote: serve the display over VNC on localhost:5900. Audio follows
# the same AUDIODEV rules as `make docker-run`, so WAV output lands in
# target/audio and ALSA/PulseAudio play live.
docker-run-vnc: docker-build
	@set -e; \
	audiod="$(AUDIODEV)"; \
	audiod="$$(printf '%s' "$$audiod" | sed 's#target/audio#/out#')"; \
	audio_args=""; \
	case "$$audiod" in \
		alsa*) audio_args="--device /dev/snd";; \
		pa*) audio_args="-v $(PULSE_SOCKET):$(PULSE_SOCKET) -e PULSE_SERVER=unix:$(PULSE_SOCKET)";; \
		wav*) mkdir -p target/audio; audio_args="-v $(CURDIR)/target/audio:/out";; \
	esac; \
	docker run --rm -it -p 5900:5900 \
		-e FEOS_DISPLAY=vnc=0.0.0.0:0 \
		-e FEOS_AUDIODEV="$$audiod" \
		$$audio_args \
		$(IMAGE)

docker-shell:
	docker run --rm -it -v "$(CURDIR)":/src -v feos-target:/src/target -w /src $(IMAGE) bash
