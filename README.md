<p align="center">
  <img src="assets/logo.png" alt="nogent" width="200" />
</p>

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

**Triggers.** A PR is reviewed once when it's **opened / reopened / marked
ready** (not on every push — that would be noisy). To re-review the latest
commit, comment **`/nogent review`** on the PR. Issues are triaged on
open/edit/reopen. PRs authored by **bots** (`dependabot`, `renovate`,
`github-actions[bot]`, …) are skipped by default so a routine bump doesn't burn
a review — comment `/nogent review` to force one.

**Output.** Findings are ranked by **severity** (`high → medium → low`) and
carry a category emoji (🔒 security, 🐛 bug, 🏗️ design, 🧪 tests, ⚡ perf,
💬 nit) plus a `[SEV · category]` badge. Each finding with a line in the diff
is posted as an **inline review comment** anchored to that line and rendered as
a GitHub markdown **alert block** keyed off severity — `> [!CAUTION]` /
`> [!WARNING]` / `> [!NOTE]` (red / orange / blue stripe). Findings that can't
be anchored go in the review body alongside the one-line summary. It's a
`COMMENT` review — advisory, never blocking.

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
- **Bot-PR skip** — automated PRs from dependabot/renovate/etc. are ignored by
  default; a human comment is required to opt them in.

## Security & hardening

In addition to the app-layer defenses above, the published image and the
reference AWS deployment are hardened end-to-end:

**Container image** (`ghcr.io/nolabs-ai/nogent:vX.Y.Z`):
- Built from Chainguard's **distroless `glibc-dynamic`** — no shell, no package
  manager, no busybox in the runtime layer.
- Runs as **non-root** (uid `65532`).
- TLS via **rustls + ring**, with Mozilla roots baked into the binary
  (`webpki-roots`); no host CA trust, no OpenSSL in the runtime.
- **Signed with cosign** (keyless, OIDC), with SBOM and SLSA-provenance
  attestations published alongside each tagged release.

**Host & infrastructure** (`deploy/terraform/`):
- Secrets stored in **AWS Secrets Manager** and fetched at boot; never baked
  into the image, never committed to git. The IAM role attached to the host
  scopes `secretsmanager:GetSecretValue` to **one secret ARN**.
- **No SSH** by default — administer over **SSM Session Manager**.
- **IMDSv2 required** on the EC2 (blocks SSRF-style metadata theft).
- Security group: only **80/443 ingress** (for ACME + webhooks); egress
  restricted to HTTPS, DNS, and NTP.
- Listener binds **127.0.0.1** behind **Caddy**, which handles automatic
  Let's Encrypt TLS and is the only public surface.
- Bootstrap places the PEM at `0640` owned by uid `65532` (readable only by
  the container user) and the env file at `0600` root-only (read by the docker
  daemon, never by the container).

**Build & release pipeline** (`.github/workflows/image.yml`):
- Branches and PRs **build the image** as a sanity check but never push to GHCR.
- **Only `v*` tags publish** — push to GHCR, sign with cosign, attach SBOM and
  provenance. This makes `terraform apply` against a tag a fully attestable
  deploy.

> **Planned hardening:** running the per-event work inside a
> [nono](https://github.com/nolabs-ai/nono) sandbox so the process handling
> untrusted content holds only phantom credentials and can reach only
> allowlisted hosts. Removed for now to keep the deploy simple; the design slots
> back in as a per-event worker.

## What the reviewer checks

The scan does **general code review and security review**. Its guidelines live
in **out-of-tree Markdown** at
[`crates/nogent-core/prompts/`](crates/nogent-core/prompts/) (editable without a
rebuild; override with `NOGENT_PROMPTS_DIR`, reuse across projects). They tell
the model **not** to duplicate what CI already runs (clippy with
`-D clippy::unwrap_used`, rustfmt, tests, `cargo audit`, commit-lint) and to
cover correctness/bugs, design fit, error handling, readability, test coverage
and perf — plus sandbox-specific security: path footguns (`String` vs
`Path::starts_with`, missing canonicalization, TOCTOU), overly-broad
capabilities, silent security fallbacks, FFI/ABI breaks, Landlock-vs-Seatbelt
divergence, missing `EnvVarGuard` in tests, missing `// SAFETY:`, and DCO.

For token economy the model receives the **diff** plus **pre-resolved
definitions** of the symbols the diff references — not full file content (which
re-sending across the agentic turns was the dominant cost). It then **navigates
the repo at the PR head on demand** via tools — `definition` (symbol → its
source), `grep`, `read_file`, `list_files` — in a bounded loop (Gemini
function-calling), followed by **focused critique lenses** (coverage/parity,
sinks, bypass) run every review to reduce variance and catch what one pass
misses. The
repo is fetched once as a tarball into a bounded in-memory index (with a regex
symbol table; skips binaries/large files; over the cap it falls back to
diff-only). Tool results and pre-resolved defs are treated as untrusted content
under the same injection rules. Per-session token usage (incl. cached tokens) is
logged for cost tracking.

## Customizing the review

Two layers, for two audiences:

- **Per-repo guidance — `NOGENT.md`** (or `.github/nogent.md`) at the root of the
  reviewed repo. Freeform Markdown where *maintainers* state their conventions
  ("errors use `MyError`", "focus on `crypto/`", "ignore formatting"). nogent
  appends it to the reviewer's instructions. It's read from the **base branch**
  (never the PR head), so a fork PR can't change how it's reviewed; it can't
  override the output contract or injection rules. Capped at ~16 KB. See
  [`deploy/NOGENT.example.md`](deploy/NOGENT.example.md).
- **Per-repo knobs — `.github/nogent.json`** (structured): enable/disable each
  workflow and tune `maxFiles` / `maxPatchBytes` / `maxContextBytes`. See
  [`deploy/nogent.example.json`](deploy/nogent.example.json).
- **Operator-level base prompt — `NOGENT_PROMPTS_DIR`**: whoever *runs* nogent
  can replace the whole system prompt (the Markdown in
  [`crates/nogent-core/prompts/`](crates/nogent-core/prompts/)) without a rebuild.

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

## Evaluate the reviewer locally

Run the full review (real Gemini, real agentic navigation) against a local
checkout — no GitHub App, webhook, or posting:

```bash
make build
git diff origin/main... > /tmp/pr.diff      # any unified diff
GEMINI_API_KEY=<key> RUST_LOG=info \
  ./target/release/nogent-listener --review-local --repo . --diff /tmp/pr.diff
```

It indexes the repo at `--repo`, feeds the diff to the model, logs each tool call
(`grep`/`read_file`/`list_files`) as it navigates, prints a token-usage line
(`tokens: in=… out=… thinking=… (calls=…)`), and prints the Markdown review.
`--diff -` reads the diff from stdin. `GEMINI_MODEL` (default
`gemini-3.5-flash`) and `GEMINI_THINKING_LEVEL` (default `high`) override the
model and reasoning effort.

## Deploy

Full setup — GitHub App, secrets, the container image, AWS (Terraform or
manual), security-group ports, verification and operations — is in
**[DEPLOY.md](DEPLOY.md)**. nogent ships as a container
([`docker/`](docker/), published to GHCR by CI); a Caddy container terminates
TLS in front of it. Terraform lives in [`deploy/terraform/`](deploy/terraform/).

## Release

Releases are **tag-driven**. Pushes to `main` and PRs build the image to catch
breakage but never publish; only a `v*` tag triggers a GHCR push.

```bash
# 1. Land changes on main; make sure tests + clippy are clean.
make ci

# 2. Tag and push — CI builds, signs (cosign keyless), and publishes
#    ghcr.io/nolabs-ai/nogent:vX.Y.Z plus :X.Y plus :sha-<git-sha>.
git tag vX.Y.Z
git push origin vX.Y.Z

# 3. Roll the deploy: update `image` in deploy/terraform/terraform.tfvars,
#    then apply. `user_data_replace_on_change = true` recycles the EC2 with
#    the new image; the EIP (and so the GitHub webhook URL) stays.
cd deploy/terraform
terraform apply
```

Tags are immutable in GHCR — pulling `vX.Y.Z` later always gets the same
signed digest. The CI workflow attaches an SBOM and SLSA-provenance
attestation to each published tag.

## License

Apache-2.0.
