ARG DISTRO=ubuntu:24.04
FROM ${DISTRO}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates python3 python3-dbus python3-gi libwebkit2gtk-4.1-0 libgtk-3-0 \
    libayatana-appindicator3-1 libnotify-bin xdg-utils xvfb xauth dbus-x11 dunst \
    libssl-dev webkit2gtk-driver weston xwayland libgl1-mesa-dri libegl1 libgles2 \
    desktop-file-utils gstreamer1.0-plugins-base gstreamer1.0-plugins-good pulseaudio \
    && rm -rf /var/lib/apt/lists/*
RUN if ! id 1000 >/dev/null 2>&1; then useradd --create-home --uid 1000 validation; fi
WORKDIR /workspace
CMD ["sleep", "infinity"]
