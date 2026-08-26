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

    package = lib.mkOption {
      type = lib.types.package;
      description = "The velstra-cloud workspace build.";
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8443";
      description = ''
        API listen address (REST + gRPC + console on one port). The binary
        terminates no TLS; anything beyond loopback belongs behind a TLS
        reverse proxy, and nodes are pointed at that proxy's URL.
      '';
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
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = (cfg.bootstrapAdmin.username == null) == (cfg.bootstrapAdmin.passwordFile == null);
        message = ''
          velstra.cloud.controlPlane.bootstrapAdmin: set username and
          passwordFile together — the api refuses a half-configured bootstrap
          rather than creating an account without a credential.
        '';
      }
    ];

    services.etcd = lib.mkIf cfg.store.bundledEtcd {
      enable = true;
      listenClientUrls = [ "http://127.0.0.1:2379" ];
      advertiseClientUrls = [ "http://127.0.0.1:2379" ];
    };

    systemd.services.velstra-cloud-api = {
      description = "Velstra Cloud API";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after =
        [ "network-online.target" ]
        ++ lib.optional cfg.store.bundledEtcd "etcd.service";
      requires = lib.optional cfg.store.bundledEtcd "etcd.service";
      serviceConfig = {
        Restart = "on-failure";
        RestartSec = 2;
        # Root only for reading tokenFile/passwordFile wherever the operator
        # keeps them; the process itself needs no privilege.
        DynamicUser = false;
      };
      script = ''
        ${lib.optionalString (cfg.bootstrapAdmin.passwordFile != null) ''
          VELSTRA_BOOTSTRAP_PASSWORD="$(cat ${cfg.bootstrapAdmin.passwordFile})"
          export VELSTRA_BOOTSTRAP_PASSWORD
        ''}
        exec ${cfg.package}/bin/velstra-cloud-api \
          --store ${cfg.store.endpoints} \
          --listen ${cfg.listen} \
          --cell ${cfg.cell} \
          --region ${cfg.region} \
          ${lib.optionalString (
            cfg.writesPerSecond != null
          ) "--writes-per-second ${toString cfg.writesPerSecond}"} \
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
        ++ lib.optional cfg.store.bundledEtcd "etcd.service";
      requires = lib.optional cfg.store.bundledEtcd "etcd.service";
      serviceConfig = {
        Restart = "on-failure";
        RestartSec = 2;
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
        );
      };
    };
  };
}
