# NixOS module: one storage pool, as a service.
#
# `velstra-cloud-poolagent` existed as a binary and was reachable from nothing:
# no module started it, so a cell built from this repository could hold a Pool
# object and a Volume object and there was no process anywhere that would put a
# byte on a disk. Every volume sat unprovisioned, every snapshot untaken, every
# backup unmade — and the only place a pool agent ran was the one-process
# development cell, against a fake.
#
# It is deliberately **not** part of the node module. A pool is not a machine:
# several nodes reach one Ceph pool, one node may export three volume groups,
# and tying storage to whichever hypervisor happened to be asked is how a volume
# becomes unreachable the moment that node is drained. A machine that is both a
# hypervisor and a pool imports both modules and says so.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.velstra.cloud.pool;
in
{
  options.velstra.cloud.pool = {
    enable = lib.mkEnableOption "a Velstra Cloud storage pool agent";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The velstra-cloud workspace build (poolagent binary).";
    };

    id = lib.mkOption {
      type = lib.types.str;
      description = ''
        This pool's id, which has to match the `pools/<id>` object an operator
        registered. It is what every volume is written against, so a mismatch
        is a pool that claims nothing and a cell whose volumes are never
        provisioned — quietly.
      '';
    };

    cell = lib.mkOption {
      type = lib.types.str;
      default = "cell-1";
      description = "The cell this pool belongs to.";
    };

    region = lib.mkOption {
      type = lib.types.str;
      default = "eu-central";
      description = "The region this pool belongs to.";
    };

    backend = lib.mkOption {
      type = lib.types.enum [
        "directory"
        "ceph"
      ];
      default = "directory";
      description = ''
        `directory` keeps volumes as qcow2 files made with `qemu-img`. It needs
        a writable directory and nothing else, and everything it holds lives on
        one machine — which is the thing to know about it: a guest on a volume
        in a directory pool cannot be migrated to a node that cannot see that
        directory.

        `ceph` keeps them as RBD images. Volumes are copy-on-write clones of an
        image that lives once in the cluster, so nothing is copied per node and
        every node reaches every volume.
      '';
    };

    directory = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/velstra/pool";
      description = "Where volumes live for the directory backend. Snapshots live under it.";
    };

    images = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/velstra/images";
      description = ''
        Where an image named by a volume is found. The same directory the node
        agent publishes images into, so a node and a pool on one machine share
        one copy rather than keeping two.
      '';
    };

    ceph = {
      pool = lib.mkOption {
        type = lib.types.str;
        default = "velstra-volumes";
        description = "The RBD pool volumes and their snapshots live in.";
      };
      imagePool = lib.mkOption {
        type = lib.types.str;
        default = "velstra-images";
        description = ''
          The RBD pool images live in. Separate on purpose: an image is written
          once and read for years, a volume is written constantly, and the two
          want different replication, placement and quota. Clones across pools
          cost nothing.
        '';
      };
      user = lib.mkOption {
        type = lib.types.str;
        default = "client.admin";
        description = ''
          The Ceph client to act as. Its keyring has to be where `rbd` looks:
          this agent does not manage credentials, because a process that could
          write its own keyring could grant itself a cluster.
        '';
      };
      conf = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "`ceph.conf`, when it is not in the default place.";
      };
    };

    apiUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Read the cell through the API instead of the store, and be handed only
        this pool's share.

        Without it this agent lists every volume and every snapshot in the cell
        on every pass, so its load grows with the cell rather than with what it
        holds. With it, the API serves every agent from one watch per collection.
        Writes go straight to the store either way — a pool's writes are already
        proportional to its own work.
      '';
    };

    tokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Bearer token for `apiUrl`. A file, so it is not in anybody's process list.";
    };

    store = lib.mkOption {
      type = lib.types.str;
      default = "memory";
      description = ''
        Where state lives: `memory`, or etcd endpoints. `memory` is a pool whose
        objects die with the process, which is a development shape and not a
        deployment one.
      '';
    };

    resyncSeconds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 30;
      description = ''
        How often the pool is re-read and reconciled. Slower than a node's, and
        there is no watch at all: storage work is measured in seconds to
        minutes, so the latency a watch would buy is lost in the noise of a copy.
      '';
    };

    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = "Anything else the backend needs on PATH.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = (cfg.apiUrl == null) == (cfg.tokenFile == null);
        message = "velstra.cloud.pool: apiUrl and tokenFile are set together or not at all — a pool reading through the API has to authenticate as itself.";
      }
    ];

    systemd.services.velstra-cloud-poolagent = {
      description = "Velstra Cloud storage pool ${cfg.id}";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      path =
        [
          # `qemu-img` for the directory backend, `rbd` for Ceph. Both are on
          # PATH regardless of the backend: a pool that was reconfigured and
          # restarted must not fail on a missing binary it had a moment ago.
          pkgs.qemu-utils
        ]
        ++ lib.optional (cfg.backend == "ceph") pkgs.ceph-client
        ++ cfg.extraPackages;
      serviceConfig = {
        Restart = "on-failure";
        RestartSec = 5;
        StateDirectory = "velstra";
        ExecStart = lib.concatStringsSep " " (
          [
            "${cfg.package}/bin/velstra-cloud-poolagent"
            "--pool ${cfg.id}"
            "--cell ${cfg.cell}"
            "--region ${cfg.region}"
            "--store ${cfg.store}"
            "--backend ${cfg.backend}"
            "--resync-secs ${toString cfg.resyncSeconds}"
          ]
          ++ lib.optionals (cfg.backend == "directory") [
            "--dir ${cfg.directory}"
            "--images ${cfg.images}"
          ]
          ++ lib.optionals (cfg.backend == "ceph") (
            [
              "--ceph-pool ${cfg.ceph.pool}"
              "--ceph-image-pool ${cfg.ceph.imagePool}"
              "--ceph-user ${cfg.ceph.user}"
            ]
            ++ lib.optional (cfg.ceph.conf != null) "--ceph-conf ${cfg.ceph.conf}"
          )
          ++ lib.optionals (cfg.apiUrl != null) [
            "--api ${cfg.apiUrl}"
            "--api-token-file ${cfg.tokenFile}"
          ]
        );
      };
    };

    # The directory the backend owns, made before the unit starts rather than by
    # the agent: a process that created its own pool directory could create one
    # on the wrong machine, on a root filesystem, exactly when a mount failed to
    # come up.
    systemd.tmpfiles.rules = lib.mkIf (cfg.backend == "directory") [
      "d ${cfg.directory} 0700 root root -"
      "d ${cfg.images} 0755 root root -"
    ];
  };
}
