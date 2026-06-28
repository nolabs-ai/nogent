output "public_ip" {
  description = "Elastic IP of the instance. Point your DNS A record here if hosted_zone_id was not set."
  value       = aws_eip.nogent.public_ip
}

output "webhook_url" {
  description = "Set this as the GitHub App webhook URL."
  value       = "https://${var.domain}/api/github/webhooks"
}

output "secret_arn" {
  description = "Set the secret value out-of-band, e.g. `aws secretsmanager put-secret-value --secret-id <this> --secret-string '{...}'`."
  value       = aws_secretsmanager_secret.nogent.arn
}

output "instance_id" {
  description = "EC2 instance id (use with `aws ssm start-session --target <id>`)."
  value       = aws_instance.nogent.id
}
