# A systemd PID 1 image for the delegation check. Debian's base image ships no
# init, so systemd is installed explicitly.
#
# Build:  docker build -f scripts/systemd-delegation.dockerfile -t wyd-systemd-test .
FROM debian:bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv libpam-systemd dbus dbus-user-session python3 procps \
 && rm -rf /var/lib/apt/lists/*

# Mask units that cannot work in a container and only add noise.
RUN systemctl mask systemd-udevd.service systemd-udevd-control.socket \
      systemd-udevd-kernel.socket systemd-modules-load.service \
      getty@.service || true

CMD ["/sbin/init"]
