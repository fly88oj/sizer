# Test entry points. `make gate` is the release gate: it fails when any layer
# fails, so no result depends on reading logs by hand.
#
# Requirements: cargo, python3 with python-xlib and PyGObject, Xvfb, gnome-shell
# (for e2e-gnome), xrandr and xprop.
CARGO ?= cargo
PYTHON ?= python3
GJS ?= gjs
JS_UNIT ?= tests/unit/preset_state_test.js
PY_UNIT ?= tests/unit/wait_applied_test.py
# The suites run the release build: it is what the package ships, and a
# debug-only pass would not prove that.
SIZER ?= target/release/sizer
CLIENT ?= tests/e2e/gnome_client.py
X11_OUT ?= tests/e2e/artifacts/x11-gate
GNOME_TAG ?= gnome46
GNOME_DISPLAY ?= :97

.PHONY: build test test-js test-py e2e-x11 e2e-gnome e2e-deb gate \
        pkg-deb pkg-rpm pkg-appimage pkg-pacman pkg-flatpak pkg-all

build:
	$(CARGO) build --release -p sizer-cli

# Rust unit tests, including the configuration and geometry model, plus the
# extension-side bookkeeping and the harness' result matching, which both need no
# compositor.
test: test-js test-py
	$(CARGO) test --workspace

# Preset request bookkeeping in the extension; pure JS, no shell imports.
test-js:
	$(GJS) -m $(JS_UNIT)

# Applied-result matching in the harness; no shell and no session bus.
test-py:
	$(PYTHON) $(PY_UNIT)

# Real X11 behaviour against gnome-shell acting as the window manager.
e2e-x11: build
	$(PYTHON) tests/e2e/x11_backend.py --strict --sizer $(SIZER) --client $(CLIENT) --out $(X11_OUT)

# The GNOME Shell extension itself, in a throwaway session bus and compositor.
e2e-gnome: build
	bash tests/e2e/run_gnome_harness.sh $(GNOME_TAG) --strict \
		--nested-display $(GNOME_DISPLAY) --sizer $(abspath $(SIZER))

# The packaged artifact: apt install in a clean container, run, purge, no leftovers.
e2e-deb: build
	$(PYTHON) tests/e2e/deb_package.py

gate: test e2e-x11 e2e-gnome e2e-deb

# ── Packaging targets ──────────────────────────────────────────────────
#
# Each target runs build first (unless you touch the skip-build flag via
# PACKAGING_SKIP_BUILD=1), then calls the corresponding build-*.sh script.
# All scripts stage the same tree via packaging/_common.sh so formats never
# drift apart on which files ship.

PKG_DEB        := packaging/build-deb.sh
PKG_RPM        := packaging/build-rpm.sh
PKG_APPIMAGE   := packaging/build-appimage.sh
PKG_PACMAN     := packaging/build-pacman.sh
PKG_FLATPAK    := packaging/build-flatpak.sh
PKG_SKIP_FLAG  := $(if $(filter 1,$(PACKAGING_SKIP_BUILD)),--skip-build,)

pkg-deb:   build; bash $(PKG_DEB)       $(PKG_SKIP_FLAG)
pkg-rpm:   build; bash $(PKG_RPM)       $(PKG_SKIP_FLAG)
pkg-appimage: build; bash $(PKG_APPIMAGE)  $(PKG_SKIP_FLAG)
pkg-pacman: build; bash $(PKG_PACMAN)   $(PKG_SKIP_FLAG)
pkg-flatpak: build; bash $(PKG_FLATPAK)  $(PKG_SKIP_FLAG)

pkg-all: pkg-deb pkg-rpm pkg-appimage pkg-pacman pkg-flatpak
