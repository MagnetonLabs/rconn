# rconn self-hosted server on Azure: one Linux VM, a static public IP, an NSG.
#   tofu init && tofu apply -var location=eastus -var admin_ssh_key="$(cat ~/.ssh/id_ed25519.pub)"

terraform {
  required_providers {
    azurerm = { source = "hashicorp/azurerm", version = "~> 4.0" }
  }
}

variable "location" { type = string }
variable "vm_size" {
  type    = string
  default = "Standard_B1ms"
}
variable "hostname" {
  type    = string
  default = ""
}
variable "admin_ssh_key" {
  description = "SSH public key for the VM admin user (Azure requires one)."
  type        = string
}

provider "azurerm" {
  features {}
}

resource "azurerm_resource_group" "rconn" {
  name     = "rconn"
  location = var.location
}

resource "azurerm_virtual_network" "rconn" {
  name                = "rconn"
  address_space       = ["10.42.0.0/16"]
  location            = azurerm_resource_group.rconn.location
  resource_group_name = azurerm_resource_group.rconn.name
}

resource "azurerm_subnet" "rconn" {
  name                 = "rconn"
  resource_group_name  = azurerm_resource_group.rconn.name
  virtual_network_name = azurerm_virtual_network.rconn.name
  address_prefixes     = ["10.42.1.0/24"]
}

resource "azurerm_public_ip" "rconn" {
  name                = "rconn"
  location            = azurerm_resource_group.rconn.location
  resource_group_name = azurerm_resource_group.rconn.name
  allocation_method   = "Static"
  sku                 = "Standard"
}

resource "azurerm_network_security_group" "rconn" {
  name                = "rconn"
  location            = azurerm_resource_group.rconn.location
  resource_group_name = azurerm_resource_group.rconn.name
  security_rule {
    name                       = "signaling"
    priority                   = 100
    direction                  = "Inbound"
    access                     = "Allow"
    protocol                   = "Tcp"
    source_port_range          = "*"
    destination_port_range     = "443"
    source_address_prefix      = "*"
    destination_address_prefix = "*"
  }
  security_rule {
    name                       = "turn"
    priority                   = 110
    direction                  = "Inbound"
    access                     = "Allow"
    protocol                   = "Udp"
    source_port_range          = "*"
    destination_port_ranges    = ["3478", "49160-49260"]
    source_address_prefix      = "*"
    destination_address_prefix = "*"
  }
}

resource "azurerm_network_interface" "rconn" {
  name                = "rconn"
  location            = azurerm_resource_group.rconn.location
  resource_group_name = azurerm_resource_group.rconn.name
  ip_configuration {
    name                          = "primary"
    subnet_id                     = azurerm_subnet.rconn.id
    private_ip_address_allocation = "Dynamic"
    public_ip_address_id          = azurerm_public_ip.rconn.id
  }
}

resource "azurerm_network_interface_security_group_association" "rconn" {
  network_interface_id      = azurerm_network_interface.rconn.id
  network_security_group_id = azurerm_network_security_group.rconn.id
}

resource "azurerm_linux_virtual_machine" "rconn" {
  name                  = "rconn"
  resource_group_name   = azurerm_resource_group.rconn.name
  location              = azurerm_resource_group.rconn.location
  size                  = var.vm_size
  admin_username        = "rconnadmin"
  network_interface_ids = [azurerm_network_interface.rconn.id]
  admin_ssh_key {
    username   = "rconnadmin"
    public_key = var.admin_ssh_key
  }
  os_disk {
    caching              = "ReadWrite"
    storage_account_type = "Standard_LRS"
  }
  source_image_reference {
    publisher = "Canonical"
    offer     = "ubuntu-24_04-lts"
    sku       = "server"
    version   = "latest"
  }
  custom_data = base64encode(templatefile("${path.module}/../cloud-init.sh.tftpl", {
    install_sh = file("${path.module}/../../install.sh")
    hostname   = var.hostname
  }))
}

output "server_url" { value = "wss://${var.hostname != "" ? var.hostname : azurerm_public_ip.rconn.ip_address}:443/v1/ws" }
output "public_ip" { value = azurerm_public_ip.rconn.ip_address }
