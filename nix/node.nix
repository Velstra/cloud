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
        The Velstra Fabric eBPF/XDP agent (`velstra`).

        Set it and this machine gets a `velstra-fabric-agent` unit — the thing
        that actually enforces a tenant network on the wire. Leave it null and
        the node still runs: it places guests, gives them addresses and reports
        healthy, and every tenant network stays a record that separates no
        traffic.

        Which controller it watches is not set here. It comes from the seed
        (`VELSTRA_FABRIC_CONTROL`), like everything else this machine was told
        about itself, so one file answers "what is this box doing" on NixOS and
        on Debian alike. The unit stays off until that key is there.
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
        # The overlay, if the seed names one. Without it the agent keeps its
        # default datapath: real taps, no tenant programming — which is why a
        # port carrying security groups is then refused rather than quietly
        # given a wire that enforces none of them.
        fabric_args=
        if [ -n "''${VELSTRA_FABRIC:-}" ]; then
          fabric_args="--datapath fabric --fabric $VELSTRA_FABRIC"
          fabric_args="$fabric_args --fabric-vtep $VELSTRA_FABRIC_VTEP"
          fabric_args="$fabric_args --fabric-underlay $VELSTRA_FABRIC_UNDERLAY"
          if [ -n "''${VELSTRA_FABRIC_SRV6_LOCATOR:-}" ]; then
            fabric_args="$fabric_args --fabric-srv6-locator $VELSTRA_FABRIC_SRV6_LOCATOR"
          fi
        fi
        # Unquoted on purpose: systemd is not involved here, this is the shell
        # splitting a flag list, and the empty case has to disappear entirely.
        # Every value in it went through the wizard's seed-safety check, which
        # refuses anything that would need quoting.
        # Image signing is read from the seed as well: VELSTRA_IMAGE_SIGNING_KEYS
        # (base64 keys, comma-separated) and VELSTRA_REQUIRE_SIGNED_IMAGES=true
        # are the agent's own variables, so they need no flag here.
        exec ${cfg.package}/bin/velstra-cloud-nodeagent \
          --node "$VELSTRA_NODE" \
          --cell "$VELSTRA_CELL" \
          --region "$VELSTRA_REGION" \
          --api "$VELSTRA_API_URL" \
          --api-token-file ${cfg.stateDir}/node-token \
          --vmm "$VELSTRA_VMM" \
          ''${vmm_binary:+--vmm-binary "$vmm_binary"} \
          --state-dir ${cfg.stateDir} \
          $fabric_args
      '';
    };

    # The data plane itself.
    #
    # Everything above decides what *should* be true — which guest is on which
    # network, which rules apply to its port. This is what makes it true on the
    # wire, and until this session it existed nowhere: the agent shipped on the
    # node image, sat on PATH, and nothing started it. A cell installed from
    # this module got tenant networks that were records and nothing else.
    #
    # It watches fabric's agent-facing service (`--controller`), which is NOT
    # the orchestrator the node agent above talks to — different port, different
    # audience, different amount of trust. Fabric binds the orchestrator to
    # localhost by default and offers mTLS on this one.
    systemd.services.velstra-fabric-agent = lib.mkIf (cfg.fabricAgent != null) {
      description = "Velstra Fabric data plane (eBPF/XDP)";
      wantedBy = [ "multi-user.target" ];
      # Before the node agent, not after: the agent creates taps and asks the
      # orchestrator to make them tenant ports, and a port programmed against a
      # data plane that is not loaded yet is a guest with a wire and no rules
      # for however long the gap lasts.
      before = [ "velstra-cloud-nodeagent.service" ];
      after = [
        "network-pre.target"
        "velstra-node-boot.service"
      ];
      unitConfig = {
        # Same rule as the node agent: no seed, no unit — and the condition
        # names the key, so `systemctl status` is the error message. A machine
        # whose cell has no fabric never starts this and never fails it.
        ConditionPathExists = "${cfg.stateDir}/node.env";
        RequiresMountsFor = [ cfg.stateDir ];
      };
      path = [ pkgs.iproute2 ];
      serviceConfig = {
        EnvironmentFile = "${cfg.stateDir}/node.env";
        Restart = "on-failure";
        RestartSec = 2;
        RuntimeDirectory = "velstra";
        RuntimeDirectoryMode = "0700";
        # Whether this cell has a fabric is a runtime answer in the seed, but
        # whether the agent is on the machine is a build-time one — so on the
        # standard node image every node carries this unit and most of them may
        # have nothing to join.
        #
        # ExecCondition rather than a script that exits 0: systemd records a
        # failed condition as "skipped", not as "ran and finished", which is the
        # difference between `systemctl status` saying this box is not part of a
        # fabric and it saying the data plane started and stopped. The condition
        # still writes to the journal, so the reason is there to read.
        ExecCondition = pkgs.writeShellScript "velstra-fabric-wanted" ''
          if grep -qE '^VELSTRA_FABRIC_CONTROL=.' ${cfg.stateDir}/node.env; then
            exit 0
          fi
          echo "no VELSTRA_FABRIC_CONTROL in ${cfg.stateDir}/node.env: this cell has no data"
          echo "plane, so tenant networks here are records that separate no traffic."
          echo "'velstra-cloud-node setup' asks for a fabric; answering it changes this."
          exit 1
        '';
        # Loading and attaching XDP/eBPF. CAP_SYS_ADMIN is broad; narrowing it
        # to CAP_BPF+CAP_PERFMON depends on the target kernel, so it stays until
        # a check proves the narrower set loads here.
        AmbientCapabilities = [
          "CAP_BPF"
          "CAP_NET_ADMIN"
          "CAP_SYS_ADMIN"
        ];
        CapabilityBoundingSet = [
          "CAP_BPF"
          "CAP_NET_ADMIN"
          "CAP_SYS_ADMIN"
        ];
        NoNewPrivileges = true;
        ProtectHome = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
      };
      # --node-id must be the cell's node id rather than the hostname the agent
      # would default to: the node agent registers this host with the
      # orchestrator under that id, and a config fetched under a second name
      # would be a config for a host nobody registered.
      script = ''
        exec ${cfg.fabricAgent}/bin/velstra run \
          --controller "$VELSTRA_FABRIC_CONTROL" \
          --node-id "$VELSTRA_NODE"
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
