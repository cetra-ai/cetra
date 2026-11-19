# The cetra-demo image (D187, DO6). Like deploy/docker/Dockerfile it compiles nothing: `just demo-image`
# builds the static musl binary and stages it with a copy of fixtures/otlp in target/demo-image/, and
# this file copies those onto the same distroless base. The build context is that directory, so the
# repository's .dockerignore does not apply:
#   just demo-image [tag]
FROM gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931

COPY cetra-demo /usr/local/bin/cetra-demo
COPY fixtures/otlp /usr/local/share/cetra-demo/fixtures/otlp

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/cetra-demo", "--fixtures", "/usr/local/share/cetra-demo/fixtures/otlp"]
