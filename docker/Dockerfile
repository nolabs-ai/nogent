# nogent-listener container image.
# Plain Dockerfile instructions only — builds identically under Docker/BuildKit
# and podman/Buildah (no `# syntax` frontend, no BuildKit-only cache mounts).
#
# Builder: Chainguard's Rust image (glibc). Runtime: Chainguard's distroless
# glibc-dynamic (nonroot, no shell, no package manager). reqwest uses rustls +
# `ring` with webpki-roots, so:
#   - the builder needs only a C toolchain + perl/make (ring's build script),
#   - the runtime needs NO ca-certificates (Mozilla roots are baked into the
#     binary), which is why a distroless base works.
#
# The system prompts are embedded at build time (include_str! over
# crates/nogent-core/prompts), so nothing extra is copied. Override them at
# runtime by mounting a dir and setting NOGENT_PROMPTS_DIR.
#
# Build (from repo root):
#   docker build -f docker/Dockerfile -t ghcr.io/nolabs-ai/nogent:dev .

FROM cgr.dev/chainguard/rust:latest-dev AS builder

USER root
RUN apk add --no-cache build-base perl make pkgconf

WORKDIR /build
COPY . .

RUN cargo build --release -p nogent-listener && \
    cp target/release/nogent-listener /usr/local/bin/nogent-listener

FROM cgr.dev/chainguard/glibc-dynamic:latest

LABEL org.opencontainers.image.source="https://github.com/nolabs-ai/nogent"
LABEL org.opencontainers.image.description="AI PR security review + issue triage GitHub App"
LABEL org.opencontainers.image.licenses="Apache-2.0"

COPY --from=builder /usr/local/bin/nogent-listener /usr/bin/nogent-listener

# glibc-dynamic already runs as nonroot (uid 65532).
EXPOSE 8080
ENTRYPOINT ["/usr/bin/nogent-listener"]
