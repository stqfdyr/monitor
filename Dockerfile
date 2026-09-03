# Carries the musl binary release.yml already built, rather than building one of
# its own: a from-source stage would have to repeat the panel build,
# scripts/theme.sh and cross, and would then drift from the binary CI actually
# ships. Put monitor-hub-amd64 / monitor-hub-arm64 beside this file to build it
# by hand; otherwise pull ghcr.io/stqfdyr/monitor.

# Pinned to the build platform because zoneinfo is arch-independent data, which
# is what keeps a two-platform build from needing QEMU.
FROM --platform=$BUILDPLATFORM alpine:3 AS base
RUN apk add --no-cache tzdata && mkdir /data

FROM scratch
ARG TARGETARCH

# The day and billing-period boundaries are local dates on purpose (db.rs), and
# chrono falls back to UTC in silence when the zone files are missing. Without
# these, today's traffic would roll over at the wrong hour and say nothing.
COPY --from=base /usr/share/zoneinfo /usr/share/zoneinfo
ENV TZ=UTC

# Nothing in this image can mkdir -- no shell, and the hub runs unprivileged --
# so /data is baked in already owned. The hub creates themes/ under it itself.
COPY --from=base --chown=65534:65534 /data /data
COPY --chown=65534:65534 monitor-hub-$TARGETARCH /monitor-hub
USER 65534:65534

EXPOSE 28080
ENTRYPOINT ["/monitor-hub", "--db", "/data/monitor.db"]
