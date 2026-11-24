# rconn self-hosted server on Google Cloud: one Compute Engine VM, a static IP, firewall.
#   tofu init && tofu apply -var project=my-project -var region=us-central1

terraform {
  required_providers {
    google = { source = "hashicorp/google", version = "~> 6.0" }
  }
}

variable "project" { type = string }
variable "region" { type = string }
variable "zone_suffix" {
  type    = string
  default = "a"
}
variable "machine_type" {
  type    = string
  default = "e2-small"
}
variable "hostname" {
  type    = string
  default = ""
}

provider "google" {
  project = var.project
  region  = var.region
}

resource "google_compute_address" "rconn" { name = "rconn" }

resource "google_compute_firewall" "rconn" {
  name          = "rconn"
  network       = "default"
  target_tags   = ["rconn"]
  source_ranges = ["0.0.0.0/0"]
  allow {
    protocol = "tcp"
    ports    = ["443"]
  }
  allow {
    protocol = "udp"
    ports    = ["3478", "49160-49260"]
  }
}

resource "google_compute_instance" "rconn" {
  name         = "rconn"
  machine_type = var.machine_type
  zone         = "${var.region}-${var.zone_suffix}"
  tags         = ["rconn"]
  boot_disk {
    initialize_params { image = "ubuntu-os-cloud/ubuntu-2404-lts-amd64" }
  }
  network_interface {
    network = "default"
    access_config { nat_ip = google_compute_address.rconn.address }
  }
  metadata = {
    startup-script = templatefile("${path.module}/../cloud-init.sh.tftpl", {
      install_sh = file("${path.module}/../../install.sh")
      hostname   = var.hostname
    })
  }
  shielded_instance_config { enable_secure_boot = true }
}

output "server_url" { value = "wss://${var.hostname != "" ? var.hostname : google_compute_address.rconn.address}:443/v1/ws" }
output "public_ip" { value = google_compute_address.rconn.address }
