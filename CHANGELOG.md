# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.2] - 2026-06-30

### Changed
- Only make a single comment in issues

## [0.2.0] — 2026-06-28

### Added
- **GitHub alert blocks** for inline review comments: severity drives the
  block keyword (`> [!CAUTION]` for `high`, `> [!WARNING]` for `medium`,
  `> [!NOTE]` for `low`), so a finding lands as a colored stripe rather than
  a plain bold badge.
- **Category emoji** prepended to every finding badge (🔒 security, 🐛 bug,
  🏗️ design, 🧪 tests, ⚡ perf, 💬 nit, 📝 fallback) — quick visual scanning
  in dense reviews.
- **Bot-PR skip**: `pull_request` events where the author is a GitHub Bot
  account (`dependabot`, `renovate`, `github-actions[bot]`, …) no longer
  trigger an auto-review. `/nogent review` from a human still works for the
  occasional bot PR you actually want assessed.
- `Actor.user_type` parsed from GitHub's `type` field, with
  `Actor::is_bot()` helper.

### Changed
- CI workflow (`.github/workflows/image.yml`) now **only publishes to GHCR on
  `v*` tags**. Pushes to `main` and PRs still build the image (Dockerfile
  sanity check) but don't push, sign, or attest. Cuts registry noise and
  scopes the cosign signature to released artifacts.

### Fixed
- Terraform `data "aws_subnets" "default"` now filters for
  `map-public-ip-on-launch=true`, so the EC2 lands in a subnet with an IGW
  route. Previous default picked any subnet in the VPC and could deploy into
  a private one with no outbound network.
- `aws_instance.nogent` sets `associate_public_ip_address = true` so the
  instance has outbound networking from t=0, independent of when the EIP
  attaches.
- User-data chowns the PEM to `65532:65532` (the Chainguard distroless
  container user) and chmods `/etc/nogent` to `0755`; without this the
  non-root container couldn't read the bind-mounted private key.
- Comment-escaping in `user_data.sh.tftpl` (literal `${...}` in a comment
  was being parsed as a Terraform template expression).

## [0.1.0] — 2026-06-26

Initial release.

### Added
- HTTP webhook listener (`axum`) with constant-time HMAC verification of
  every payload.
- GitHub App auth: RS256 App JWT, installation-token mint with in-memory
  short-lived cache, App authenticates as itself (works for fork PRs).
- **PR review** pipeline: bounded diff digest, pre-resolved symbol
  definitions, agentic Gemini loop with `definition` / `grep` / `read_file`
  / `list_files` tools over a tarball repo snapshot at the PR head,
  followed by deterministic focused critique lenses
  (coverage/sinks/bypass) for variance reduction.
- **Issue triage** pipeline.
- Canary-gated JSON output as the prompt-injection backstop; on validation
  failure, posts a fixed "manual review needed" comment.
- **Per-repo guidance**: `NOGENT.md` / `.github/nogent.md` read from the
  base ref, appended to the system prompt (capped ~16 KB).
- **Per-repo knobs**: `.github/nogent.json` (`maxFiles`, `maxPatchBytes`,
  `maxContextBytes`, per-workflow enable flags) with fail-secure parse.
- **Operator-level prompt override** via `NOGENT_PROMPTS_DIR` (the system
  prompts ship as out-of-tree Markdown in `crates/nogent-core/prompts/`).
- Inline review comments anchored to diff lines, with body fallback for
  un-anchorable findings.
- Severity (`high` / `medium` / `low`) on findings, sorted highest first,
  with truncation salvage if the model's JSON gets cut off.
- Container image (Chainguard distroless `glibc-dynamic`, non-root, rustls
  + ring, no host CA trust), GHCR publish with cosign signature + BuildKit
  SBOM and SLSA-provenance attestations.
- Terraform deploy (`deploy/terraform/`) for AWS — EC2, EIP, IAM role,
  Secrets Manager secret, security group, IMDSv2-required, Caddy fronting
  the listener.
- `--review-local` mode for evaluating the reviewer end-to-end against a
  local checkout (real Gemini, no GitHub).

[Unreleased]: https://github.com/nolabs-ai/nogent/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/nolabs-ai/nogent/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/nolabs-ai/nogent/releases/tag/v0.1.0
