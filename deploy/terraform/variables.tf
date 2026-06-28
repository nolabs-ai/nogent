variable "region" {
  type        = string
  description = "AWS region to deploy into."
  default     = "eu-west-2"
}

variable "name" {
  type        = string
  description = "Name prefix for all resources."
  default     = "nogent"
}

variable "domain" {
  type        = string
  description = "Public DNS name for the webhook endpoint, e.g. nogent.example.com."
}

variable "hosted_zone_id" {
  type        = string
  description = "Route 53 hosted zone ID for `domain`. Leave empty to skip the DNS record (you manage DNS elsewhere)."
  default     = ""
}

variable "instance_type" {
  type        = string
  description = "EC2 instance type. t3.small is plenty for the listener."
  default     = "t3.small"
}

variable "admin_cidr" {
  type        = string
  description = "CIDR allowed to SSH (port 22). Leave empty to open NO SSH and rely on SSM Session Manager."
  default     = ""
}

variable "webhook_ingress_cidrs" {
  type        = list(string)
  description = "CIDRs allowed to reach 443/80. Default is open; tighten to GitHub's `hooks` ranges (api.github.com/meta) if you automate refresh."
  default     = ["0.0.0.0/0"]
}

variable "secret_name" {
  type        = string
  description = "Secrets Manager secret holding nogent's secrets as JSON keys: github_app_private_key, github_webhook_secret, gemini_api_key. Created empty by Terraform; set the value out-of-band."
  default     = "nogent/app"
}

variable "github_app_id" {
  type        = string
  description = "GitHub App ID (not secret)."
}

variable "gemini_model" {
  type        = string
  description = "Gemini model id."
  default     = "gemini-3.5-flash"
}

variable "gemini_thinking_level" {
  type        = string
  description = "Reasoning effort for Gemini 3.x models: minimal|low|medium|high. Empty to omit (e.g. for 2.5 models)."
  default     = "high"
}

variable "image" {
  type        = string
  description = "Fully-qualified nogent container image, e.g. ghcr.io/nolabs-ai/nogent:0.1.0."
}

variable "image_registry_auth" {
  type        = string
  sensitive   = true
  description = "Optional `docker login` credential for a private image registry, as 'user:token'. Leave empty for a public image."
  default     = ""
}

variable "acme_email" {
  type        = string
  description = "Contact email for Let's Encrypt (cert expiry notices)."
}

variable "tags" {
  type        = map(string)
  description = "Extra tags applied to all resources."
  default     = {}
}
