# nogent Terraform (single EC2 instance)

Provisions the single-instance deploy from the top-level README: a security
group (the exact port table), an Elastic IP, an optional Route 53 A record, an
IAM role (SSM + read-only on one secret), a Secrets Manager secret *container*,
and the EC2 instance whose user-data installs Docker, renders config, and
`docker compose up`s the nogent + Caddy containers.

## What Terraform does NOT manage

- **Secret values.** Terraform state is plaintext, so the secret value is set
  out-of-band (below). Terraform only creates the empty secret + the IAM grant.
- **The image.** CI builds + pushes `ghcr.io/nolabs-ai/nogent:<tag>` (see
  `.github/workflows/image.yml`). Pass the tag as `image`. For a private
  registry, put the `user:token` credential under the optional
  `image_registry_auth` key of the Secrets Manager secret (below) — it is
  intentionally NOT a Terraform variable, because user-data is served by IMDS
  in cleartext to any process on the box.

## Usage

```bash
cd deploy/terraform

cat > terraform.tfvars <<'EOF'
domain         = "nogent.example.com"
hosted_zone_id = "Z0123456789ABCDEFGHIJ"   # "" to manage DNS yourself
github_app_id  = "123456"
image          = "ghcr.io/nolabs-ai/nogent:0.1.0"
acme_email     = "ops@example.com"
admin_cidr     = ""                          # "" = no SSH, use SSM
EOF

terraform init
terraform apply

# Set the secret value (NOT in Terraform). Private key is the raw PEM. Add the
# optional `image_registry_auth` key ("user:token") only if pulling from a
# private registry — omit it for public images.
aws secretsmanager put-secret-value \
  --secret-id "$(terraform output -raw secret_arn)" \
  --secret-string "$(jq -n \
      --rawfile k /path/to/app-private-key.pem \
      --arg w "$WEBHOOK_SECRET" --arg g "$GEMINI_API_KEY" \
      --arg r "${IMAGE_REGISTRY_AUTH:-}" \
      '{github_app_private_key:$k, github_webhook_secret:$w, gemini_api_key:$g}
        + (if $r == "" then {} else {image_registry_auth:$r} end)')"

# Re-run bootstrap so it picks up the secret (or just reboot the instance):
aws ssm start-session --target "$(terraform output -raw instance_id)"
#   sudo cloud-init clean && sudo cloud-init init  # or: sudo reboot
```

Then set the GitHub App webhook URL to `terraform output -raw webhook_url`.

## Notes

- Uses the **default VPC** for simplicity; edit the `aws_vpc`/`aws_subnets`
  data sources in `main.tf` to target a dedicated VPC.
- `webhook_ingress_cidrs` defaults to `0.0.0.0/0`; tighten to GitHub's `hooks`
  ranges (`api.github.com/meta`) if you automate refreshing them. The webhook
  HMAC check is the real authentication gate regardless.
- SSM Session Manager is enabled (the role has `AmazonSSMManagedInstanceCore`),
  so you can get a shell with no SSH port open. Leave `admin_cidr = ""`.
- The instance enforces IMDSv2 and an encrypted gp3 root volume.
- Secret rotation: update the secret value, then restart the listener
  (`systemctl restart nogent-listener` via SSM) — it reads secrets at startup.
