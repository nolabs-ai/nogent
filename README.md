<div align="center">

<img src="assets/logo.png" alt="nogent" width="600"/>

</div>

# nogent

`nogent` is a GitHub App that runs AI-powered **PR code review**.

**Triggers.** A PR is reviewed once when it's **opened / reopened / marked
ready** (not on every push — that would be noisy). To re-review the latest
commit, comment **`/nogent review`** on the PR. Issues are triaged on
open/edit/reopen.

PRs authored by **bots** (`dependabot`, `renovate`, `github-actions[bot]`, …)
are skipped by default so a routine bump doesn't burn a review — comment `/nogent review`
to force one.


# Security model and hardening
The nogent reviewer is **agentic** — it can read files, list directories, and
call external tools (e.g., `grep`) to navigate the repo and understand the diff. This
is powerful, but it also means that **prompt injection** is a real risk: a malicious
PR could try to trick the model into revealing secrets or taking unsafe actions. 

To mitigate this, nogent implements several layers of defense:

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

In addition to the app-layer defenses above, the published image is signed and verified,
and the runtime is hardened:

**Container image** (`ghcr.io/nolabs-ai/nogent:vX.Y.Z`):
- Built from Chainguard's **distroless `glibc-dynamic`** — no shell, no package
  manager, no busybox in the runtime layer.
- Runs as **non-root** (uid `65532`).
- TLS via **rustls + ring**, with Mozilla roots baked into the binary
  (`webpki-roots`); no host CA trust, no OpenSSL in the runtime.
- **Signed with cosign** (keyless, OIDC), with SBOM and SLSA-provenance
  attestations published alongside each tagged release.

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

## Release

Releases are **tag-driven**. Pushes to `main` and PRs build the image to catch
breakage but never publish; only a `v*` tag triggers a GHCR push.

The version bump touches **four places** — keep them in lockstep so the
`v*` tag, the published image, the binary's reported version, and the
deployed instance all agree:

```bash
# 1. Land changes on main; make sure tests + clippy are clean.
make ci

# 2. Bump version in both crates' Cargo.toml.
#    (workspace doesn't share `version`; each crate carries its own.)
$EDITOR crates/nogent-core/Cargo.toml \
        crates/nogent-listener/Cargo.toml

# 3. Add a CHANGELOG.md entry under a new `## [X.Y.Z] — YYYY-MM-DD` heading,
#    summarising user-visible changes since the previous tag. Move the
#    `[Unreleased]` link to compare from the new tag.
$EDITOR CHANGELOG.md

# 4. Commit the bump, tag, and push — CI builds, signs (cosign keyless),
#    and publishes ghcr.io/nolabs-ai/nogent:X.Y.Z plus :X.Y plus :sha-<git-sha>.
git add crates/*/Cargo.toml CHANGELOG.md
git commit -s -m "chore(release): vX.Y.Z"
git tag vX.Y.Z
git push origin main vX.Y.Z

# 5. Roll the deploy: bump `image` in deploy/terraform/terraform.tfvars to
#    the new tag, then apply. `user_data_replace_on_change = true` recycles
#    the EC2 with the new image; the EIP (and the GitHub webhook URL) stays.
$EDITOR deploy/terraform/terraform.tfvars
cd deploy/terraform
terraform apply
```

Tags are immutable in GHCR — pulling `vX.Y.Z` later always gets the same
signed digest. The CI workflow attaches BuildKit SBOM and SLSA-provenance
attestations to each published tag, inspectable with `docker buildx
imagetools inspect`.

## License

Apache-2.0.
