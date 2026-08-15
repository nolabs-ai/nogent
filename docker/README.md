# nogent container image

`Dockerfile` builds `nogent-listener` on Chainguard's Rust image and ships it on
the distroless `glibc-dynamic` base (nonroot, no shell/package manager). It also
installs a checksum-pinned nono binary. Every review/triage job runs as a
separate least-privilege worker with phantom credentials; the listener retains
the real App secrets. CA roots are baked into the binary (rustls +
webpki-roots), so no `ca-certificates` layer is needed.

CI (`.github/workflows/image.yml`) builds and pushes to
`ghcr.io/nolabs-ai/nogent` on every `v*` tag (and `:edge` on `main`), with
an SBOM, provenance, and a keyless cosign signature.

## Build / run locally

```bash
# from the repo root
docker build -f docker/Dockerfile -t ghcr.io/nolabs-ai/nogent:dev .

docker run --rm -p 8080:8080 \
  --read-only --tmpfs /tmp:rw,noexec,nosuid,nodev,size=128m \
  --pids-limit 128 --memory 1g --cap-drop ALL \
  --security-opt no-new-privileges \
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

## Worker sandbox

The production profile is
[`../deploy/nogent-worker.nono.json`](../deploy/nogent-worker.nono.json). It
permits only GitHub's required repository endpoints, Gemini generation, and
`codeload.github.com`; the worker receives no filesystem write grant. At
listener startup, nogent launches a real sandbox child and refuses to bind the
webhook port if the profile, executable grant, phantom credentials, or proxy
URLs are broken. Validate profile changes with:

```bash
nono profile validate deploy/nogent-worker.nono.json
```
