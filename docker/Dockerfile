FROM czyt/rustdesk-console:v0.2.57@sha256:abe3b19568576489cbbd2f8045c87d031af25ba135f79ff1bd694a27b8903854

ARG S6_OVERLAY_VERSION=3.2.0.0
ARG S6_ARCH=x86_64
ARG SERVER_BUILD_REVISION=unknown
LABEL org.opencontainers.image.revision="$SERVER_BUILD_REVISION"
LABEL org.opencontainers.image.source="https://github.com/dockers-x/rustdesk-server"
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz /tmp
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${S6_ARCH}.tar.xz /tmp
RUN \
  apt-get update && \
  apt-get install -y --no-install-recommends curl sqlite3 xz-utils && \
  rm -rf /var/lib/apt/lists/* && \
  tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz && \
  tar -C / -Jxpf /tmp/s6-overlay-${S6_ARCH}.tar.xz && \
  rm /tmp/s6-overlay*.tar.xz && \
  if [ ! -e /var/run ]; then ln -s /run /var/run; fi

COPY rootfs /

ENV RELAY=relay.example.com
ENV ENCRYPTED_ONLY=0
ENV HBBS_HEALTH_PORT=21120
ENV HBBR_HEALTH_PORT=21121

EXPOSE 21114 21115 21116 21116/udp 21117 21118 21119

HEALTHCHECK --interval=10s --timeout=5s CMD /usr/bin/healthcheck.sh

WORKDIR /app

VOLUME /app/data

VOLUME /data

ENTRYPOINT ["/init"]
CMD []
