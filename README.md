# nogent

`nogent` is a GitHub App that runs AI-powered **PR security review** and
**issue triage**, using Google Gemini.

## How it works

```
GitHub ──webhook──▶  nogent-listener
                       1. verify webhook HMAC (reject if invalid)
                       2. mint a per-event installation token (App JWT)
                       3. read .github/nogent.json (fail-secure)
                       4. fetch the PR diff / issue, call Gemini
                       5. validate canary-gated JSON output
                       6. post the review / triage comment
```

A single long-lived process holds the secrets (App private key, webhook secret,
Gemini key) in zeroizing memory, fast-acks the webhook with `202`, and does the
work on a detached task. The App authenticates as itself with its own
installation token, so it reviews **fork PRs** without depending on any CI
secret.

Defenses that matter for a bot ingesting attacker-controlled input (diffs,
titles, issue bodies, and the model's own response):

- **HMAC verification** of every webhook against the raw body, constant-time.
- **Canary-gated output** — the model must echo a per-run random canary inside a
  strict JSON schema; any response that omits/alters it or adds keys is discarded
  and replaced by a fixed "manual review" comment. This is the prompt-injection
  backstop.
- **Least-privilege App permissions** (`contents:read`, `issues:write`,
  `pull_requests:write`, `metadata:read`).
- **Bounded diffs** (`maxFiles`/`maxPatchBytes`) and **fail-secure config**.

> **Planned hardening:** running the per-event work inside a
> [nono](https://github.com/always-further/nono) sandbox so the process handling
> untrusted content holds only phantom credentials and can reach only
> allowlisted hosts. Removed for now to keep the deploy simple; the design slots
> back in as a per-event worker.

## What the reviewer checks

The scan's guidelines live in **out-of-tree Markdown** at
[`crates/nogent-core/prompts/`](crates/nogent-core/prompts/) (editable without a
rebuild; override with `NOGENT_PROMPTS_DIR`, reuse across projects). They tell
the model **not** to duplicate what CI already runs (clippy with
`-D clippy::unwrap_used`, rustfmt, tests, `cargo audit`, commit-lint) and to
focus on semantic, sandbox-specific concerns: path footguns (`String` vs
`Path::starts_with`, missing canonicalization, TOCTOU), overly-broad
capabilities, silent security fallbacks, FFI/ABI breaks, Landlock-vs-Seatbelt
divergence, missing `EnvVarGuard` in tests, missing `// SAFETY:`, and DCO.

## Crates

| Crate | Role |
|-------|------|
| `nogent-core` | Shared, transport-light logic: webhook event types, repo config (fail-secure), prompts, canary output validator, diff bounding, HMAC verify. |
| `nogent-listener` | The app (axum). HMAC verify, App JWT + installation-token mint/cache, and the in-process GitHub + Gemini clients + review/triage orchestration. |

## Build & test

```bash
make build      # cargo build --release
make test       # cargo test --workspace
make ci         # clippy (strict) + fmt-check + tests
```

## Deploy

Full setup — GitHub App, secrets, the container image, AWS (Terraform or
manual), security-group ports, verification and operations — is in
**[DEPLOY.md](DEPLOY.md)**. nogent ships as a container
([`docker/`](docker/), published to GHCR by CI); a Caddy container terminates
TLS in front of it. Terraform lives in [`deploy/terraform/`](deploy/terraform/).

## License

Apache-2.0.
