FROM czyt/rustdesk-console:v0.2.44@sha256:b94fbfe63798a83f4478970176b9a5f095b91f6ef4a5f9862aa104c32f0d0ca9

ARG S6_OVERLAY_VERSION=3.2.0.0
ARG S6_ARCH=x86_64
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz /tmp
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${S6_ARCH}.tar.xz /tmp
RUN \
  apt-get update && \
  apt-get install -y --no-install-recommends xz-utils && \
  rm -rf /var/lib/apt/lists/* && \
  tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz && \
  tar -C / -Jxpf /tmp/s6-overlay-${S6_ARCH}.tar.xz && \
  rm /tmp/s6-overlay*.tar.xz && \
  if [ ! -e /var/run ]; then ln -s /run /var/run; fi

COPY rootfs /

ENV RELAY=relay.example.com
ENV ENCRYPTED_ONLY=0

EXPOSE 21114 21115 21116 21116/udp 21117 21118 21119

HEALTHCHECK --interval=10s --timeout=5s CMD /usr/bin/healthcheck.sh

WORKDIR /app

VOLUME /app/data

VOLUME /data

ENTRYPOINT ["/init"]
CMD []
