ARG RUNTIME_IMAGE=debian:bookworm-slim
FROM ${RUNTIME_IMAGE}

ARG NYRO_VERSION=dev

LABEL org.opencontainers.image.title="Nyro AI Gateway" \
      org.opencontainers.image.description="Local Nyro server runtime image" \
      org.opencontainers.image.source="https://github.com/Link2Link/nyro" \
      org.opencontainers.image.version="${NYRO_VERSION}" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libgcc-s1 tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system nyro \
    && useradd --system --gid nyro --home-dir /var/lib/nyro \
       --create-home --shell /usr/sbin/nologin nyro \
    && mkdir -p /var/lib/nyro

COPY nyro-server /usr/local/bin/nyro-server

RUN chmod 0755 /usr/local/bin/nyro-server \
    && chown nyro:nyro /usr/local/bin/nyro-server /var/lib/nyro

USER nyro

ENV NYRO_DATA_DIR=/var/lib/nyro \
    NYRO_PROXY_HOST=0.0.0.0 \
    NYRO_ADMIN_HOST=0.0.0.0

EXPOSE 19530 19531

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:19530/health > /dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/nyro-server"]
