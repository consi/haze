# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

# Multi-arch image built from the pre-compiled static musl binaries that
# the release workflow drops into ./dist/ before invoking buildx.
# Expected names: haze-x86_64-unknown-linux-musl, haze-aarch64-unknown-linux-musl.

FROM --platform=$BUILDPLATFORM alpine:3.24@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b AS picker
ARG TARGETARCH
# `libcap` provides `setcap`; we use it below to set CAP_NET_RAW on the
# binary so the non-root distroless runtime can open ICMP sockets for
# `ping` probes without needing `--cap-add NET_RAW --user 0`.
RUN apk add --no-cache libcap
COPY dist/ /dist/
RUN set -eux; \
    case "$TARGETARCH" in \
        amd64) cp /dist/haze-x86_64-unknown-linux-musl /haze ;; \
        arm64) cp /dist/haze-aarch64-unknown-linux-musl /haze ;; \
        *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    chmod +x /haze; \
    # File capability `cap_net_raw=+ep` lets surge-ping open raw ICMP
    # sockets under uid 65532 (nonroot in distroless). Without this, the
    # PingClients constructor logs "ICMP v4 socket unavailable" at boot
    # and every ping probe's `build_probe` call fails on the host loop.
    # Operators who still don't want raw sockets can drop the capability
    # at runtime with `--cap-drop NET_RAW` (the probe just stops working
    # for `ping` hosts; other probe kinds are unaffected).
    setcap cap_net_raw=+ep /haze; \
    # Empty skeleton dir; COPY --chown below transfers it into the runtime
    # image so /var/lib/haze exists at the right uid/gid before the VOLUME
    # mount-point is materialised at first run.
    mkdir -p /var-lib-haze-skel

FROM gcr.io/distroless/static-debian13:nonroot@sha256:1c2c046bc09ed40fad370b599a0b1ae7987f55b01e247cf27a7c27cd97e5bbc7

LABEL org.opencontainers.image.source="https://github.com/consi/haze"
LABEL org.opencontainers.image.description="Haze — network latency monitor with embedded UI"
LABEL org.opencontainers.image.licenses="AGPL-3.0-or-later"

COPY --from=picker /haze /usr/bin/haze
# Pre-create /var/lib/haze with the nonroot user's ownership (uid 65532 /
# gid 65532 in distroless). Without this, Docker creates the VOLUME
# mount-point as root on first run and the nonroot runtime process gets
# SQLITE_CANTOPEN (code 14) trying to open haze.sqlite.
#
# Note: this only helps when the user runs with an anonymous volume or no
# `-v` at all. Bind-mounts (`-v /path/on/host:/var/lib/haze`) inherit the
# host path's ownership, so the operator still needs:
#   mkdir -p /path/on/host && sudo chown 65532:65532 /path/on/host
# or to run the container with `--user $(id -u):$(id -g)` so the runtime
# uid matches their host directory.
COPY --from=picker --chown=65532:65532 /var-lib-haze-skel /var/lib/haze

EXPOSE 4420
VOLUME ["/var/lib/haze"]

ENV HAZE_BIND=0.0.0.0:4420 \
    HAZE_DATA_DIR=/var/lib/haze \
    HAZE_LOG=haze=info

USER nonroot:nonroot
ENTRYPOINT ["/usr/bin/haze"]
