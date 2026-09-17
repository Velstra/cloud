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
      default = pkgs.qemu_kvm.override { cephSupport = true; };
      description = ''
        QEMU used when the seed selects `VELSTRA_VMM=qemu`.

        Built with the RBD block driver, which nixpkgs leaves off
        (`cephSupport ? false`, so no `--enable-rbd`). Without it a guest whose
        disk is a Ceph volume cannot start at all: QEMU answers
        `Unknown driver 'rbd'`, and the node reports a guest that will not boot
        with no indication that the cause is which QEMU was built.

        It is on by default rather than opt-in because this is the module the
        sealed appliance uses, and an appliance is flashed long before anybody
        knows whether that cell will have Ceph. The cost is real — an override
        means building QEMU rather than taking the cached one — and a cell that
        is certain it will never use Ceph can set this back to
        `pkgs.qemu_kvm`.

        Cloud Hypervisor has no equivalent: it takes a path and nothing else,
        so `velstra-cloud-nodeagent` refuses an `rbd:` disk there by name
        rather than handing it a path that is not one.
      '';
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
        # `lspci`, for the installed system too: the passthrough binary reads
        # sysfs and needs none of it, but an operator on the console asking
        # "what card is in this box" should not have to.
        pkgs.pciutils
        # `cephadm` and the `ceph` CLI, so an operator can add a Ceph cluster
        # to a cell of flashed machines afterwards. The platform still installs
        # nothing on its own — cephadm pulls the daemon containers only once
        # somebody has asked for a cluster — but a machine with no package
        # manager has nowhere to get cephadm from, and the image has to carry
        # what it cannot fetch. Daemons run as containers, hence podman.
        pkgs.ceph
      ]
      ++ lib.optional (cfg.fabricAgent != null) cfg.fabricAgent;
    virtualisation.podman.enable = true;

    # KVM now, IOMMU-ready for the passthrough phase: the design doc's device
    # model needs `iommu=pt` and the vendor IOMMU enabled from day one, because
    # a node that must reboot to *see* its devices cannot report them. The
    # cross-vendor pair is harmless on the other vendor's hardware.
    boot.kernelModules = [
      "kvm-intel"
      "kvm-amd"
      # What a held-back card is bound to. Present always: which cards a
      # machine reserves is read from its seed at boot, and cannot be a kernel
      # parameter here — this image's command line is sealed into a signed UKI,
      # which would make it a fact about the image instead of the machine.
      "vfio-pci"
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

    # Take the reserved cards away from the host, before anything can use them
    # and before the agent reports what it sees. A seed that reserves nothing
    # makes this a no-op that says so.
    systemd.services.velstra-node-passthrough = {
      description = "Bind the PCI devices this node reserves to vfio-pci";
      wantedBy = [ "multi-user.target" ];
      after = [ "velstra-node-boot.service" ];
      before = [ "velstra-cloud-nodeagent.service" ];
      unitConfig = {
        ConditionPathExists = "${cfg.stateDir}/node.env";
        RequiresMountsFor = [ cfg.stateDir ];
      };
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        EnvironmentFile = "${cfg.stateDir}/node.env";
        ExecStart = "${cfg.package}/bin/velstra-cloud-passthrough";
      };
    };

    # SSH, present but not started. The seed decides: `velstra-node-access`
    # starts it when a key was seeded and stops it when one was not, so the
    # closed default is closed on every boot rather than only the first.
    #
    # Host keys on the writable partition, or every reboot would present a new
    # identity and every client would refuse to connect a second time.
    services.openssh = {
      enable = true;
      startWhenNeeded = false;
      hostKeys = [
        {
          type = "ed25519";
          path = "${cfg.stateDir}/ssh/ssh_host_ed25519_key";
        }
      ];
      settings = {
        # Key only, even when a console password is set: a password that is
        # reachable from the network is a different decision from one that is
        # reachable by somebody standing at the machine, and the installer only
        # asks for the second.
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
        PermitRootLogin = "prohibit-password";
      };
    };
    systemd.services.sshd.wantedBy = lib.mkForce [ ];
    systemd.tmpfiles.rules = [ "d ${cfg.stateDir}/ssh 0700 root root -" ];

    # Who may log in, from the seed, every boot — `/etc` here is a tmpfs, so a
    # password set last week is gone by morning and the seed is the only
    # durable statement of it.
    systemd.services.velstra-node-access = {
      description = "Apply what the seed says about logging in to this machine";
      wantedBy = [ "multi-user.target" ];
      after = [ "velstra-node-boot.service" ];
      before = [ "getty.target" ];
      unitConfig.RequiresMountsFor = [ cfg.stateDir ];
      path = [
        pkgs.shadow
        pkgs.systemd
        pkgs.coreutils
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${cfg.package}/bin/velstra-cloud-node apply-access --dir ${cfg.stateDir}";
      };
    };

    # What the screen says before anybody signs in. Every machine: a hypervisor
    # that shows nothing leaves whoever is standing at it with no way to learn
    # the address they need.
    systemd.services.velstra-node-banner = {
      description = "Write the console banner (name, addresses, roles)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [
        "network-online.target"
        "velstra-node-boot.service"
      ];
      before = [ "getty.target" ];
      unitConfig.RequiresMountsFor = [ cfg.stateDir ];
      path = [ pkgs.iproute2 ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${cfg.package}/bin/velstra-cloud-node banner --dir ${cfg.stateDir}";
      };
    };

    # The first machine of a cell, brought up from its seed alone. What
    # `quickstart` does after writing the seed on a Debian box happens here at
    # first boot, in two halves, because a flashed machine had no API to talk
    # to at install time and no addresses to put in a certificate.
    #
    # Both are gated on the seed naming the control-plane role — on every
    # other machine they show as skipped — and both are no-ops after the first
    # boot: `ensure-tls` keeps a certificate that exists, `bootstrap-cell`
    # creates nothing twice. See docs/joining.md.
    systemd.services.velstra-cell-tls = {
      description = "Make this cell's certificate and tell the seed about it";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      # After the network, so a DHCP lease is in the certificate; before the
      # API, so it has one to serve.
      after = [
        "network-online.target"
        "velstra-node-boot.service"
      ];
      before = [ "velstra-cloud-api.service" ];
      unitConfig = {
        ConditionPathExists = "${cfg.stateDir}/node.env";
        RequiresMountsFor = [ cfg.stateDir ];
      };
      path = [ pkgs.iproute2 ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecCondition = "${cfg.package}/bin/velstra-cloud-node has-role control-plane";
        ExecStart = "${cfg.package}/bin/velstra-cloud-node ensure-tls --dir ${cfg.stateDir}";
      };
    };

    systemd.services.velstra-cell-bootstrap = {
      description = "Create this machine's Node and Pool objects in the cell it is";
      wantedBy = [ "multi-user.target" ];
      wants = [ "velstra-cloud-api.service" ];
      after = [ "velstra-cloud-api.service" ];
      # Not ordered before the agents: they park on the token this writes
      # (`ConditionPathExists`), and this starts them itself once it exists —
      # then, for a machine born with Ceph, waits for the node agent's first
      # inventory to name the OSD disks the way the node names them.
      unitConfig = {
        ConditionPathExists = "${cfg.stateDir}/node.env";
        RequiresMountsFor = [ cfg.stateDir ];
      };
      path = [ pkgs.curl ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecCondition = "${cfg.package}/bin/velstra-cloud-node has-role control-plane";
        ExecStart = "${cfg.package}/bin/velstra-cloud-node bootstrap-cell --dir ${cfg.stateDir}";
      };
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
        # The seed's own answer to "is this box a hypervisor", asked the same
        # way the control-plane and pool units ask about theirs.
        #
        # It mattered the moment the image stopped being hypervisor-only: a
        # machine flashed to be nothing but a control plane has a seed and a
        # token, so both `ConditionPathExists` above are satisfied and this
        # agent would come up, claim the box for guests, and report capacity
        # for a hypervisor nobody asked for.
        #
        # A seed with no `VELSTRA_ROLES` at all still runs it — `roles_of_seed`
        # reads an absent key as `[hypervisor]`, because that is the only thing
        # the installer could make before roles existed, and reading it as
        # "nothing" would turn an upgrade into a fleet that stops running
        # guests.
        ExecCondition = "${cfg.package}/bin/velstra-cloud-node has-role hypervisor";
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
