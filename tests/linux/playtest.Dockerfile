# Local interactive Steam/Proton testing; supports optional WSL graphics.
# This still does not replace a native Linux physical-display validation host.
ARG BASE_IMAGE=receos-linux-build:local
FROM ${BASE_IMAGE}
RUN dpkg --add-architecture i386 \
    && curl -fsSL https://repo.steampowered.com/steam/archive/stable/steam.gpg -o /usr/share/keyrings/steam.gpg \
    && printf '%s\n' 'deb [arch=amd64,i386 signed-by=/usr/share/keyrings/steam.gpg] https://repo.steampowered.com/steam/ stable steam' > /etc/apt/sources.list.d/steam-stable.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
       libgl1-mesa-dri:amd64 libgl1-mesa-dri:i386 \
       libgl1-mesa-glx:amd64 libgl1-mesa-glx:i386 libvulkan1:amd64 libvulkan1:i386 \
       mesa-vulkan-drivers:amd64 mesa-vulkan-drivers:i386 vulkan-tools \
       libnss3:i386 libasound2:i386 \
       xfce4 xfce4-statusnotifier-plugin xfce4-notifyd \
       x11vnc novnc websockify xfonts-base
# Preserve the signed repository configuration generated above when the
# launcher package supplies its default copy of the same conffile.
RUN apt-get -o Dpkg::Options::=--force-confold install -y --no-install-recommends \
    steam-launcher steam-libs-amd64:amd64 steam-libs-i386:i386 x11-utils
# Keep apt metadata: Steam's normal dependency check needs a current cache.
RUN apt-get install -y --no-install-recommends xdg-desktop-portal xdg-desktop-portal-gtk \
    && steamdeps --dry-run
COPY tests/linux/playtest-session.sh /usr/local/bin/receos-playtest-session
COPY tests/linux/wsl-gpu-bwrap.py /usr/local/bin/receos-wsl-gpu-bwrap
RUN chmod 755 /usr/local/bin/receos-playtest-session /usr/local/bin/receos-wsl-gpu-bwrap
EXPOSE 6080
CMD ["/usr/local/bin/receos-playtest-session"]
