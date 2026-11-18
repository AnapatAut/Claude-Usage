# Throwaway machine for sandbox/run.sh: Ubuntu 24.04 booting real systemd, the real curl, and
# nginx as an HTTPS stand-in for api.anthropic.com (on loopback, /etc/hosts points the name
# there), its certificate signed by a test CA that only this image trusts. Two users: sbuser
# (2001) with linger, so it has a user manager, and sbuser2 (2002) without. The repo's tracked
# files in /opt/cu, the release binary that scripts/cargo built in /opt/cu-bin.
# Never used in production. Packages are baked in: the test container has no network.
# Pinned by digest (a re-pointed tag cannot swap it): the same ubuntu:24.04 as tests/cli.sh.
FROM ubuntu@sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus dbus-user-session curl ca-certificates openssl nginx jq procps \
    && rm -rf /var/lib/apt/lists/* \
    && systemctl mask systemd-sysctl.service systemd-udevd.service systemd-udevd-control.socket \
       systemd-udevd-kernel.socket systemd-modules-load.service systemd-remount-fs.service \
       systemd-firstboot.service getty.target console-getty.service \
       sys-kernel-config.mount sys-kernel-debug.mount sys-kernel-tracing.mount \
       sys-fs-fuse-connections.mount dev-hugepages.mount systemd-binfmt.service \
       proc-sys-fs-binfmt_misc.automount systemd-hwdb-update.service systemd-random-seed.service \
       systemd-timesyncd.service systemd-resolved.service systemd-networkd.service \
    && useradd -m -s /bin/bash -u 2001 sbuser \
    && useradd -m -s /bin/bash -u 2002 sbuser2 \
    && mkdir -p /var/lib/systemd/linger && touch /var/lib/systemd/linger/sbuser
# The test CA and the stand-in's certificate, valid 7 days. CERT_DATE (run.sh passes today's
# date) busts the build cache daily, so a cached layer never hands out expired certificates.
ARG CERT_DATE
RUN echo "test certificates made ${CERT_DATE}" && mkdir -p /opt/sb && cd /opt/sb \
    && openssl req -x509 -newkey rsa:2048 -nodes -days 7 -subj "/CN=cu-sandbox-ca" \
       -keyout ca.key -out ca.crt 2>/dev/null \
    && openssl req -newkey rsa:2048 -nodes -subj "/CN=api.anthropic.com" -keyout tls.key -out tls.csr 2>/dev/null \
    && printf 'subjectAltName=DNS:api.anthropic.com\nextendedKeyUsage=serverAuth\n' > ext \
    && openssl x509 -req -in tls.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 7 -extfile ext \
       -out tls.crt 2>/dev/null \
    && rm ca.key tls.csr \
    && cp ca.crt /usr/local/share/ca-certificates/cu-sandbox-ca.crt && update-ca-certificates >/dev/null
COPY cu/sandbox/https-stub/stub.conf /etc/nginx/sites-available/cu-stub
COPY cu/sandbox/https-stub/mode-ok.conf /etc/nginx/cu-mode.conf
RUN rm -f /etc/nginx/sites-enabled/default \
    && ln -s /etc/nginx/sites-available/cu-stub /etc/nginx/sites-enabled/cu-stub \
    && nginx -t 2>/dev/null
COPY cu /opt/cu
COPY claude-usage /opt/cu-bin/claude-usage
