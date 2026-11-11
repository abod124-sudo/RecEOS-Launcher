FROM rust:1.98-bookworm AS rust
FROM node:24-bookworm-slim AS node
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl wget file build-essential pkg-config python3 python3-dbus python3-gi \
    libwebkit2gtk-4.1-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
    patchelf libfuse2 libnotify-bin xdg-utils xvfb xauth dbus-x11 dunst \
    webkit2gtk-driver weston xwayland libgl1-mesa-dri mesa-utils desktop-file-utils \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good pulseaudio \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo/bin /usr/local/cargo/bin
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:$PATH CARGO_TARGET_DIR=/build
RUN useradd --create-home --uid 1000 validation
WORKDIR /workspace
CMD ["sleep", "infinity"]
