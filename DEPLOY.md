# Deploying nogent

Everything required to stand up nogent end-to-end. Two paths share the same
setup (sections 1–4); pick **A (Terraform)** or **B (manual)** for the host.

- **A — Terraform** (`deploy/terraform/`): provisions the security group, Elastic
  IP, optional Route 53 record, IAM, Secrets Manager, and an EC2 instance whose
  user-data does all host config. Recommended.
- **B — Manual**: the same thing by hand on an instance you already have.

Either way the runtime shape is: **Caddy** terminates TLS and reverse-proxies to
the **listener** on `127.0.0.1:8080`; the listener does the review/triage
in-process. See the README for the security rationale.

---

## 1. Prerequisites

- An AWS account + the `aws` CLI configured (for path A also `terraform >= 1.5`).
- A **DNS name** you control for the webhook endpoint (e.g. `nogent.example.com`)
  and the ability to create an A record for it.
- A **Gemini API key**.
- Permission to create a **GitHub App** on the target account/org.
- A Linux host/AMI with a **container runtime** + compose (Docker, or
  Podman + `podman compose`). Amazon Linux 2023 is fine.

---

## 2. The container image

nogent ships as a container image. CI builds it on each `v*` tag and pushes to
**GHCR** (`ghcr.io/nolabs-ai/nogent`) with an SBOM, provenance, and a
keyless cosign signature — see [`docker/`](docker/) and
`.github/workflows/image.yml`. The image is built on Chainguard's Rust image and
runs on distroless `glibc-dynamic` (nonroot, no shell); CA roots are baked into
the binary, so it needs no `ca-certificates` layer.

Tag a release to publish:

```bash
git tag v0.1.0 && git push origin v0.1.0     # → ghcr.io/.../nogent:0.1.0
```

Or build locally and push by hand:

```bash
docker build -f docker/Dockerfile -t ghcr.io/nolabs-ai/nogent:0.1.0 .
docker push ghcr.io/nolabs-ai/nogent:0.1.0
```

On the host you run two containers — the image above + stock `caddy:2` — via
[`docker/compose.yaml`](docker/compose.yaml). Caddy is the only thing that binds
public ports; the listener is reachable only on the internal compose network.

> If the GHCR package is **private**, the host needs a pull credential
> (`docker login ghcr.io` with a read-only PAT, or an ECR pull-through cache).
> Public packages need nothing.

---

## 3. Create & install the GitHub App

You need a **GitHub App** (not an OAuth app, not a PAT). The App has its own
installation token, which is why nogent works on **fork PRs** without exposing
any repo/CI secret.

### 3.1 Create

**Settings → Developer settings → GitHub Apps → New GitHub App** (org: **Org
settings → Developer settings → GitHub Apps → New**):

| Field | Value |
|-------|-------|
| **GitHub App name** | e.g. `nogent-<yourorg>` (globally unique) |
| **Homepage URL** | your repo or site |
| **Webhook → Active** | ✅ |
| **Webhook URL** | `https://<your-domain>/api/github/webhooks` |
| **Webhook secret** | `openssl rand -hex 32` — keep it |
| **Where can this be installed** | *Only on this account* |

**Repository permissions** (least privilege — exactly these):

| Permission | Access |
|------------|--------|
| Contents | Read-only |
| Issues | Read & write |
| Pull requests | Read & write |
| Metadata | Read-only (auto) |

**Subscribe to events:** ✅ *Pull request*, ✅ *Issues*. Click **Create**.

(Values mirror [`deploy/github-app-manifest.json`](deploy/github-app-manifest.json)
if you prefer the manifest flow; set `hook_attributes.url` + `redirect_url` first.)

### 3.2 Capture credentials

- **App ID** — top of the App's *General* page.
- **Private key** — *Private keys → Generate a private key*; a `.pem` downloads
  (PKCS#1 `-----BEGIN RSA PRIVATE KEY-----`; nogent accepts it).
- **Webhook secret** — the value you set above.

### 3.3 Install

App settings → **Install App** → the account → **Only select repositories** →
pick the target repo(s). Installation is what lets nogent mint scoped tokens.

---

## 4. Configuration & secrets reference

The listener reads these (see [`.env.example`](.env.example)):

| Var | Secret? | Notes |
|-----|:------:|-------|
| `GITHUB_APP_ID` | no | numeric App ID |
| `GITHUB_APP_PRIVATE_KEY` *or* `GITHUB_APP_PRIVATE_KEY_FILE` | **yes** | inline PEM (`\n`-escaped) **or** path to a PEM file. Deploys use the file form. |
| `GITHUB_WEBHOOK_SECRET` | **yes** | the `openssl rand` value |
| `GEMINI_API_KEY` | **yes** | Gemini key |
| `GEMINI_MODEL` | no | default `gemini-2.5-pro` |
| `NOGENT_BIND_ADDR` | no | default `127.0.0.1:8080` |
| `NOGENT_MAX_BODY_BYTES` | no | webhook body cap, default 2 MiB |
| `NOGENT_PROMPTS_DIR` | no | dir of Markdown system prompts to use instead of the embedded ones (see `crates/nogent-core/prompts/`) |

For AWS, store the three secrets as a **Secrets Manager JSON** with these keys
(this is exactly what the Terraform user-data expects):

```json
{
  "github_app_private_key": "-----BEGIN RSA PRIVATE KEY-----\n...\n-----END RSA PRIVATE KEY-----\n",
  "github_webhook_secret": "<openssl rand -hex 32>",
  "gemini_api_key": "<gemini key>"
}
```

---

## Path A — Terraform

Terraform provisions the instance and its user-data installs a container
runtime, writes `docker/compose.yaml` + `Caddyfile`, and `compose up`s the
image.

```bash
cd deploy/terraform

cat > terraform.tfvars <<'EOF'
domain         = "nogent.example.com"
hosted_zone_id = "Z0123456789ABCDEFGHIJ"   # "" to manage DNS yourself
github_app_id  = "123456"
image          = "ghcr.io/nolabs-ai/nogent:0.1.0"
acme_email     = "ops@example.com"
admin_cidr     = ""                          # "" = no SSH, use SSM
region         = "eu-west-2"
EOF

terraform init
terraform apply
```

Then set the secret value (Terraform deliberately does **not** — state is
plaintext, so it manages only the empty secret + IAM grant):

```bash
aws secretsmanager put-secret-value \
  --secret-id "$(terraform output -raw secret_arn)" \
  --secret-string "$(jq -n \
      --rawfile k /path/to/app-private-key.pem \
      --arg w "$WEBHOOK_SECRET" --arg g "$GEMINI_API_KEY" \
      '{github_app_private_key:$k, github_webhook_secret:$w, gemini_api_key:$g}')"
```

Re-run bootstrap so it picks up the secret (the first boot may have run before
the value existed):

```bash
aws ssm start-session --target "$(terraform output -raw instance_id)"
#   sudo cloud-init clean && sudo cloud-init init     # re-run user-data
#   (or just: sudo reboot)
```

If `hosted_zone_id` was empty, point your DNS A record at
`terraform output -raw public_ip`. Finally confirm the App's webhook URL matches
`terraform output -raw webhook_url`. See
[`deploy/terraform/README.md`](deploy/terraform/README.md) for variables + VPC
notes.

---

## Path B — Manual on an instance (compose)

1. **Instance:** Linux with Docker (or Podman) + compose; assign an **Elastic
   IP**; create the **DNS A record** → that IP. Keep `chronyd` running (the App
   JWT has a short `iat/exp` window).
2. **Secrets/config on the host:**
   ```bash
   sudo install -d -m 0750 /etc/nogent
   sudo install -m 0600 app-private-key.pem /etc/nogent/app-private-key.pem
   # write /etc/nogent/nogent.env (0600) — see .env.example, with
   # GITHUB_APP_PRIVATE_KEY_FILE=/etc/nogent/app-private-key.pem
   # NOGENT_BIND_ADDR is forced to 0.0.0.0:8080 by compose.
   ```
3. **Compose files:** copy [`docker/compose.yaml`](docker/compose.yaml) and
   [`docker/Caddyfile`](docker/Caddyfile) to the host; edit the Caddyfile domain
   + email. If the GHCR package is private, `docker login ghcr.io` first.
4. **Up:**
   ```bash
   IMAGE_TAG=0.1.0 docker compose -f docker/compose.yaml up -d
   ```
   Only Caddy's 80/443 are published; the listener stays on the internal network.
5. **Webhook URL:** set it to `https://<your-domain>/api/github/webhooks`.

> The pre-container systemd path (`deploy/nogent-listener.service` +
> `deploy/Caddyfile`, running the raw binary) still works if you prefer no
> container runtime; it's kept as an alternative.

---

## 5. Security group / ports

The listener never binds a public port — Caddy is the only public listener.

| Direction | Port | Source / Dest | Why |
|-----------|------|---------------|-----|
| Ingress | **443/tcp** | `0.0.0.0/0` (or GitHub `hooks` ranges from `api.github.com/meta`) | webhook delivery (Caddy) |
| Ingress | **80/tcp** | `0.0.0.0/0` | Let's Encrypt HTTP-01. Omit with a DNS-01 challenge. |
| Ingress | **22/tcp** | your admin IP only | SSH — prefer **SSM** and open nothing |
| Egress | **443/tcp** | GitHub, Gemini (+ Secrets Manager, ACME) | token mint, model calls, cert issuance |
| Egress | **53 udp/tcp** | VPC resolver | DNS (only if you restrict egress; SGs are stateful) |
| Egress | **123/udp** | NTP | clock; Amazon Time Sync needs no rule but other NTP does |

- Pinning ingress 443 to GitHub's `hooks` CIDRs is stronger but those ranges
  change; `0.0.0.0/0` is fine because the **HMAC check is the real gate**.
- Restricting egress to GitHub + Gemini (+ Secrets Manager/ACME) limits where a
  compromised process can reach — worth doing since the listener holds real
  credentials.

---

## 6. Verify

```bash
curl https://<your-domain>/healthz          # → ok
```

- Open a PR or issue on an installed repo → a review/triage comment appears.
- Replay deterministically: App → **Advanced → Recent Deliveries → Redeliver**.
- Check logs: `journalctl -u nogent-listener -f` shows HMAC rejects, the minted
  installation token use, and the posted comment.

---

## 7. Per-repo configuration (optional)

Add [`.github/nogent.json`](deploy/nogent.example.json) to a target repo to
tune limits or disable a workflow:

```json
{
  "enabled": true,
  "issueTriage": { "enabled": true },
  "pullRequestSecurityReview": { "enabled": true, "maxFiles": 25, "maxPatchBytes": 120000 }
}
```

A **malformed** config is fail-secure: nogent skips the event rather than
falling back to "enabled".

---

## 8. Operations

- **Logs:** `journalctl -u nogent-listener -f`. Get a shell with
  `aws ssm start-session --target <instance-id>` — no SSH needed.
- **Secret rotation:** update the Secrets Manager value (or `/etc/nogent/*`),
  then restart the listener container (`docker compose restart nogent`) —
  secrets are read at startup. Rotate the App private key + webhook secret +
  Gemini key periodically.
- **Upgrading:** tag a release (CI publishes the image), then bump `IMAGE_TAG`
  and `docker compose up -d` (path B) or re-apply Terraform with the new `image`
  (path A). The installation-token cache is in-memory and rebuilt automatically.
- **Clock:** keep `chronyd` running; JWT auth fails if the clock drifts.

---

## 9. Caveats

- The listener holds the real installation token + Gemini key in memory while
  processing untrusted content; there is no sandbox boundary in this version.
  Restrict egress (section 5), keep the host minimal, and treat re-introducing
  the nono sandbox as the next hardening step.
- nogent fetches only the first 100 changed files of very large PRs and bounds
  the diff to `maxFiles`/`maxPatchBytes`; truncation is logged and marked in the
  prompt.
