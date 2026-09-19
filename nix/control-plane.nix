# NixOS module: the Velstra Cloud control plane under systemd.
#
# The design doc's rule: the control plane has no hardware opinions, so it
# imposes none — a customer running Kubernetes takes the OCI images, a customer
# running one machine takes this module. Same binaries either way.
#
# The store is etcd, bundled by default for the single-cell case. There is no
# `memory` option here on purpose: the api and the controller are two
# processes, and two in-memory stores are two empty universes that cannot see
# each other (the reason `velstra-cloud-dev` exists as ONE process). A module
# that offered `memory` would ship that failure as a configuration.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.velstra.cloud.controlPlane;
in
{
  options.velstra.cloud.controlPlane = {
    enable = lib.mkEnableOption "the Velstra Cloud control plane (api + controllers)";

    fromSeed = lib.mkEnableOption ''
      taking every answer from `/etc/velstra/node.env` instead of from the
      options below, and running only when that seed names the `control-plane`
      role.

      For a machine that is **flashed rather than declared**. Every option in
      this module is read when the image is built, and an appliance is built
      once for a fleet: which of those boxes is the control plane, what it
      listens on and which cell it belongs to are all answered at install time,
      by somebody standing at a console. Until this existed the sealed image
      could only ever be a hypervisor, so a cell built from it needed a
      separately-managed machine for its own control plane.

      etcd ships in the image and is gated by the same role, so a single box
      can be the whole cell without fetching anything.
    '';

    package = lib.mkOption {
      type = lib.types.package;
      description = "The velstra-cloud workspace build.";
    };
    seedFile = lib.mkOption {
      type = lib.types.path;
      default = "/etc/velstra/node.env";
      description = ''
        Which file `fromSeed` reads the answers out of.

        The default is where a machine keeps *who it is*: `/etc` is per-machine
        by construction, and that is the whole reason identity was moved out of
        the state directory. A cell whose machines share one filesystem — which
        is what makes moving a guest possible at all — had every agent reading
        one `node.env` and answering to one name; the second machine to mount it
        renamed the first, and the next upgrade took the control plane down.

        The sealed appliance is the one machine that cannot use that path: its
        `/etc` is a read-only dm-verity store, so it points this at its own
        writable partition instead. That is safe there for the same reason the
        default is safe everywhere else — the partition belongs to one machine.

        Deliberately one file and not a search order. Reading both and letting
        one win is not the fix: the keys the winner does not mention would still
        come from the loser, so a control plane would inherit a hypervisor's
        pool.
      '';
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8443";
      description = ''
        API listen address (REST + gRPC + console on one port). Set tlsCert
        and tlsKey together, or terminate TLS at a trusted reverse proxy.
      '';
    };

    tlsCert = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Runtime path to the API TLS certificate PEM.";
    };
    tlsKey = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Runtime path to the API TLS private key PEM (root-readable only).";
    };

    cell = lib.mkOption {
      type = lib.types.str;
      default = "cell-1";
      description = "Cell identity.";
    };

    region = lib.mkOption {
      type = lib.types.str;
      default = "eu-central";
      description = "Region identity.";
    };

    store = {
      endpoints = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:2379";
        description = "Comma-separated etcd endpoints, shared by api and controllers.";
      };
      bundledEtcd = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Run a single-member etcd on this host (the single-cell default).
          Disable when the cell has its own etcd, and set `endpoints`.
        '';
      };
    };

    tokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Static API tokens, one per line (`token subject`) — operator/automation
        credentials. Keep it out of the store: use a root-readable file, not a
        /nix/store path, for anything real.
      '';
    };

    cellAdmins = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Subjects (from tokenFile or sessions) that operate the cell.";
    };

    bootstrapAdmin = {
      username = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          First console user, created only when the cell has no users at all —
          safe to leave set; an existing cell ignores it.
        '';
      };
      passwordFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "File holding the bootstrap admin's initial password (0600, root).";
      };
    };

    metricsListen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:9310";
      description = "Controller Prometheus endpoint (`off` disables it).";
    };

    cells = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        "cell-2" = "https://cell-2.example:8443";
      };
      description = ''
        Where the *other* cells of this installation are, by name.

        A cell is the failure and scaling domain, so growing means adding
        cells — and that only works if a client can reach one address and have
        the request land in the cell holding the resource. Set this and the API
        forwards; leave it empty and every request is answered here, which is
        what a single-cell installation wants and costs nothing.

        Which cell owns what is read from the projects, not from this map: this
        only says where each cell is. A project this installation has not heard
        of yet is answered locally rather than refused — a router a few seconds
        behind must not turn propagation delay into an error a tenant sees.
      '';
    };

    fabric = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "http://fabric.cell-1.example:50051";
      description = ''
        Where this cell's fabric orchestrator answers.

        Set it and every tenant network, router, port and load balancer is
        mirrored there as it is decided here — which is the only way the data
        plane learns what a VNI *is*. Leave it `null` and the control plane
        still runs, still places guests and still hands out addresses; they
        reach each other on no overlay, because nothing was ever programmed.

        That is a legitimate way to run a cell, and it is also the failure that
        looks most like success: everything reports healthy and no packet
        crosses. Which is why this has no default — a guessed endpoint would be
        a cell that mirrors into the void without saying so.

        The nodes need the same address: see `velstra.cloud.node.fabric`, which
        is what actually starts an agent on the machine.
      '';
    };

    imageSigningKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Ed25519 public keys (the raw 32 bytes, base64) an image's
        `spec.signature` may verify under. Empty — the default — refuses every
        signature at admission, because a claim nobody can check is worse than
        no claim. See docs/operating.md, "Signed images".
      '';
    };

    writesPerSecond = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.unsigned;
      default = null;
      description = ''
        Cap how fast one caller may **write**. `null` — the default — is no cap.

        What it stops is the ordinary accident: a script in a loop, a
        controller written with no backoff, taking the cell's write path from
        everybody else. It is not a security boundary. Reads are never counted,
        and node agents are never limited — an agent reports when something
        changed, and something changing is not something it can defer.
      '';
    };

    resyncSeconds = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      description = ''
        How often the controllers re-list everything and reconcile it again.

        `null` keeps the binary's own default. This is the longest a missed
        watch event can cost, and it is also how quickly the cell notices
        something that changed because of the *clock* rather than because
        somebody wrote it — a maintenance window opening or closing, a
        migration running past its timeout. Shortening it is cheap: a
        reconcile of a settled object writes nothing.
      '';
    };

    alerts = {
      webhook = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Where the controller POSTs an alert: one JSON object per transition,
          `firing` when a condition appears and `resolved` when it goes. The
          rules are fixed — a machine silent past its fencing deadline, a pool
          nearly full, a project out of quota, an object stuck — and nothing is
          posted without this.
        '';
      };

      mailTo = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Addresses to mail the same alerts to, through `alerts.sendmail`.";
      };

      mailFrom = lib.mkOption {
        type = lib.types.str;
        default = "velstra-cloud@localhost";
        description = "The sender an alert mail carries.";
      };

      sendmail = lib.mkOption {
        type = lib.types.str;
        default = "/run/wrappers/bin/sendmail";
        description = ''
          A sendmail-compatible binary, given the message on stdin with `-t`.
          The system MTA's wrapper by default; msmtp's `sendmail` works too.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.fromSeed || (cfg.tlsCert == null) == (cfg.tlsKey == null);
        message = "velstra.cloud.controlPlane: set tlsCert and tlsKey together.";
      }
      {
        assertion =
          cfg.fromSeed
          || (cfg.bootstrapAdmin.username == null) == (cfg.bootstrapAdmin.passwordFile == null);
        message = ''
          velstra.cloud.controlPlane.bootstrapAdmin: set username and
          passwordFile together — the api refuses a half-configured bootstrap
          rather than creating an account without a credential.
        '';
      }
    ];

    services.etcd = lib.mkIf (cfg.fromSeed || cfg.store.bundledEtcd) {
      enable = true;
      listenClientUrls = [ "http://127.0.0.1:2379" ];
      advertiseClientUrls = [ "http://127.0.0.1:2379" ];
    };

    # On a flashed machine the store has to be *in* the image — there is no
    # package manager to fetch it from later — but only the box that turns out
    # to be the control plane should run one. So etcd ships either way and is
    # gated by the same seed everything else here reads: on a hypervisor the
    # unit is skipped, which systemd shows as skipped rather than failed.
    #
    # An etcd running on every appliance would be worse than wasteful. It would
    # be a second empty store on every machine, listening on the loopback
    # address the API looks for — so a control plane that lost its seed would
    # come up against a store that answers and holds nothing, which reads
    # exactly like a cell whose objects have been deleted.
    systemd.services.etcd = lib.mkIf cfg.fromSeed {
      serviceConfig.ExecCondition = "${cfg.package}/bin/velstra-cloud-node has-role control-plane";
    };

    systemd.services.velstra-cloud-api = {
      description = "Velstra Cloud API";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after =
        [ "network-online.target" ]
        ++ lib.optional (cfg.fromSeed || cfg.store.bundledEtcd) "etcd.service";
      # `wants`, not `requires`, under fromSeed: etcd is skipped by its own
      # ExecCondition on a machine that is not the control plane, and a hard
      # requirement on a unit that legitimately did not run is how a hypervisor
      # would refuse to finish booting.
      requires = lib.optional cfg.store.bundledEtcd "etcd.service";
      serviceConfig = {
        Restart = "on-failure";
        RestartSec = 2;
        # Root only for reading tokenFile/passwordFile wherever the operator
        # keeps them; the process itself needs no privilege.
        DynamicUser = false;
      }
      // lib.optionalAttrs cfg.fromSeed {
        EnvironmentFile = "-${cfg.seedFile}";
        ExecCondition = "${cfg.package}/bin/velstra-cloud-node has-role control-plane";
      };
      script =
        if cfg.fromSeed then
          # Bare but for the two things an environment file cannot express: a
          # secret that must not be in anybody's process list, and a default
          # that has to be a default rather than a value the seed carries.
          #
          # Everything else reaches the binary through its own `env =`
          # fallbacks. This is deliberately the same shape the Debian package's
          # unit has — it is the same machine described twice, and the moment
          # the two descriptions differ, one of the two packagings is wrong
          # about a cell in a way nobody sees until an upgrade.
          ''
            # Identity first, state as the fallback — the same order the
            # Debian unit uses. On the sealed appliance only the second exists,
            # because /etc there is a read-only verity store; on every other
            # machine the wizards write the first.
            pw=/var/lib/velstra/bootstrap-password
            [ -f /etc/velstra/bootstrap-password ] && pw=/etc/velstra/bootstrap-password
            if [ -f "$pw" ]; then
              VELSTRA_BOOTSTRAP_PASSWORD="$(cat "$pw")"
              export VELSTRA_BOOTSTRAP_PASSWORD
            fi
            : "''${VELSTRA_STORE_BACKUP_DIR:=/var/lib/velstra/store-backups}"
            export VELSTRA_STORE_BACKUP_DIR
            exec ${cfg.package}/bin/velstra-cloud-api
          ''
        else
          ''
            ${lib.optionalString (cfg.bootstrapAdmin.passwordFile != null) ''
              VELSTRA_BOOTSTRAP_PASSWORD="$(cat ${cfg.bootstrapAdmin.passwordFile})"
              export VELSTRA_BOOTSTRAP_PASSWORD
            ''}
            exec ${cfg.package}/bin/velstra-cloud-api \
          --store ${cfg.store.endpoints} \
          --listen ${cfg.listen} \
          ${lib.optionalString (cfg.tlsCert != null) "--tls-cert ${lib.escapeShellArg (toString cfg.tlsCert)}"} \
          ${lib.optionalString (cfg.tlsKey != null) "--tls-key ${lib.escapeShellArg (toString cfg.tlsKey)}"} \
          --cell ${cfg.cell} \
          --region ${cfg.region} \
          ${lib.optionalString (
            cfg.writesPerSecond != null
          ) "--writes-per-second ${toString cfg.writesPerSecond}"} \
          ${lib.concatStringsSep " " (map (k: "--image-signing-key ${lib.escapeShellArg k}") cfg.imageSigningKeys)} \
          ${lib.concatStringsSep " " (
            lib.mapAttrsToList (cell: endpoint: "--cell-endpoint ${cell}=${endpoint}") cfg.cells
          )} \
          ${lib.optionalString (cfg.tokenFile != null) "--token-file ${cfg.tokenFile}"} \
          ${lib.optionalString (cfg.bootstrapAdmin.username != null) "--bootstrap-admin ${cfg.bootstrapAdmin.username}"} \
          ${lib.concatMapStrings (a: "--cell-admin ${a} ") cfg.cellAdmins}
      '';
    };

    systemd.services.velstra-cloud-controller = {
      description = "Velstra Cloud controllers";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after =
        [ "network-online.target" ]
        ++ lib.optional (cfg.fromSeed || cfg.store.bundledEtcd) "etcd.service";
      # `wants`, not `requires`, under fromSeed: etcd is skipped by its own
      # ExecCondition on a machine that is not the control plane, and a hard
      # requirement on a unit that legitimately did not run is how a hypervisor
      # would refuse to finish booting.
      requires = lib.optional cfg.store.bundledEtcd "etcd.service";
      serviceConfig = {
        Restart = "on-failure";
        RestartSec = 2;
      }
      // lib.optionalAttrs cfg.fromSeed {
        EnvironmentFile = "-${cfg.seedFile}";
        ExecCondition = "${cfg.package}/bin/velstra-cloud-node has-role control-plane";
        # Bare, exactly as the Debian package starts it. The controller's own
        # arguments carry `env =` fallbacks for everything a seed answers, and
        # the rest are defaults no cell has ever had a reason to state.
        ExecStart = "${cfg.package}/bin/velstra-cloud-controller";
      }
      // lib.optionalAttrs (!cfg.fromSeed) {
        ExecStart = lib.concatStringsSep " " (
          [
            "${cfg.package}/bin/velstra-cloud-controller"
            "--store ${cfg.store.endpoints}"
            "--cell ${cfg.cell}"
            "--region ${cfg.region}"
            "--metrics-addr ${cfg.metricsListen}"
          ]
          ++ lib.optional (
            cfg.resyncSeconds != null
          ) "--resync-interval ${toString cfg.resyncSeconds}"
          ++ lib.optional (cfg.fabric != null) "--fabric ${cfg.fabric}"
          ++ lib.optional (cfg.alerts.webhook != null) "--alert-webhook ${lib.escapeShellArg cfg.alerts.webhook}"
          ++ lib.optional (cfg.alerts.mailTo != [ ]) "--alert-mail-to ${lib.escapeShellArg (lib.concatStringsSep "," cfg.alerts.mailTo)}"
          ++ lib.optional (cfg.alerts.mailTo != [ ]) "--alert-mail-from ${lib.escapeShellArg cfg.alerts.mailFrom}"
          ++ lib.optional (cfg.alerts.mailTo != [ ]) "--alert-sendmail ${lib.escapeShellArg cfg.alerts.sendmail}"
        );
      };
    };
  };
}
