# syntax=docker/dockerfile:1.7

# Multi-arch image built from the pre-compiled static musl binaries that
# the release workflow drops into ./dist/ before invoking buildx.
# Expected names: haze-x86_64-unknown-linux-musl, haze-aarch64-unknown-linux-musl.

FROM --platform=$BUILDPLATFORM alpine:3 AS picker
ARG TARGETARCH
COPY dist/ /dist/
RUN set -eux; \
    case "$TARGETARCH" in \
        amd64) cp /dist/haze-x86_64-unknown-linux-musl /haze ;; \
        arm64) cp /dist/haze-aarch64-unknown-linux-musl /haze ;; \
        *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    chmod +x /haze

FROM gcr.io/distroless/static-debian12:nonroot

LABEL org.opencontainers.image.source="https://github.com/consi/haze"
LABEL org.opencontainers.image.description="Haze — network latency monitor with embedded UI"
LABEL org.opencontainers.image.licenses="AGPL-3.0-or-later"

COPY --from=picker /haze /usr/bin/haze

EXPOSE 4420
VOLUME ["/var/lib/haze"]

ENV HAZE_BIND=0.0.0.0:4420 \
    HAZE_DATA_DIR=/var/lib/haze \
    HAZE_LOG=haze=info

USER nonroot:nonroot
ENTRYPOINT ["/usr/bin/haze"]
