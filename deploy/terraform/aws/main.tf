# rconn self-hosted server on AWS: one EC2 VM, an Elastic IP, and a security group.
#   tofu init && tofu apply -var region=us-east-1 [-var hostname=rconn.example.com]
# Outputs the server URL; the server pin and admin password are in the instance's
# console output / /opt/rconn/init.txt (install.sh prints them).

terraform {
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 5.0" }
  }
}

variable "region" { type = string }
variable "instance_type" {
  type    = string
  default = "t3.small"
}
variable "hostname" {
  description = "DNS name clients use; defaults to the Elastic IP."
  type        = string
  default     = ""
}
variable "ssh_cidr" {
  description = "CIDR allowed to SSH (empty: no SSH)."
  type        = string
  default     = ""
}

provider "aws" { region = var.region }

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical
  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }
}

resource "aws_security_group" "rconn" {
  name_prefix = "rconn-"
  description = "rconn signaling (TLS) and TURN relay"
  ingress {
    description = "signaling"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  ingress {
    description = "TURN"
    from_port   = 3478
    to_port     = 3478
    protocol    = "udp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  ingress {
    description = "TURN relay range"
    from_port   = 49160
    to_port     = 49260
    protocol    = "udp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  dynamic "ingress" {
    for_each = var.ssh_cidr == "" ? [] : [var.ssh_cidr]
    content {
      description = "ssh"
      from_port   = 22
      to_port     = 22
      protocol    = "tcp"
      cidr_blocks = [ingress.value]
    }
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_eip" "rconn" { domain = "vpc" }

resource "aws_instance" "rconn" {
  ami                    = data.aws_ami.ubuntu.id
  instance_type          = var.instance_type
  vpc_security_group_ids = [aws_security_group.rconn.id]
  metadata_options { http_tokens = "required" } # IMDSv2 only
  user_data = templatefile("${path.module}/../cloud-init.sh.tftpl", {
    install_sh = file("${path.module}/../../install.sh")
    hostname   = var.hostname
  })
  tags = { Name = "rconn" }
}

resource "aws_eip_association" "rconn" {
  instance_id   = aws_instance.rconn.id
  allocation_id = aws_eip.rconn.id
}

output "server_url" { value = "wss://${var.hostname != "" ? var.hostname : aws_eip.rconn.public_ip}:443/v1/ws" }
output "public_ip" { value = aws_eip.rconn.public_ip }
