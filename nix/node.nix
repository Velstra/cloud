# NixOS module: the Velstra Cloud compute-node service set.
#
# This is the half of the node image that is not the appliance machinery: the
# node agent under systemd, the hypervisors it drives, the boot-time seed
# reader, and the LUKS unlock for an encrypted install. It composes with the
# Sentinel flake's `nixosModules.applianceImage` to become the immutable node
# image, but it is deliberately usable without it — the register/guest VM
# checks run it on a plain test VM, and that sameness is what makes them
# evidence about the image.
#
# The contract with the installer (`velstra-cloud-node install`, the ISO
# wizard): the writable partition mounts at `stateDir`, and its root carries
#   node.env     — VELSTRA_NODE / _CELL / _REGION / _API_URL / _VMM / _HOSTNAME
#   node-token   — the one-time registration token (0600)
#   network/     — optional systemd-networkd units overriding the DHCP default
# A node without a seed boots to a getty and an idle agent unit (the
# ConditionPathExists below), which is what a flashed-but-unregistered box
# should be: quiet, not crash-looping.
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  cfg = config.velstra.cloud.node;
in
{
  options.velstra.cloud.node = {
    enable = lib.mkEnableOption "the Velstra Cloud compute-node services";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The velstra-cloud workspace build (nodeagent + node installer binaries).";
    };

    qemu = lib.mkOption {
      type = lib.types.package;
      default = pkgs.qemu_kvm;
      description = "QEMU used when the seed selects `VELSTRA_VMM=qemu`.";
    };

    cloudHypervisor = lib.mkOption {
      type = lib.types.package;
      default = pkgs.cloud-hypervisor;
      description = ''
        Cloud Hypervisor used for `VELSTRA_VMM=cloud-hypervisor`. Also puts
        `ch-remote` on the system PATH — the agent's transient guest units
        resolve it by name.
      '';
    };

    fabricAgent = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = ''
        The Velstra Fabric eBPF/XDP agent (`velstra`), put on PATH when set.
        A seam, on purpose: the binary is available for the fabric datapath,
        but no unit starts it — its configuration comes from the fabric
        controller, and a service wired to a controller nobody has named would
        be a promise nothing keeps. See docs/install.md.
      '';
    };

    stateDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/velstra";
      description = ''
        The node's writable state (the appliance image mounts the data
        partition here). Must agree with `velstra-cloud-node/src/product.rs`.
      '';
    };

    hugepages = lib.mkOption {
      type = lib.types.int;
      default = 0;
      description = ''
        2 MiB hugepages to reserve at boot (`vm.nr_hugepages`). 0 reserves
        none; guests then use ordinary pages. Reserve on hosts whose guests
        are configured for hugepage backing.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # The agent + installer, the hypervisors, and the disk tools the installer
    # and updater resolve by name on PATH (this repo does not pin tool paths
    # the way Sentinel's wrapped CLI does — PATH is supplied here instead).
    environment.systemPackages =
      [
        cfg.package
        cfg.qemu
        cfg.cloudHypervisor
        pkgs.gptfdisk
        pkgs.parted
        pkgs.cryptsetup
        pkgs.e2fsprogs
        pkgs.mdadm
      ]
      ++ lib.optional (cfg.fabricAgent != null) cfg.fabricAgent;

    # KVM now, IOMMU-ready for the passthrough phase: the design doc's device
    # model needs `iommu=pt` and the vendor IOMMU enabled from day one, because
    # a node that must reboot to *see* its devices cannot report them. The
    # cross-vendor pair is harmless on the other vendor's hardware.
    boot.kernelModules = [
      "kvm-intel"
      "kvm-amd"
    ];
    boot.kernelParams = [
      "intel_iommu=on"
      "amd_iommu=on"
      "iommu=pt"
    ];
    boot.kernel.sysctl = lib.mkIf (cfg.hugepages > 0) {
      "vm.nr_hugepages" = cfg.hugepages;
    };

    # networkd everywhere; DHCP on every ethernet uplink unless the installer
    # seeded static units (velstra-node-boot copies those into /run/systemd/
    # network, where their lower filename order wins the match).
    networking.useNetworkd = true;
    networking.useDHCP = false;
    systemd.network.enable = true;
    systemd.network.networks."80-uplink" = {
      matchConfig.Name = "en* eth*";
      networkConfig.DHCP = "yes";
    };

    # The metadata service address. The agent binds 169.254.169.254:80 and
    # treats failure as fatal (a cell whose guests silently get no metadata is
    # worse than a node that says so at startup) — this dummy interface is what
    # makes the bind possible before any guest network exists.
    systemd.network.netdevs."10-vmeta" = {
      netdevConfig = {
        Name = "vmeta0";
        Kind = "dummy";
      };
    };
    systemd.network.networks."10-vmeta" = {
      matchConfig.Name = "vmeta0";
      address = [ "169.254.169.254/32" ];
    };

    # Boot-time seed reader: install the operator's network units where
    # networkd reads them, and apply the seeded hostname. Runs before networkd
    # on purpose — networkd reads /run/systemd/network on its own start, so
    # being earlier is the whole ordering story (the same pattern Sentinel's
    # boot apply uses, and its verified-boot check pins).
    systemd.services.velstra-node-boot = {
      description = "Apply the Velstra node seed (hostname, network) from the data partition";
      wantedBy = [ "multi-user.target" ];
      before = [
        "systemd-networkd.service"
        "velstra-cloud-nodeagent.service"
      ];
      unitConfig.RequiresMountsFor = [ cfg.stateDir ];
      path = [ pkgs.nettools ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        mkdir -p ${cfg.stateDir}
        if [ -d ${cfg.stateDir}/network ]; then
          mkdir -p /run/systemd/network
          for f in ${cfg.stateDir}/network/*.network; do
            if [ -e "$f" ]; then
              cp "$f" /run/systemd/network/
              echo "installed $(basename "$f") from the install-time seed"
            fi
          done
        fi
        if [ -f ${cfg.stateDir}/node.env ]; then
          . ${cfg.stateDir}/node.env
          if [ -n "''${VELSTRA_HOSTNAME:-}" ]; then
            hostname "$VELSTRA_HOSTNAME"
          fi
        fi
      '';
    };

    # The node agent. Everything identifying this node comes from the seed —
    # the image is identical across the fleet, which is what makes an image
    # update one artefact instead of one per node.
    systemd.services.velstra-cloud-nodeagent = {
      description = "Velstra Cloud node agent";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [
        "network-online.target"
        "velstra-node-boot.service"
      ];
      unitConfig = {
        # Unseeded box: the unit stays off instead of crash-looping on missing
        # required flags. `systemctl status` then shows the condition, which
        # names the file to create — that is the error message.
        ConditionPathExists = [
          "${cfg.stateDir}/node.env"
          "${cfg.stateDir}/node-token"
        ];
        RequiresMountsFor = [ cfg.stateDir ];
      };
      # `systemd-run`/`systemctl` (guest units), `ip` (taps), `qemu-img`
      # (disks), `lsblk`/`df` (inventory) — all resolved by name.
      path = [
        cfg.qemu
        cfg.cloudHypervisor
        pkgs.iproute2
        pkgs.util-linux
        pkgs.coreutils
        pkgs.systemd
      ];
      serviceConfig = {
        EnvironmentFile = "${cfg.stateDir}/node.env";
        Restart = "on-failure";
        RestartSec = 5;
      };
      script = ''
        # A transient guest unit does not inherit this unit's PATH, so the
        # hypervisor must be named absolutely (the agent's own --vmm-binary
        # rationale). `fake` is the test hypervisor and has no binary.
        case "''${VELSTRA_VMM:-}" in
          qemu)             vmm_binary=${cfg.qemu}/bin/qemu-system-x86_64 ;;
          cloud-hypervisor) vmm_binary=${cfg.cloudHypervisor}/bin/cloud-hypervisor ;;
          fake)             vmm_binary= ;;
          *)
            echo "node.env sets VELSTRA_VMM='"''${VELSTRA_VMM:-}"' — expected qemu, cloud-hypervisor or fake; edit ${cfg.stateDir}/node.env" >&2
            exit 1
            ;;
        esac
        exec ${cfg.package}/bin/velstra-cloud-nodeagent \
          --node "$VELSTRA_NODE" \
          --cell "$VELSTRA_CELL" \
          --region "$VELSTRA_REGION" \
          --api "$VELSTRA_API_URL" \
          --api-token-file ${cfg.stateDir}/node-token \
          --vmm "$VELSTRA_VMM" \
          ''${vmm_binary:+--vmm-binary "$vmm_binary"} \
          --state-dir ${cfg.stateDir}
      '';
    };

    # Unlock the encrypted data partition before `stateDir` is mounted. The
    # same image serves plaintext and encrypted installs: `velstra-cloud-node
    # unlock` inspects the disk and is a no-op (exit 0) on a plaintext one.
    # The stateDir mount carries `x-systemd.requires=` on this unit via the
    # appliance image module's `unlockUnit` option.
    systemd.services.velstra-node-unlock = {
      description = "Unlock the encrypted Velstra node data volume";
      wantedBy = [ "local-fs.target" ];
      before = [
        "local-fs.target"
        "${utils.escapeSystemdPath cfg.stateDir}.mount"
      ];
      # The block devices (and any assembled RAID array) must exist first.
      after = [
        "local-fs-pre.target"
        "mdmonitor.service"
      ];
      unitConfig.DefaultDependencies = false;
      path = [
        pkgs.util-linux
        pkgs.cryptsetup
        pkgs.systemd
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${cfg.package}/bin/velstra-cloud-node unlock";
      };
    };
  };
}
