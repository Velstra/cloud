# Velstra Cloud

A virtualisation platform for people who want OpenStack's separation without
OpenStack's weight, and Proxmox's directness without its ceiling.

Everything the platform decides, it says out loud: what was asked for and what
is actually running are two columns, never one; a guest that will not place
names the rule that stopped it; a backup says whether anybody has read it back.

---

## Start here

**[Quick start](quickstart.md)** — one machine, from nothing to a guest you can
log into, with pictures of the console it describes. Twenty minutes, most of it
waiting for a download.

Three machines rather than one? Start there anyway. A cell of one is the same
platform with fewer boxes in it.

---

## Then, when you have a reason

| You want | Read |
|---|---|
| a second and third machine, or several cells | [Setting up a cell](setup-guide.md) |
| to run it day to day: maintenance, recovery, which copy survives which loss | [Operating it](operating.md) |
| the appliance image, the installer ISO, the Debian package | [Installing it](install.md) |
| to drive it from a script | [The REST contract](rest-contract.md) |
| a cell of mixed processor generations | [CPU heterogeneity](cpu-heterogeneity.md) |
| the decision record behind the packaging | [Deployment and devices](deployment-and-devices.md) |

---

## What it looks like

The boards show two things most platforms collapse into one. **Asked** is what
you wanted; **State** is what the node reports. When they differ, that is the
interesting moment — and hiding it behind a single "status" is how a guest sits
stopped for a week while a dashboard says Running.

![The instances board](images/instances.png)

---

## Licence

AGPL-3.0-or-later — the same model Proxmox and VyOS use. The source is at
[github.com/Velstra/cloud](https://github.com/Velstra/cloud).
