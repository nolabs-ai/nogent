locals {
  tags = merge({ app = var.name }, var.tags)
}

# ── AMI + network (default VPC for simplicity; override the data sources if you
#    run in a dedicated VPC) ────────────────────────────────────────────────
data "aws_ssm_parameter" "al2023" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"
}

data "aws_vpc" "default" {
  default = true
}

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

# ── Security group ──────────────────────────────────────────────────────────
resource "aws_security_group" "nogent" {
  name        = "${var.name}-sg"
  description = "nogent listener host: 443/80 in (Caddy), 443/53/123 out."
  vpc_id      = data.aws_vpc.default.id
  tags        = local.tags
}

# Webhooks (Caddy terminates TLS).
resource "aws_security_group_rule" "in_https" {
  type              = "ingress"
  security_group_id = aws_security_group.nogent.id
  protocol          = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_blocks       = var.webhook_ingress_cidrs
  description       = "GitHub webhook delivery"
}

# Let's Encrypt HTTP-01. Remove if you switch Caddy to a DNS-01 challenge.
resource "aws_security_group_rule" "in_http_acme" {
  type              = "ingress"
  security_group_id = aws_security_group.nogent.id
  protocol          = "tcp"
  from_port         = 80
  to_port           = 80
  cidr_blocks       = var.webhook_ingress_cidrs
  description       = "ACME HTTP-01 cert issuance"
}

# Optional SSH; omitted entirely when admin_cidr is empty (use SSM instead).
resource "aws_security_group_rule" "in_ssh" {
  count             = var.admin_cidr == "" ? 0 : 1
  type              = "ingress"
  security_group_id = aws_security_group.nogent.id
  protocol          = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_blocks       = [var.admin_cidr]
  description       = "Admin SSH"
}

# Egress: HTTPS (GitHub, Gemini, Secrets Manager, ACME), DNS, NTP. SGs are
# stateful, so only outbound-initiation ports are needed.
resource "aws_security_group_rule" "out_https" {
  type              = "egress"
  security_group_id = aws_security_group.nogent.id
  protocol          = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_blocks       = ["0.0.0.0/0"]
  description       = "HTTPS to GitHub / Gemini / Secrets Manager / ACME"
}

resource "aws_security_group_rule" "out_dns_udp" {
  type              = "egress"
  security_group_id = aws_security_group.nogent.id
  protocol          = "udp"
  from_port         = 53
  to_port           = 53
  cidr_blocks       = ["0.0.0.0/0"]
  description       = "DNS"
}

resource "aws_security_group_rule" "out_dns_tcp" {
  type              = "egress"
  security_group_id = aws_security_group.nogent.id
  protocol          = "tcp"
  from_port         = 53
  to_port           = 53
  cidr_blocks       = ["0.0.0.0/0"]
  description       = "DNS (TCP fallback)"
}

resource "aws_security_group_rule" "out_ntp" {
  type              = "egress"
  security_group_id = aws_security_group.nogent.id
  protocol          = "udp"
  from_port         = 123
  to_port           = 123
  cidr_blocks       = ["0.0.0.0/0"]
  description       = "NTP (clock accuracy for App JWT iat/exp)"
}

# ── Secrets Manager (container only; set the value out-of-band) ──────────────
resource "aws_secretsmanager_secret" "nogent" {
  name        = var.secret_name
  description = "nogent secrets: github_app_private_key, github_webhook_secret, gemini_api_key (JSON). Value set out-of-band, NOT by Terraform."
  tags        = local.tags
}

# ── IAM: SSM + read-only access to the one secret ───────────────────────────
data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "nogent" {
  name               = "${var.name}-role"
  assume_role_policy = data.aws_iam_policy_document.assume.json
  tags               = local.tags
}

# SSM Session Manager so you can get a shell with no SSH port open.
resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.nogent.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

data "aws_iam_policy_document" "secret_read" {
  statement {
    actions   = ["secretsmanager:GetSecretValue"]
    resources = [aws_secretsmanager_secret.nogent.arn]
  }
}

resource "aws_iam_role_policy" "secret_read" {
  name   = "${var.name}-secret-read"
  role   = aws_iam_role.nogent.id
  policy = data.aws_iam_policy_document.secret_read.json
}

resource "aws_iam_instance_profile" "nogent" {
  name = "${var.name}-profile"
  role = aws_iam_role.nogent.name
}

# ── Instance ────────────────────────────────────────────────────────────────
resource "aws_instance" "nogent" {
  ami                    = data.aws_ssm_parameter.al2023.value
  instance_type          = var.instance_type
  subnet_id              = data.aws_subnets.default.ids[0]
  vpc_security_group_ids = [aws_security_group.nogent.id]
  iam_instance_profile   = aws_iam_instance_profile.nogent.name

  user_data = templatefile("${path.module}/user_data.sh.tftpl", {
    region              = var.region
    secret_arn          = aws_secretsmanager_secret.nogent.arn
    domain              = var.domain
    acme_email          = var.acme_email
    github_app_id       = var.github_app_id
    gemini_model        = var.gemini_model
    image               = var.image
    image_registry_auth = var.image_registry_auth
  })
  # Re-provision when user-data changes.
  user_data_replace_on_change = true

  metadata_options {
    http_tokens   = "required" # IMDSv2 only
    http_endpoint = "enabled"
  }

  root_block_device {
    encrypted   = true
    volume_size = 20
    volume_type = "gp3"
  }

  tags = merge(local.tags, { Name = var.name })
}

resource "aws_eip" "nogent" {
  instance = aws_instance.nogent.id
  domain   = "vpc"
  tags     = local.tags
}

resource "aws_route53_record" "nogent" {
  count   = var.hosted_zone_id == "" ? 0 : 1
  zone_id = var.hosted_zone_id
  name    = var.domain
  type    = "A"
  ttl     = 300
  records = [aws_eip.nogent.public_ip]
}
