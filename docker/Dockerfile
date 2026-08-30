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

# nono is pinned by version AND per-architecture release checksum. The amd64
# release is static musl; arm64 is the published glibc build (the final image is
# glibc-dynamic). Keeping the checksum here prevents a replaced release asset
# from silently changing the sandbox supervisor embedded in nogent.
FROM cgr.dev/chainguard/rust:latest-dev AS nono

ARG TARGETARCH
ARG NONO_VERSION=0.72.0

USER root
RUN apk add --no-cache curl
RUN case "${TARGETARCH}" in \
      amd64) \
        NONO_TARGET="x86_64-unknown-linux-musl"; \
        NONO_SHA256="c36f7a3b9ceb85a53f247f25a310a0da353f099e921e3da6a402ae6b8ade9d0a" ;; \
      arm64) \
        NONO_TARGET="aarch64-unknown-linux-gnu"; \
        NONO_SHA256="ab47ee32893ec0429b6e6988a56f9d9cb0bdcc076144b9790fe47e2bfe546dcd" ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    NONO_ASSET="nono-v${NONO_VERSION}-${NONO_TARGET}.tar.gz" && \
    curl -fsSL \
      "https://github.com/nolabs-ai/nono/releases/download/v${NONO_VERSION}/${NONO_ASSET}" \
      -o /tmp/nono.tar.gz && \
    echo "${NONO_SHA256}  /tmp/nono.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/nono.tar.gz -C /usr/local/bin && \
    chmod 0755 /usr/local/bin/nono

FROM cgr.dev/chainguard/glibc-dynamic:latest

LABEL org.opencontainers.image.source="https://github.com/nolabs-ai/nogent"
LABEL org.opencontainers.image.description="AI PR security review + issue triage GitHub App"
LABEL org.opencontainers.image.licenses="Apache-2.0"

COPY --from=builder /usr/local/bin/nogent-listener /usr/bin/nogent-listener
COPY --from=nono /usr/local/bin/nono /usr/bin/nono
COPY deploy/nogent-worker.nono.json /usr/share/nogent/nogent-worker.nono.json
COPY deploy/nogent-question.nono.json /usr/share/nogent/nogent-question.nono.json

ENV NOGENT_WORKER_MODE=nono \
    NOGENT_NONO_BIN=/usr/bin/nono \
    NOGENT_NONO_PROFILE=/usr/share/nogent/nogent-worker.nono.json \
    NOGENT_NONO_QUESTION_PROFILE=/usr/share/nogent/nogent-question.nono.json \
    HOME=/tmp/nogent-home \
    XDG_CONFIG_HOME=/tmp/nogent-config \
    XDG_STATE_HOME=/tmp/nogent-state \
    XDG_CACHE_HOME=/tmp/nogent-cache

# glibc-dynamic already runs as nonroot (uid 65532).
EXPOSE 8080
ENTRYPOINT ["/usr/bin/nogent-listener"]
