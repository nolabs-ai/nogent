# nogent container image

`Dockerfile` builds `nogent-listener` on Chainguard's Rust image and ships it on
the distroless `glibc-dynamic` base (nonroot, no shell/package manager). CA roots
are baked into the binary (rustls + webpki-roots), so no `ca-certificates` layer
is needed.

CI (`.github/workflows/image.yml`) builds and pushes to
`ghcr.io/nolabs-ai/nogent` on every `v*` tag (and `:edge` on `main`), with
an SBOM, provenance, and a keyless cosign signature.

## Build / run locally

```bash
# from the repo root
docker build -f docker/Dockerfile -t ghcr.io/nolabs-ai/nogent:dev .

docker run --rm -p 8080:8080 \
  -e NOGENT_BIND_ADDR=0.0.0.0:8080 \
  --env-file ./.env \
  -v "$PWD/secrets/app-private-key.pem:/etc/nogent/app-private-key.pem:ro" \
  ghcr.io/nolabs-ai/nogent:dev
```

## Single-host deploy (listener + Caddy)

`compose.yaml` runs the listener behind a Caddy container that terminates TLS.
The listener port is **not** published to the host — only Caddy's 80/443 are.

```bash
# host: /etc/nogent/nogent.env (0600) + /etc/nogent/app-private-key.pem (0600)
# edit docker/Caddyfile (domain + email)
IMAGE_TAG=0.1.0 docker compose -f docker/compose.yaml up -d
```

Verify the signature before deploying:

```bash
cosign verify ghcr.io/nolabs-ai/nogent:0.1.0 \
  --certificate-identity-regexp 'https://github.com/nolabs-ai/nogent/.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Overriding prompts

The system prompts are embedded in the image. To change them without rebuilding,
mount a directory and point `NOGENT_PROMPTS_DIR` at it (see
`../crates/nogent-core/prompts/`):

```yaml
    environment:
      NOGENT_PROMPTS_DIR: /etc/nogent/prompts
    volumes:
      - /etc/nogent:/etc/nogent:ro          # includes prompts/
```
