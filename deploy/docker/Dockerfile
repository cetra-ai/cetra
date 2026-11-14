# The llmobs image (D-N-3). Built from the release binaries that tools/packaging/dist.sh stages
# in target/dist/image/linux-<arch>/; nothing is compiled here, so a multi-arch build needs no
# emulation. Build from the repository root:
#   docker build -f deploy/docker/Dockerfile -t llmobs:dev .
FROM gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931

ARG TARGETARCH

# The binary and the licence notices, owned by root.
COPY target/dist/image/linux-${TARGETARCH}/rootfs/ /
# /data, owned by the runtime user, so a new named volume is writable (Docker copies the
# directory's owner into an empty volume).
COPY --chown=65532:65532 target/dist/image/linux-${TARGETARCH}/datadir/ /

ENV LLMOBS_DATA_DIR=/data
USER 65532:65532
VOLUME ["/data"]
# 8080: UI, API, OTLP/HTTP, MCP. 4317: OTLP/gRPC. 4318: OTLP/HTTP (S11).
EXPOSE 8080 4317 4318
HEALTHCHECK --interval=30s --timeout=10s --start-period=120s --retries=3 \
  CMD ["/usr/local/bin/llmobs", "health"]
ENTRYPOINT ["/usr/local/bin/llmobs"]
CMD ["serve"]
