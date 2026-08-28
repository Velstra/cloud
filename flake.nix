{
  description = "Velstra Cloud — control plane packaging, the immutable compute-node image, and a one-command development cell";

  # Verify with Nix:
  #   nix run   .#dev                      # a whole cell in one process, seeded
  #   nix build .#node-image               # the flashable compute-node appliance
  #   nix build .#node-iso                 # the installer ISO (first-boot wizard)
  #   nix build .#api-image                # OCI image, velstra-cloud-api
  #   nix build .#deb                      # the Debian package (roles chosen at setup)
  #   nix build .#checks.x86_64-linux.register -L      # a node registers over a token
  #   nix build .#checks.x86_64-linux.maintenance -L   # a window closes a node, and expiry reopens it

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    # Rust only: the workspace's tonic 0.14 needs rustc >= 1.88, which 25.05
    # (1.86) does not carry. The OS side of every image stays on 25.05 — this
    # input supplies nothing but the toolchain that builds the binaries.
    nixpkgs-rust.url = "github:NixOS/nixpkgs/nixos-unstable";
    # The Sentinel appliance factory: A/B slots, dm-verity store, Secure Boot,
    # installer ISO — the node image is a different package set in the SAME
    # machinery (docs/deployment-and-devices.md §2A), so the machinery is an
    # input, not a copy.
    #
    # The public URL rather than the sibling checkout, and the difference is not
    # cosmetic: a `git+file:` pointing into one person's home directory is a
    # flake nobody else can evaluate — not another developer, and not CI, which
    # is why none of the checks below had ever run on a runner.
    #
    # Working on both repos at once still works, and does not need this line
    # edited: pass the checkout for the length of one command.
    #
    #     nix build .#checks.x86_64-linux.guest \
    #       --override-input sentinel path:../sentinel
    #
    # That is better than editing it, because an edit is a thing to remember to
    # undo and an override is not.
    sentinel = {
      url = "github:Velstra/sentinel";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-rust,
      sentinel,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      rustPlatform = nixpkgs-rust.legacyPackages.${system}.rustPlatform;
      lib = nixpkgs.lib;

      # From the api crate's Cargo.toml, so the image tag and `--version`
      # cannot disagree.
      version =
        (builtins.fromTOML (builtins.readFile ./velstra-cloud-api/Cargo.toml)).package.version;

      # --- the workspace binaries -------------------------------------------
      # One build for all six: api, controller, nodeagent, poolagent, the
      # node installer, and the dev cell. protoc for the tonic build scripts.
      velstra-cloud = rustPlatform.buildRustPackage {
        pname = "velstra-cloud";
        inherit version;
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.protobuf ];
        PROTOC = "${pkgs.protobuf}/bin/protoc";
        # The workspace tests need etcd, QEMU, /dev/kvm and a user session bus
        # — they are the repository's CI gate (`cargo test --workspace`), not a
        # sandbox's. This derivation exists for the binaries.
        doCheck = false;
      };

      # The fabric eBPF/XDP agent that runs on a compute node, built by the
      # Sentinel flake (which pins the fabric revision and the nightly
      # toolchain its eBPF needs). Reusing that build is the point: one pinned
      # data plane across both products.
      fabricAgent = sentinel.packages.${system}.velstra;

      # --- the compute-node appliance ---------------------------------------
      # Identity + sizing for the shared appliance factory. Everything here is
      # the `velstra.appliance.*` counterpart of what the installer's
      # `product.rs` hardcodes — the two must agree.
      nodeIdentity = {
        velstra.appliance = {
          productName = "Velstra Cloud Node";
          osId = "velstra-cloud-node";
          slotPrefix = "velstra-node";
          defaultHostname = "velstra-node";
          stateDir = "/var/lib/velstra";
          unlockUnit = "velstra-node-unlock.service";
          stateDirServices = [
            "velstra-node-boot"
            "velstra-cloud-nodeagent"
          ];
          slotTypesEnvFile = "velstra-node/slot-types.env";
          # The node closure carries two hypervisors; roomier slots than
          # Sentinel's. Both slots reserve this, so the disk floor is
          # ~2×(store+verity)+data.
          storeSize = "4096M";
          veritySize = "320M";
          secureBootCommonName = "Velstra Cloud Node Secure Boot";
        };
        velstra.cloud.node = {
          enable = true;
          package = velstra-cloud;
          fabricAgent = fabricAgent;
        };
        system.stateVersion = "25.05";
      };

      nodeImageRaw =
        let
          c = self.nixosConfigurations.node-image.config;
        in
        "${c.system.build.finalImageSigned}/${c.image.filePath}";

      # --- OCI images for the control plane ---------------------------------
      ociImage =
        {
          name,
          binary,
          env ? [ ],
          ports ? { },
        }:
        pkgs.dockerTools.buildLayeredImage {
          inherit name;
          tag = version;
          # The full workspace package: both control-plane binaries are in one
          # closure anyway, and a debugging `docker exec` that finds the other
          # binary present is worth more than a few shaved megabytes.
          contents = [
            velstra-cloud
            pkgs.cacert
          ];
          config = {
            Entrypoint = [ "${velstra-cloud}/bin/${binary}" ];
            Env = env;
            ExposedPorts = ports;
          };
        };

      # --- the development cell ---------------------------------------------
      # `nix run .#dev` — THE onboarding path. The binary itself is the honest
      # one (one process, one store, seeded); the wrapper's job is to make the
      # two ways it can fail loud before they look like a hang.
      dev = pkgs.writeShellApplication {
        name = "dev";
        text = ''
          listen="''${1:-127.0.0.1:8080}"
          host="''${listen%:*}"
          port="''${listen##*:}"
          # A used port surfaces as a bind error eventually — but say it up
          # front, with the fix in the sentence.
          if { exec 3<>"/dev/tcp/$host/$port"; } 2>/dev/null; then
            exec 3>&-
            echo "error: $listen is already in use — pass a free address, e.g.:  nix run .#dev -- 127.0.0.1:9090" >&2
            exit 1
          fi
          echo "starting the Velstra Cloud development cell on $listen"
          echo "(no /dev/kvm needed — the dev cell's hypervisor is fake and no guest is real)"
          echo
          echo "Open the console URL below and sign in with the printed token."
          echo
          exec ${velstra-cloud}/bin/velstra-cloud-dev "$listen"
        '';
      };

      # Chromium in a sandbox has no fonts, and it aborts rather than degrade
      # when fontconfig can supply nothing at all (Sentinel's console check
      # found this the hard way). One family pins the existence of a fallback.
      sandboxFonts = pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; };

      # The ISO's product config, shared by `nixosConfigurations.node-iso` and
      # the wizard check so the check drives the medium that actually ships.
      nodeIsoConfig = {
        velstra.iso = {
          productName = "Velstra Cloud Node";
          brandId = "velstra-node";
          installerPackage = velstra-cloud;
          installCommand = "velstra-cloud-node install";
          imageSource = nodeImageRaw;
          sourceEnvVar = "VELSTRA_NODE_INSTALL_SOURCE";
          label = version;
          isoBaseName = "velstra-cloud-node-installer";
          volumeId = "VELSTRA_NODE";
          hostname = "velstra-node-installer";
          tagline = "Installs the immutable compute-node appliance onto internal storage";
        };
        # The installer resolves its disk tools by name on PATH (unlike
        # Sentinel's wrapped CLI) — the live system must carry them.
        environment.systemPackages = [
          pkgs.gptfdisk
          pkgs.parted
          pkgs.mdadm
          pkgs.cryptsetup
          pkgs.e2fsprogs
        ];
      };

      # A VM node running the full control plane with a static operator token —
      # the shared base of the register and guest checks.
      controlPlaneNode = {
        imports = [ self.nixosModules.controlPlane ];
        velstra.cloud.controlPlane = {
          enable = true;
          package = velstra-cloud;
          listen = "0.0.0.0:8443";
          tokenFile = pkgs.writeText "dev-tokens" ''
            opstoken ops
          '';
          cellAdmins = [ "ops" ];
        };
        networking.firewall.allowedTCPPorts = [ 8443 ];
        # The test scripts talk to the API with curl; a minimal test VM does
        # not carry it.
        environment.systemPackages = [ pkgs.curl ];
      };
    in
    {
      packages.${system} = {
        default = velstra-cloud;
        inherit velstra-cloud dev;

        # The flashable verified-boot compute-node image (A/B slots, dm-verity
        # store, Secure Boot demo keys — the Sentinel factory).
        #   nix build .#node-image
        node-image = self.nixosConfigurations.node-image.config.system.build.finalImageSigned;

        # The live installer ISO: boots into the first-boot wizard
        # (`velstra-cloud-node install`), which clones the bundled image and
        # seeds the control-plane URL + registration token.
        #   nix build .#node-iso
        node-iso = self.nixosConfigurations.node-iso.config.system.build.isoImage;

        # Control-plane OCI images (`docker load < result`).
        api-image = ociImage {
          name = "velstra-cloud-api";
          binary = "velstra-cloud-api";
          env = [ "VELSTRA_LISTEN=0.0.0.0:8443" ];
          ports."8443/tcp" = { };
        };
        # The Debian package: the same binaries, the same wizard, the same
        # seed — for a fleet that already runs Debian and is not going to be
        # told to install NixOS first.
        #   nix build .#deb
        deb = import ./nix/debian.nix {
          inherit pkgs lib velstra-cloud version;
        };

        controller-image = ociImage {
          name = "velstra-cloud-controller";
          binary = "velstra-cloud-controller";
          env = [ "VELSTRA_METRICS=0.0.0.0:9310" ];
          ports."9310/tcp" = { };
        };
      };

      apps.${system}.dev = {
        type = "app";
        program = "${dev}/bin/dev";
      };

      nixosModules.node = ./nix/node.nix;
      nixosModules.controlPlane = ./nix/control-plane.nix;
      # Storage, as its own module and not part of the node: a pool is not a
      # machine. A box that is both a hypervisor and a pool imports both and
      # says so.
      nixosModules.pool = ./nix/pool.nix;

      nixosConfigurations.node-image = lib.nixosSystem {
        inherit system;
        modules = [
          sentinel.nixosModules.applianceImage
          self.nixosModules.node
          nodeIdentity
          { nixpkgs.hostPlatform = system; }
        ];
      };

      nixosConfigurations.node-iso = lib.nixosSystem {
        inherit system;
        modules = [
          sentinel.nixosModules.applianceIso
          nodeIsoConfig
          { nixpkgs.hostPlatform = system; }
        ];
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [
          pkgs.cargo
          pkgs.rustc
          pkgs.clippy
          pkgs.rustfmt
          pkgs.protobuf
          pkgs.etcd
          pkgs.nodejs
        ];
        PROTOC = "${pkgs.protobuf}/bin/protoc";
      };

      checks.${system} = {
        # The development cell actually develops: it comes up, is seeded, and
        # answers as itself — API, console, and the fake node. This is the
        # `nix run .#dev` promise, checked.
        #   nix build .#checks.x86_64-linux.dev-smoke -L
        dev-smoke =
          pkgs.runCommand "velstra-cloud-dev-smoke"
            {
              nativeBuildInputs = [
                velstra-cloud
                pkgs.curl
              ];
            }
            ''
              set -eu
              velstra-cloud-dev 127.0.0.1:18080 > dev.log 2>&1 &
              pid=$!
              auth="Authorization: Bearer devtoken"
              up=0
              for _ in $(seq 150); do
                if curl -fsS -H "$auth" http://127.0.0.1:18080/api/v1/nodes >/dev/null 2>&1; then
                  up=1; break
                fi
                sleep 0.2
              done
              if [ "$up" != 1 ]; then
                echo "the dev cell never answered:"; cat dev.log; exit 1
              fi
              # Downloads land in files first: `curl | grep -q` dies of SIGPIPE
              # when grep quits at the first match, and set -e calls that a
              # failed check.
              curl -fsS -H "$auth" http://127.0.0.1:18080/api/v1/nodes > nodes.json
              grep -q node-a nodes.json
              curl -fsS -H "$auth" http://127.0.0.1:18080/api/v1/projects/p1/instances > instances.json
              grep -q web-1 instances.json
              # The console ships on the same port, as itself.
              curl -fsS http://127.0.0.1:18080/ > console.html
              grep -qi velstra console.html
              # …and the startup banner tells the operator how to get in.
              grep -q "token" dev.log
              kill $pid
              cp dev.log $out
            '';

        # The console suite in a real browser, against the in-repo contract
        # server — Sentinel's console-check pattern, minus its cargo step (the
        # page generator is already built).
        #   nix build .#checks.x86_64-linux.console -L
        console =
          pkgs.runCommand "velstra-cloud-console"
            {
              nativeBuildInputs = [
                velstra-cloud
                pkgs.nodejs
                pkgs.chromium
              ];
              HOME = "/build";
              FONTCONFIG_FILE = sandboxFonts;
            }
            ''
              set -eu
              cp -r ${./velstra-cloud-console/tests/console} tests-console
              chmod -R u+w tests-console
              velstra-console-page > console.html
              # The whole console is one script in one scope; a stray brace is
              # a page that parses to nothing. Same early tripwire as run.sh.
              sed -n '/^<script>$/,/^<\/script>$/p' console.html | sed '1d;$d' > console.js
              node --check console.js
              export CHROMIUM=${pkgs.chromium}/bin/chromium
              export CONSOLE_TOKEN=testtoken
              CONSOLE_PAGE=$PWD/console.html FAKE_PORT=0 node tests-console/fake-api.mjs > fake.log 2>&1 &
              port=""
              for _ in $(seq 100); do
                port=$(sed -n 's/^listening //p' fake.log | head -1)
                [ -n "$port" ] && break
                sleep 0.2
              done
              if [ -z "$port" ]; then
                echo "the contract server never came up:"; cat fake.log; exit 1
              fi
              export CONSOLE_URL="http://127.0.0.1:$port/"
              node tests-console/console.test.mjs | tee output
              grep -Eq "^[1-9][0-9]*/[1-9][0-9]* passed" output
              cp output $out
            '';

        # The node image boots as the sealed appliance it claims to be:
        # verity-backed store, volatile root, the agent parked (not
        # crash-looping) until a seed exists, the metadata address bindable,
        # and the IOMMU-ready cmdline present. Boots via a qcow2 overlay of
        # the actual built image — Sentinel's verified-boot pattern.
        #   nix build .#checks.x86_64-linux.node-image-boots -L
        node-image-boots = pkgs.testers.runNixOSTest {
          name = "velstra-node-image-boots";
          nodes.machine = {
            imports = [
              sentinel.nixosModules.applianceImage
              self.nixosModules.node
              nodeIdentity
            ];
            virtualisation = {
              directBoot.enable = false;
              mountHostNixStore = false;
              useEFIBoot = true;
              memorySize = 2048;
              fileSystems = lib.mkVMOverride { };
            };
            # `dmsetup` for the test's own verity assertion.
            environment.systemPackages = [ pkgs.lvm2 ];
          };
          testScript =
            { nodes, ... }:
            ''
              import os
              import subprocess
              import tempfile

              tmp = tempfile.NamedTemporaryFile()
              subprocess.run([
                "${nodes.machine.virtualisation.qemu.package}/bin/qemu-img",
                "create", "-f", "qcow2",
                "-b", "${nodes.machine.system.build.finalImage}/${nodes.machine.image.filePath}",
                "-F", "raw", tmp.name,
              ], check=True)
              os.environ["NIX_DISK_IMAGE"] = tmp.name

              machine.wait_for_unit("multi-user.target")

              with subtest("verified boot: volatile root + dm-verity store"):
                  machine.succeed("findmnt --kernel --type tmpfs /")
                  verity = machine.succeed("dmsetup info --target verity usr")
                  assert "ACTIVE" in verity, verity
                  backing = machine.succeed("df --output=source /nix/store | tail -n1").strip()
                  assert backing == "/dev/mapper/usr", backing

              with subtest("an unseeded node is parked, not crash-looping"):
                  # No node.env on a fresh image: the agent's condition holds it
                  # off, and `systemctl status` names the file to create.
                  machine.succeed("test ! -f /var/lib/velstra/node.env")
                  out = machine.succeed(
                      "systemctl show velstra-cloud-nodeagent"
                      " -p ActiveState -p ConditionResult"
                  )
                  assert "ActiveState=inactive" in out, out
                  assert "ConditionResult=no" in out, out

              with subtest("the unlock unit is a no-op on a plaintext install"):
                  machine.wait_for_unit("velstra-node-unlock.service")

              with subtest("the compute-node kernel and metadata plumbing are in place"):
                  cmdline = machine.succeed("cat /proc/cmdline")
                  for param in ["intel_iommu=on", "amd_iommu=on", "iommu=pt"]:
                      assert param in cmdline, cmdline
                  # The dummy interface that makes the agent's fatal
                  # 169.254.169.254:80 bind possible.
                  machine.wait_until_succeeds("ip addr show vmeta0 | grep -q 169.254.169.254")

              with subtest("both hypervisors and the fabric agent ship on the image"):
                  machine.succeed("command -v qemu-system-x86_64")
                  machine.succeed("command -v cloud-hypervisor")
                  machine.succeed("command -v ch-remote")
                  machine.succeed("command -v velstra")
                  machine.succeed("command -v velstra-cloud-node")

              with subtest("state persists on a real data partition, not the volatile root"):
                  src = machine.succeed("findmnt -no SOURCE /var/lib/velstra").strip()
                  assert src.startswith("/dev/"), src
                  machine.succeed("echo persisted > /var/lib/velstra/marker")
                  machine.succeed("grep -qx persisted /var/lib/velstra/marker")
            '';
        };

        # The management story end to end: an operator creates the node at the
        # API and gets the one-time token; the node is seeded exactly the way
        # the ISO wizard seeds it; the agent comes up and the node starts
        # reporting. Two machines, a real network between them.
        #   nix build .#checks.x86_64-linux.register -L
        register = pkgs.testers.runNixOSTest {
          name = "velstra-node-register";
          nodes.cell = controlPlaneNode;
          nodes.node = {
            imports = [ self.nixosModules.node ];
            velstra.cloud.node = {
              enable = true;
              package = velstra-cloud;
            };
          };
          testScript = ''
            import json

            cell.wait_for_unit("velstra-cloud-api.service")
            cell.wait_for_unit("velstra-cloud-controller.service")
            auth = "-H 'Authorization: Bearer opstoken'"
            api = "http://127.0.0.1:8443/api/v1"
            cell.wait_until_succeeds(f"curl -fsS {auth} {api}/nodes")

            with subtest("an operator registers the node and is shown the token once"):
                created = json.loads(cell.succeed(
                    f"curl -fsS -X POST {auth} -H 'Content-Type: application/json'"
                    f" -d '{{\"id\": \"node-1\", \"spec\": {{\"schedulable\": true}}}}'"
                    f" {api}/nodes"
                ))
                assert created["target"] == "nodes/node-1", created
                token = created["nodeToken"]
                assert len(token) == 64, created

            with subtest("the wizard's seed brings the agent up"):
                node.wait_for_unit("multi-user.target")
                # Exactly the files `velstra-cloud-node install` writes.
                node.succeed("mkdir -p /var/lib/velstra")
                node.succeed(
                    "printf 'VELSTRA_NODE=node-1\nVELSTRA_CELL=cell-1\n"
                    "VELSTRA_REGION=eu-central\nVELSTRA_API_URL=http://cell:8443\n"
                    "VELSTRA_VMM=fake\nVELSTRA_HOSTNAME=node-t1\n'"
                    " > /var/lib/velstra/node.env"
                )
                node.succeed(f"echo {token} > /var/lib/velstra/node-token")
                node.succeed("chmod 600 /var/lib/velstra/node-token")
                node.succeed("systemctl start velstra-cloud-nodeagent")
                node.wait_for_unit("velstra-cloud-nodeagent.service")

            with subtest("the node reports itself to the cell"):
                # The agent's first status report carries its capacity — that
                # arriving over the node token IS the registration working.
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/nodes/node-1 | grep -q vcpus",
                    timeout=120,
                )
          '';
        };

        # One box that is the whole cell.
        #
        # The smallest real installation: control plane, hypervisor and storage
        # pool on one machine, with the bundled etcd, which is what somebody
        # running this at home has. It is also the shape most likely to rot
        # unnoticed — every other check runs two of the three roles together
        # (`guest` takes control plane + hypervisor, `storage` control plane +
        # pool) and nothing ran all three, so anything that only breaks when
        # they share a machine had nowhere to be caught.
        #
        # What it proves is the whole chain on one host: the pool provisions a
        # volume, the scheduler places a guest on the only node there is, and
        # the agent runs it. Then the two things a single node makes awkward on
        # purpose — a spread policy with nothing to spread across, and drain,
        # which on one machine means "nothing may run here".
        #   nix build .#checks.x86_64-linux.single-node -L
        single-node = pkgs.testers.runNixOSTest {
          name = "velstra-single-node";
          nodes.home = {
            imports = [
              controlPlaneNode
              self.nixosModules.node
              self.nixosModules.pool
            ];
            velstra.cloud.node = {
              enable = true;
              package = velstra-cloud;
            };
            velstra.cloud.pool = {
              enable = true;
              package = velstra-cloud;
              id = "local";
              # The cell's own etcd — the one the control plane on this very
              # machine brought up. `memory` would give the pool agent a store
              # of its own, and it would report a pool nobody can see.
              store = "127.0.0.1:2379";
              resyncSeconds = 2;
            };
            velstra.cloud.controlPlane.resyncSeconds = 5;
            # Deliberately modest. A machine somebody has at home is not a
            # rack, and a cell that needs a rack to hold its own control plane
            # would not be the thing this claims to be.
            virtualisation = {
              memorySize = 2048;
              diskSize = 4096;
            };
          };
          testScript = ''
            import json

            auth = "-H 'Authorization: Bearer opstoken'"
            ct = "-H 'Content-Type: application/json'"
            api = "http://127.0.0.1:8443/api/v1"

            with subtest("all three roles come up on one machine"):
                for unit in [
                    "etcd.service",
                    "velstra-cloud-api.service",
                    "velstra-cloud-controller.service",
                    "velstra-cloud-poolagent.service",
                ]:
                    home.wait_for_unit(unit)
                home.wait_until_succeeds(f"curl -fsS {auth} {api}/pools")

            with subtest("the box registers as its own node"):
                created = json.loads(home.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"home-1\", \"spec\": {{\"schedulable\": true}}}}'"
                    f" {api}/nodes"
                ))
                token = created["nodeToken"]
                home.succeed("mkdir -p /var/lib/velstra")
                home.succeed(f"echo {token} > /var/lib/velstra/node-token")
                home.succeed("chmod 600 /var/lib/velstra/node-token")
                # The seed a home installation actually writes: every role on
                # one line, which is what `velstra-cloud-node setup` produces
                # when somebody answers "1 2 3".
                home.succeed(
                    "printf 'VELSTRA_ROLES=control-plane,hypervisor,pool\n"
                    "VELSTRA_NODE=home-1\nVELSTRA_CELL=cell-1\n"
                    "VELSTRA_REGION=eu-central\nVELSTRA_API_URL=http://127.0.0.1:8443\n"
                    "VELSTRA_VMM=fake\nVELSTRA_POOL=local\n"
                    "VELSTRA_POOL_BACKEND=directory\nVELSTRA_HOSTNAME=home\n'"
                    " > /var/lib/velstra/node.env"
                )
                home.succeed("systemctl start velstra-cloud-nodeagent")
                home.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/nodes/home-1 | grep -q vcpus",
                    timeout=120,
                )

            with subtest("the local pool provisions a volume"):
                home.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"local\", \"spec\": {{\"accepting\": true}}}}'"
                    f" {api}/pools"
                )
                home.succeed(
                    f"curl -fsS -X POST {auth} {ct} -d '{{\"id\": \"home\","
                    f" \"spec\": {{\"quota\": {{}}}}}}' {api}/projects"
                )
                home.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"disk\", \"spec\": {{\"sizeGib\": 1,"
                    f" \"pool\": \"local\"}}}}'"
                    f" {api}/projects/home/volumes"
                )
                home.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/home/volumes/disk"
                    " | grep -q '\"provisioned\":true'",
                    timeout=180,
                )
                # The bytes, not the object.
                home.succeed("ls /var/lib/velstra/pool | grep -q qcow2")

            with subtest("a guest is placed on the only node there is"):
                slug = "sha256-" + "ab" * 32
                home.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"{slug}\", \"spec\": {{"
                    f"\"digest\": \"sha256:{'ab' * 32}\", \"format\": \"Raw\","
                    f" \"sizeBytes\": 8388608,"
                    f" \"sourceUrl\": \"file:///dev/null\"}}}}'"
                    f" {api}/projects/home/images"
                )
                home.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"g1\", \"spec\": {{\"vcpus\": 1,"
                    f" \"memoryMib\": 256, \"rootDiskGib\": 1,"
                    f" \"image\": \"projects/home/images/{slug}\"}}}}'"
                    f" {api}/projects/home/instances"
                )
                home.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/home/instances/g1"
                    " | grep -q '\"node\":\"home-1\"'",
                    timeout=180,
                )

            def guest(name, policy=""):
                extra = f" \"placementPolicy\": {{{policy}}}," if policy else ""
                return (
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"{name}\", \"spec\": {{\"vcpus\": 1,"
                    f" \"memoryMib\": 256, \"rootDiskGib\": 1,{extra}"
                    f" \"image\": \"projects/home/images/{slug}\"}}}}'"
                    f" {api}/projects/home/instances"
                )

            with subtest("keeping a pair apart is a wish a one-node cell can still grant"):
                # This is the difference a home installation actually feels.
                # `Preferred` means "put it elsewhere if anywhere else will take
                # it, rather than not running at all" — and on one machine there
                # is nowhere else, so it runs beside its sibling. A cell of one
                # must not be a cell where half the placement vocabulary means
                # "never starts".
                home.succeed(guest("g2", "\"antiAffinityGroup\": \"web\","
                                         " \"spread\": \"Preferred\""))
                home.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/home/instances/g2"
                    " | grep -q '\"node\":\"home-1\"'",
                    timeout=180,
                )

            with subtest("and demanding it is refused in words, not left pending"):
                # `Required` genuinely cannot be met here, and the important
                # thing is that somebody is told which rule stopped it — not a
                # guest that sits unplaced with nothing said.
                home.succeed(guest("g3", "\"antiAffinityGroup\": \"web\","
                                         " \"spread\": \"Required\""))
                said = home.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/home/instances/g3:explainPlacement"
                    " | grep -o '\"[^\"]*\"' | head -40",
                    timeout=180,
                )
                # It names the node it could not use, so the answer is about
                # this cell rather than a generic "no valid host".
                assert "home-1" in said, said

            with subtest("the one node can still be taken out of service"):
                # Somebody has to be able to take their own machine down, and
                # the platform has to say what that costs rather than refuse or
                # pretend the guests moved somewhere.
                said = home.succeed(
                    f"curl -fsS {auth} {api}/nodes/home-1:explainMaintenance"
                )
                assert "home-1" in said, said
          '';
        };

        # A node that comes up has a data plane — or says why it does not.
        #
        # This is the check for the gap that existed until now: the fabric agent
        # shipped on the node image, sat on PATH, and no unit anywhere started
        # it. Every node installed from this module got tenant networks that
        # were records in a database and separated no traffic, and nothing
        # anywhere said so. Everything reported healthy.
        #
        # A stub agent rather than the real one. What is under test is the
        # wiring — does a unit exist, is it gated on the right two things, does
        # it get the controller and the node id, does it load before the node
        # agent starts asking for ports. Whether eBPF verifies on this kernel is
        # fabric's own question and it has its own VM tests for it; pulling that
        # in here would make this check slow, and make it fail for reasons that
        # are not about the wiring.
        #   nix build .#checks.x86_64-linux.fabric-agent -L
        fabric-agent = pkgs.testers.runNixOSTest {
          name = "velstra-fabric-agent";
          nodes.node =
            { pkgs, ... }:
            {
              imports = [ self.nixosModules.node ];
              velstra.cloud.node = {
                enable = true;
                package = velstra-cloud;
                # Records how it was called and then stays up, the way the real
                # agent does — a stub that exited would make "the unit is
                # active" untestable.
                fabricAgent = pkgs.writeShellScriptBin "velstra" ''
                  echo "$@" > /run/fabric-agent-argv
                  exec sleep infinity
                '';
              };
            };
          testScript = ''
            node.wait_for_unit("multi-user.target")
            node.succeed("mkdir -p /var/lib/velstra")

            seed = (
                "VELSTRA_NODE=node-1\nVELSTRA_CELL=cell-1\nVELSTRA_REGION=eu-central\n"
                "VELSTRA_API_URL=http://cell:8443\nVELSTRA_VMM=fake\n"
            )

            with subtest("a cell with no fabric skips it, and the journal says why"):
                node.succeed(f"printf '{seed}' > /var/lib/velstra/node.env")
                # `start` on a unit whose condition fails is success: systemd
                # records it as skipped. What must NOT happen is a failed unit —
                # a cell without an overlay is a real way to run.
                node.succeed("systemctl start velstra-fabric-agent.service")
                state = node.succeed(
                    "systemctl show -p ActiveState --value velstra-fabric-agent.service"
                ).strip()
                assert state != "failed", f"a fabric-less cell must not fail the unit, got {state}"
                node.fail("test -e /run/fabric-agent-argv")
                # And it is discoverable rather than silent: the condition says
                # what is missing and what to do about it.
                #
                # `journal`, not `log`: the test driver already has a global
                # `log` (its logger), and the name is not rejected, it is
                # shadowed — the type checker catches it here, a plain
                # assignment would not have.
                journal = node.succeed("journalctl -u velstra-fabric-agent --no-pager -o cat")
                assert "VELSTRA_FABRIC_CONTROL" in journal, journal
                assert "separate no traffic" in journal, journal

            with subtest("naming a fabric brings the data plane up"):
                node.succeed(
                    f"printf '{seed}VELSTRA_FABRIC=http://fab:50052\n"
                    "VELSTRA_FABRIC_CONTROL=http://fab:50051\n"
                    "VELSTRA_FABRIC_VTEP=10.0.0.7\nVELSTRA_FABRIC_UNDERLAY=eth1\n'"
                    " > /var/lib/velstra/node.env"
                )
                node.succeed("systemctl daemon-reload")
                node.succeed("systemctl restart velstra-fabric-agent.service")
                node.wait_for_unit("velstra-fabric-agent.service")

            with subtest("it watches the agent-facing service, under the cell's node id"):
                argv = node.succeed("cat /run/fabric-agent-argv").strip()
                # The config service, NOT the orchestrator on :50052. They are
                # different services on different ports with different amounts
                # of trust, and pointing this at the other one gets
                # `unimplemented` — a confusing way to learn that.
                assert "--controller http://fab:50051" in argv, argv
                assert "50052" not in argv, f"that is the orchestrator, not the config service: {argv}"
                # The cell's node id, not the hostname the agent would default
                # to: the orchestrator is told about this host under that id.
                assert "--node-id node-1" in argv, argv

            with subtest("the node agent is given the overlay too"):
                # The other half of the same seed. Without these the agent keeps
                # its default datapath — real taps, nothing programmed — and a
                # port carrying security groups is refused rather than quietly
                # given a wire that enforces none of them.
                #
                # The generated start script, not `systemctl cat`: NixOS turns a
                # `script` into its own store path, so the unit file names a
                # wrapper and says nothing about what it runs.
                start = node.succeed(
                    "systemctl show -p ExecStart --value velstra-cloud-nodeagent.service"
                )
                # systemd renders ExecStart as a record — `{ path=/nix/...;
                # argv[]=...; }` — so the word carries a `path=` prefix.
                word = [w for w in start.split() if "unit-script" in w][0]
                path = word.split("=", 1)[1].rstrip(";")
                body = node.succeed(f"cat {path}")
                assert "--datapath fabric" in body, body
                assert "fabric-vtep" in body, body
                assert "fabric-underlay" in body, body

            with subtest("the data plane is ordered before the ports are asked for"):
                # A port programmed against a data plane that is not loaded yet
                # is a guest with a wire and no rules for however long the gap
                # lasts. Ordering is the whole fix, so it is asserted.
                after = node.succeed(
                    "systemctl show -p After --value velstra-cloud-nodeagent.service"
                )
                assert "velstra-fabric-agent.service" in after, after
          '';
        };

        # The installer ISO's first-boot wizard, driven prompt by prompt the
        # way an operator answers it, onto a blank disk — then the seed it
        # wrote is read back. The control plane is deliberately unreachable
        # during the install: the wizard records, the first boot registers.
        #   nix build .#checks.x86_64-linux.wizard -L
        wizard = pkgs.testers.runNixOSTest {
          name = "velstra-node-wizard";
          nodes.machine = {
            imports = [
              sentinel.nixosModules.applianceIso
              nodeIsoConfig
            ];
            virtualisation = {
              memorySize = 2048;
              mountHostNixStore = true;
              # vdb — the install target. Comfortably larger than the layout
              # the installer clones onto it (ESP + verity store + data);
              # sized at 8000 it was refused, correctly and by name, for being
              # 7.8 GiB against a layout needing 8.9.
              emptyDiskImages = [ 12000 ];
            };
            environment.systemPackages = [ pkgs.expect ];
          };
          testScript = ''
            import re

            token = "ab" * 32

            machine.wait_for_unit("multi-user.target")

            # The wizard's own candidate listing decides the pick — a
            # hardcoded disk number slides out of step on a machine with a
            # different disk set.
            listing = machine.succeed("velstra-cloud-node install </dev/null 2>&1 || true")
            m = re.search(r"\[(\d+)\] /dev/vdb ", listing)
            assert m, "the candidate listing does not offer /dev/vdb:\n" + listing
            pick = m.group(1)

            # Run it detached from the assertion so a failure can show the
            # transcript. `machine.succeed` would raise with nothing but an
            # exit code, and "the installer exited 1" is not a diagnosis of an
            # installer that had just printed the reason.
            status, _ = machine.execute(
                f"API_URL=http://cell.example:8443 TOKEN={token} PICK={pick}"
                f" expect ${./nix/node-wizard.exp} >/tmp/transcript 2>&1"
            )
            if status != 0:
                print(machine.succeed("cat -v /tmp/transcript"))
                raise Exception(f"the wizard did not finish: exit {status}")

            with subtest("the token never appears in the transcript"):
                machine.fail(f"grep -q {token} /tmp/transcript")

            with subtest("the wizard's seed is on the data partition, ready for first boot"):
                machine.succeed("mkdir -p /mnt && mount /dev/vdb6 /mnt")
                env = machine.succeed("cat /mnt/node.env")
                for line in [
                    "VELSTRA_NODE=node-1",
                    "VELSTRA_CELL=cell-1",
                    "VELSTRA_REGION=eu-central",
                    "VELSTRA_API_URL=http://cell.example:8443",
                    "VELSTRA_VMM=qemu",
                    "VELSTRA_HOSTNAME=node-t1",
                ]:
                    assert line in env, f"node.env is missing {line}:\n{env}"
                mode = machine.succeed("stat -c %a /mnt/node-token").strip()
                assert mode == "600", f"the token file is {mode}, not 600"
                machine.succeed(f"grep -qx {token} /mnt/node-token")
                # DHCP was chosen: no static units, so nothing to drift from
                # the image's networkd default.
                machine.fail("test -d /mnt/network")

            with subtest("the clone is a real sealed system, not just a seed"):
                # ESP + both slot-A partitions came over; slot B stayed empty.
                parts = machine.succeed("lsblk -rno NAME /dev/vdb")
                assert "vdb6" in parts, parts
                machine.succeed("sgdisk -p /dev/vdb | grep -q esp")
          '';
        };

        # A guest actually starts: the operator path (API → scheduler → agent)
        # ends in a real QEMU/KVM guest printing a kernel banner on its serial
        # console. Needs nested KVM — the agent's QEMU backend is accel=kvm by
        # design, so if /dev/kvm is missing inside the VM this fails loudly
        # rather than proving something weaker.
        #   nix build .#checks.x86_64-linux.guest -L
        guest = pkgs.testers.runNixOSTest {
          name = "velstra-cloud-guest";
          nodes.cell = {
            imports = [
              controlPlaneNode
              self.nixosModules.node
            ];
            velstra.cloud.node = {
              enable = true;
              package = velstra-cloud;
            };
            virtualisation = {
              memorySize = 4096;
              cores = 2;
              # Expose the host CPU's virtualisation extension to this VM so
              # the guest-of-a-guest can be KVM, matching the agent's backend.
              qemu.options = [
                "-cpu"
                "max"
              ];
            };
          };
          testScript = ''
            import json

            kernel = "${pkgs.linuxPackages.kernel}/bzImage"

            cell.wait_for_unit("velstra-cloud-api.service")
            cell.wait_for_unit("velstra-cloud-controller.service")

            with subtest("nested KVM is available (the agent's QEMU is accel=kvm)"):
                cell.succeed("test -c /dev/kvm")

            auth = "-H 'Authorization: Bearer opstoken'"
            ct = "-H 'Content-Type: application/json'"
            api = "http://127.0.0.1:8443/api/v1"
            cell.wait_until_succeeds(f"curl -fsS {auth} {api}/nodes")

            with subtest("node registered over its token, QEMU backend"):
                created = json.loads(cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"node-1\", \"spec\": {{\"schedulable\": true}}}}'"
                    f" {api}/nodes"
                ))
                token = created["nodeToken"]
                cell.succeed("mkdir -p /var/lib/velstra")
                cell.succeed(f"echo {token} > /var/lib/velstra/node-token")
                cell.succeed("chmod 600 /var/lib/velstra/node-token")
                # Seeded for QEMU with a direct kernel boot: the check's guest
                # payload is a bare kernel whose banner on the serial console
                # is the proof of life — no distribution image to fetch.
                cell.succeed(
                    "mkdir -p /run/systemd/system/velstra-cloud-nodeagent.service.d && "
                    "printf '[Service]\nExecStart=\nExecStart=${velstra-cloud}/bin/velstra-cloud-nodeagent "
                    "--node node-1 --cell cell-1 --region eu-central "
                    "--api http://127.0.0.1:8443 --api-token-file /var/lib/velstra/node-token "
                    "--vmm qemu --vmm-binary ${pkgs.qemu_kvm}/bin/qemu-system-x86_64 "
                    "--state-dir /var/lib/velstra "
                    f"--boot-kernel {kernel} --boot-cmdline console=ttyS0\n'"
                    " > /run/systemd/system/velstra-cloud-nodeagent.service.d/boot.conf"
                )
                cell.succeed(
                    "printf 'VELSTRA_NODE=node-1\nVELSTRA_CELL=cell-1\n"
                    "VELSTRA_REGION=eu-central\nVELSTRA_API_URL=http://127.0.0.1:8443\n"
                    "VELSTRA_VMM=qemu\nVELSTRA_HOSTNAME=cell\n'"
                    " > /var/lib/velstra/node.env"
                )
                cell.succeed("systemctl daemon-reload")
                cell.succeed("systemctl start velstra-cloud-nodeagent")
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/nodes/node-1 | grep -q vcpus",
                    timeout=120,
                )

            with subtest("project, image and instance via the API"):
                # Pre-placed under its published slug: content-addressed means
                # "already here" is a complete answer, so the agent skips the
                # fetch (hostfs::fetch_image's first check).
                cell.succeed("dd if=/dev/zero of=/root/guest.raw bs=1M count=8")
                cell.succeed(
                    "mkdir -p /var/lib/velstra/images && "
                    "cp /root/guest.raw"
                    " '/var/lib/velstra/images/projects~p1~images~sha256-${lib.concatStrings (lib.replicate 32 "ab")}'"
                )
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d @${pkgs.writeText "project.json" (builtins.toJSON {
                      id = "p1";
                      # An empty quota is the api tests' own idiom for "not the
                      # thing under test" (delete_guards.rs) — spec fields all
                      # default.
                      spec.quota = { };
                    })}"
                    f" {api}/projects"
                )
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d @${pkgs.writeText "image.json" (builtins.toJSON {
                      id = "sha256-${lib.concatStrings (lib.replicate 32 "ab")}";
                      spec = {
                        digest = "sha256:${lib.concatStrings (lib.replicate 32 "ab")}";
                        format = "Raw";
                        sizeBytes = 8388608;
                        sourceUrl = "file:///root/guest.raw";
                      };
                    })}"
                    f" {api}/projects/p1/images"
                )
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d @${pkgs.writeText "instance.json" (builtins.toJSON {
                      id = "g1";
                      spec = {
                        vcpus = 1;
                        memoryMib = 512;
                        image = "projects/p1/images/sha256-${lib.concatStrings (lib.replicate 32 "ab")}";
                        rootDiskGib = 1;
                        desiredState = "Running";
                        ports = [ ];
                      };
                    })}"
                    f" {api}/projects/p1/instances"
                )

            with subtest("the guest runs, and really booted a kernel"):
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/p1/instances/g1"
                    " | grep -qi running",
                    timeout=300,
                )
                console = "/var/lib/velstra/instances/projects~p1~instances~g1/console.log"
                cell.wait_until_succeeds(
                    f"grep -q 'Linux version' {console}",
                    timeout=300,
                )
          '';
        };
        # A machine taken out of service on purpose, end to end: the API accepts
        # a window, the scheduler stops placing on the node while it is open,
        # and — the claim the whole design rests on — service comes back when
        # the window *expires*, with nobody having flipped anything back.
        #
        # Nothing here is faked but the hypervisor: a real API, a real
        # controller and a real node agent, on a real clock. The guest is left
        # `Stopped`, so this needs no nested KVM and no image bytes — what is
        # under test is placement, not boot.
        #   nix build .#checks.x86_64-linux.maintenance -L
        maintenance = pkgs.testers.runNixOSTest {
          name = "velstra-cloud-maintenance";
          nodes.cell = {
            imports = [
              controlPlaneNode
              self.nixosModules.node
            ];
            velstra.cloud.node = {
              enable = true;
              package = velstra-cloud;
            };
            # A window closing is not a write, so nothing announces it: the
            # scheduler notices on its next pass. Five seconds rather than the
            # default three hundred — what is being shown is that one pass is
            # all it takes, not what the default happens to be.
            velstra.cloud.controlPlane.resyncSeconds = 5;
            virtualisation = {
              memorySize = 2048;
              cores = 2;
            };
          };
          testScript = ''
            import json
            import time

            cell.wait_for_unit("velstra-cloud-api.service")
            cell.wait_for_unit("velstra-cloud-controller.service")

            auth = "-H 'Authorization: Bearer opstoken'"
            ct = "-H 'Content-Type: application/json'"
            api = "http://127.0.0.1:8443/api/v1"
            cell.wait_until_succeeds(f"curl -fsS {auth} {api}/nodes")

            with subtest("a node registers and reports itself"):
                created = json.loads(cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"node-1\", \"spec\": {{\"schedulable\": true}}}}'"
                    f" {api}/nodes"
                ))
                token = created["nodeToken"]
                cell.succeed("mkdir -p /var/lib/velstra")
                cell.succeed(f"echo {token} > /var/lib/velstra/node-token")
                cell.succeed("chmod 600 /var/lib/velstra/node-token")
                cell.succeed(
                    "printf 'VELSTRA_NODE=node-1\nVELSTRA_CELL=cell-1\n"
                    "VELSTRA_REGION=eu-central\nVELSTRA_API_URL=http://127.0.0.1:8443\n"
                    "VELSTRA_VMM=fake\nVELSTRA_HOSTNAME=cell\n'"
                    " > /var/lib/velstra/node.env"
                )
                cell.succeed("systemctl start velstra-cloud-nodeagent")
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/nodes/node-1 | grep -q vcpus",
                    timeout=120,
                )

            with subtest("a project and an image to point a guest at"):
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct} -d '{{\"id\": \"p1\","
                    f" \"spec\": {{\"quota\": {{}}}}}}' {api}/projects"
                )
                slug = "sha256-" + "ab" * 32
                digest = "sha256:" + "ab" * 32
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"{slug}\", \"spec\": {{"
                    f"\"digest\": \"{digest}\", \"format\": \"Raw\","
                    f" \"sizeBytes\": 8388608,"
                    f" \"sourceUrl\": \"file:///dev/null\"}}}}'"
                    f" {api}/projects/p1/images"
                )

            with subtest("the node is declared out of service for a minute"):
                # The *cell's* clock, not the test driver's. They are two
                # machines, and a window is arithmetic on the clock of whoever
                # is reading it — taking the host's would make the window open
                # or expire at a time the API does not agree with.
                now = int(cell.succeed("date +%s%3N").strip())
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"dimm-swap\", \"spec\": {{"
                    f"\"node\": \"node-1\", \"startsAt\": {now},"
                    f" \"minutes\": 1, \"drain\": false,"
                    f" \"note\": \"swapping the failed DIMM in slot 3\"}}}}'"
                    f" {api}/maintenance-windows"
                )
                said = json.loads(cell.succeed(
                    f"curl -fsS {auth} {api}/nodes/node-1:explainMaintenance"
                ))
                assert said["open"] is not None, said
                assert said["open"]["note"].startswith("swapping"), said
                # Nothing is being asked to leave: that is what `drain: false`
                # means, and a fleet moving here would be the bug.
                assert said["draining"] is False, said

            with subtest("nothing new is placed there, in the operator's own words"):
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"g1\", \"spec\": {{"
                    f"\"vcpus\": 1, \"memoryMib\": 512,"
                    f" \"image\": \"projects/p1/images/{slug}\","
                    f" \"rootDiskGib\": 1, \"desiredState\": \"Stopped\","
                    f" \"ports\": []}}}}'"
                    f" {api}/projects/p1/instances"
                )

                def rejection():
                    answer = json.loads(cell.succeed(
                        f"curl -fsS {auth}"
                        f" {api}/projects/p1/instances/g1:explainPlacement"
                    ))
                    if answer["placed"] is not None:
                        return None
                    for r in answer["rejected"]:
                        if r["node"] == "node-1":
                            return r
                    return None

                deadline = time.time() + 60
                seen = None
                while time.time() < deadline:
                    seen = rejection()
                    if seen:
                        break
                    time.sleep(2)
                assert seen is not None, "a guest was placed on a node that is out of service"
                assert seen["why"] == "InMaintenance", seen
                # Relative, and carrying what the operator typed: "no valid
                # host" is not a sentence anybody can act on.
                assert "another" in seen["detail"], seen
                assert "DIMM" in seen["detail"], seen

                # And the operator's own two switches were never touched. That
                # is the whole design: a window is a declaration, and nothing
                # writes `schedulable` or `evacuate` on their behalf.
                node = json.loads(cell.succeed(f"curl -fsS {auth} {api}/nodes/node-1"))
                assert node["spec"]["schedulable"] is True, node
                assert node["spec"]["evacuate"] is False, node

            with subtest("the window ends and the machine takes work again"):
                # Nothing is flipped back and nothing is deleted: the window is
                # still there, it has simply stopped being open.
                #
                # Read as JSON rather than grepped. `grep node-1` over the whole
                # object passes the moment the guest is *refused*, because the
                # rejection chain on its condition names every node it was
                # refused by — so the loose version reported a placement that
                # had not happened, and the assertions below then ran while the
                # window was still open.
                def placed():
                    guest = json.loads(cell.succeed(
                        f"curl -fsS {auth} {api}/projects/p1/instances/g1"
                    ))
                    return guest["spec"]["node"]

                deadline = time.time() + 240
                where = None
                while time.time() < deadline:
                    where = placed()
                    if where:
                        break
                    time.sleep(3)
                assert where == "node-1", (
                    f"the guest was not placed once the window closed: {where!r}"
                )
                still = json.loads(cell.succeed(
                    f"curl -fsS {auth} {api}/maintenance-windows/dimm-swap"
                ))
                assert still["spec"]["node"] == "node-1", still
                after = json.loads(cell.succeed(
                    f"curl -fsS {auth} {api}/nodes/node-1:explainMaintenance"
                ))
                assert after["open"] is None, after
                node = json.loads(cell.succeed(f"curl -fsS {auth} {api}/nodes/node-1"))
                assert node["spec"]["schedulable"] is True, node
          '';
        };
        # Storage, end to end, on a real filesystem: a volume asked for through
        # the API is provisioned by a pool agent as a qcow2 file, a backup of it
        # lands on a target as real bytes, and a second volume is restored from
        # that copy.
        #
        # Until the pool module existed there was no process in any deployment
        # that would put a byte on a disk — the agent was a binary nothing
        # started. This is what makes that a property rather than a claim.
        #   nix build .#checks.x86_64-linux.storage -L
        storage = pkgs.testers.runNixOSTest {
          name = "velstra-cloud-storage";
          nodes.cell = {
            imports = [
              controlPlaneNode
              self.nixosModules.pool
            ];
            velstra.cloud.pool = {
              enable = true;
              package = velstra-cloud;
              id = "nvme";
              # The cell's own etcd, which the control plane brings up here.
              # `memory` would give this agent a store of its very own — it
              # would come up, report a pool nobody can see, and provision
              # nothing, which is a shape worth *not* writing a test around.
              store = "127.0.0.1:2379";
              resyncSeconds = 2;
            };
            virtualisation = {
              memorySize = 2048;
              diskSize = 4096;
            };
          };
          testScript = ''
            import json
            import time

            cell.wait_for_unit("velstra-cloud-api.service")
            cell.wait_for_unit("velstra-cloud-poolagent.service")

            auth = "-H 'Authorization: Bearer opstoken'"
            ct = "-H 'Content-Type: application/json'"
            api = "http://127.0.0.1:8443/api/v1"
            cell.wait_until_succeeds(f"curl -fsS {auth} {api}/pools")

            with subtest("the pool registers itself and reports what it has"):
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"nvme\", \"spec\": {{\"accepting\": true}}}}'"
                    f" {api}/pools"
                )
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/pools/nvme | grep -q capacityGib",
                    timeout=120,
                )

            with subtest("a volume asked for becomes a file on the disk"):
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct} -d '{{\"id\": \"p1\","
                    f" \"spec\": {{\"quota\": {{}}}}}}' {api}/projects"
                )
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"v1\", \"spec\": {{\"sizeGib\": 1,"
                    f" \"pool\": \"nvme\"}}}}'"
                    f" {api}/projects/p1/volumes"
                )
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/p1/volumes/v1"
                    " | grep -q '\"provisioned\":true'",
                    timeout=120,
                )
                # The bytes, not the object: a status that said `provisioned`
                # over an empty directory is exactly what this check exists to
                # rule out.
                cell.succeed("ls /var/lib/velstra/pool | grep -q qcow2")

            with subtest("a backup of it is real bytes on a target"):
                cell.succeed("mkdir -p /srv/archive")
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"archive\", \"spec\": {{\"kind\": \"directory\","
                    f" \"path\": \"/srv/archive\", \"accepting\": true,"
                    f" \"agent\": \"nvme\"}}}}'"
                    f" {api}/backup-targets"
                )
                # A target reports whether it is writable, and a backup is
                # refused until it has: the platform does not assume a path it
                # has never touched is one it can write.
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/backup-targets/archive | grep -q '\"writable\":true'",
                    timeout=120,
                )
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"b1\", \"spec\": {{"
                    f"\"volume\": \"projects/p1/volumes/v1\","
                    f" \"target\": \"backup-targets/archive\"}}}}'"
                    f" {api}/projects/p1/backups"
                )
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/p1/backups/b1 | grep -q '\"taken\":true'",
                    timeout=180,
                )
                # Named for the backup with its slashes flattened, so a person
                # can read the target with `ls` and two cells sharing one cannot
                # collide.
                cell.succeed("test -s /srv/archive/projects~p1~backups~b1")

            with subtest("a volume is restored from that copy"):
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"v2\", \"spec\": {{\"sizeGib\": 1,"
                    f" \"pool\": \"nvme\","
                    f" \"sourceBackup\": \"projects/p1/backups/b1\"}}}}'"
                    f" {api}/projects/p1/volumes"
                )
                cell.wait_until_succeeds(
                    f"curl -fsS {auth} {api}/projects/p1/volumes/v2"
                    " | grep -q '\"provisioned\":true'",
                    timeout=180,
                )

            with subtest("a restore with no copy to read is refused, never made blank"):
                cell.succeed(
                    f"curl -fsS -X POST {auth} {ct}"
                    f" -d '{{\"id\": \"v3\", \"spec\": {{\"sizeGib\": 1,"
                    f" \"pool\": \"nvme\","
                    f" \"sourceBackup\": \"projects/p1/backups/never\"}}}}'"
                    f" {api}/projects/p1/volumes"
                )
                # Given several passes to get it wrong, then asked.
                time.sleep(10)
                v3 = json.loads(cell.succeed(
                    f"curl -fsS {auth} {api}/projects/p1/volumes/v3"
                ))
                assert v3["status"].get("provisioned") is not True, (
                    f"a volume asked to be a restore was made blank: {v3['status']}"
                )
                ready = [c for c in v3["status"]["conditions"] if c["kind"] == "Ready"]
                assert ready and ready[0]["status"] == "False", v3["status"]
                assert "nothing to restore from" in ready[0]["message"], ready[0]
          '';
        };
        # What is actually in the .deb, asked of the .deb.
        #
        # A package is a claim about a machine somebody else will run, and the
        # cheapest way for it to be wrong is silently: a unit that did not make
        # it in, a postinst that is not executable, a binary that is a dangling
        # symlink into a /nix that is not there.
        #   nix build .#checks.x86_64-linux.deb -L
        deb =
          pkgs.runCommand "velstra-cloud-deb-check"
            {
              nativeBuildInputs = [
                pkgs.dpkg
                pkgs.patchelf
                pkgs.llvm
              ];
            }
            ''
              deb=${self.packages.${system}.deb}
              contents=$(dpkg-deb --contents "$deb")

              for want in \
                ./usr/bin/velstra-cloud-api \
                ./usr/bin/velstra-cloud-controller \
                ./usr/bin/velstra-cloud-nodeagent \
                ./usr/bin/velstra-cloud-poolagent \
                ./usr/bin/velstra-cloud-node \
                ./lib/systemd/system/velstra-cloud-api.service \
                ./lib/systemd/system/velstra-cloud-controller.service \
                ./lib/systemd/system/velstra-cloud-nodeagent.service \
                ./lib/systemd/system/velstra-cloud-poolagent.service \
                ./lib/systemd/system/velstra-fabric-agent.service \
                ./usr/share/doc/velstra-cloud/copyright; do
                echo "$contents" | grep -q " $want\$" || {
                  echo "the package is missing $want" >&2
                  echo "$contents" >&2
                  exit 1
                }
              done

              # Real files, not symlinks into a /nix that is not on the target.
              # A .deb that depended on the store existing would be a Nix
              # installation wearing a Debian filename.
              if echo "$contents" | grep -E "^l.* \./usr/bin/" ; then
                echo "a binary in the package is a symlink" >&2
                exit 1
              fi

              # And no binary may name the store either — which is the check
              # that was missing, and the reason a package installed cleanly and
              # could not run a single one of its five programs.
              #
              # An ELF binary carries the path of its interpreter inside itself.
              # Copying the file out of the store does not change that path, so
              # every binary here still demanded a /nix/store glibc that a
              # Debian machine does not have; the kernel refused to start them
              # and the shell said "required file not found" about a file that
              # was plainly there.
              mkdir -p bins
              dpkg-deb --fsys-tarfile "$deb" | tar -x -C bins ./usr/bin
              for b in bins/usr/bin/*; do
                  interp=$(patchelf --print-interpreter "$b")
                if [ "$interp" != "/lib64/ld-linux-x86-64.so.2" ]; then
                  echo "::error::$(basename "$b") wants the interpreter $interp," >&2
                  echo "which is not on a Debian machine — it would install and refuse to run." >&2
                  exit 1
                fi
                if patchelf --print-rpath "$b" | grep -q "/nix/store"; then
                  echo "$(basename "$b") carries an rpath into the Nix store" >&2
                  exit 1
                fi
              done
              # The glibc floor the package promises has to be the one the
              # binaries actually need: `Depends: libc6 (>= 2.39)` is a claim,
              # and apt believes it. Too low and it installs on a release whose
              # loader cannot resolve the symbols; too high and it refuses
              # machines that would have worked.
              want=$(for b in bins/usr/bin/*; do
                       llvm-readelf --dyn-syms "$b" | grep -oE "GLIBC_2\.[0-9]+"
                     done | sort -V | tail -1)
              # Asked of the package rather than of a file this script has not
              # unpacked yet — which is what the first version of this check did,
              # and it failed on a package whose control file said exactly the
              # right thing.
              dpkg-deb --field "$deb" Depends | grep -q "libc6 (>= ''${want#GLIBC_})" || {
                echo "::error::the binaries need $want and the package does not say so" >&2
                dpkg-deb --field "$deb" Depends >&2
                exit 1
              }

              # And nothing *inside* a unit may name the store either. The
              # binaries being copied is only half of it: the first build of
              # this package interpolated `''${velstra-cloud}/bin/…` into every
              # `ExecStart`, which installs cleanly and then fails on a machine
              # with no /nix — a package that is wrong only where nobody
              # building it can see.
              mkdir -p units
              dpkg-deb --fsys-tarfile "$deb" | tar -x -C units ./lib/systemd/system
              if grep -rn "/nix/store" units; then
                echo "a unit points into the Nix store, which is not on a Debian machine" >&2
                exit 1
              fi
              # The binary named by an absolute path, whatever wrapper is
              # around it — the agents are started through `sh -c` now, because
              # their answers come out of the seed and a static ExecStart cannot
              # carry them.
              #
              # With a message, because a bare `grep -q` under `set -e` fails
              # the whole derivation and prints nothing at all: this assertion
              # went stale when the unit changed shape and cost an hour of
              # bisecting a check that died in silence.
              grep -q "/usr/bin/velstra-cloud-poolagent" \
                units/lib/systemd/system/velstra-cloud-poolagent.service || {
                echo "the pool unit does not name its binary by absolute path:" >&2
                cat units/lib/systemd/system/velstra-cloud-poolagent.service >&2
                exit 1
              }

              # Every unit is conditional on its own role, and starts nothing on
              # a machine that has not been told what it is for.
              dpkg-deb --fsys-tarfile "$deb" | tar -xO ./lib/systemd/system/velstra-cloud-poolagent.service > unit
              grep -q "ExecCondition=.*has-role pool" unit || {
                echo "the pool unit is not conditional on the pool role:" >&2
                cat unit >&2
                exit 1
              }
              grep -q "EnvironmentFile=-/var/lib/velstra/node.env" unit

              # The fabric unit is gated twice, and both gates matter.
              #
              # A cell with no fabric is a legitimate way to run — it is what
              # every cell did before the data plane had a unit at all — and the
              # agent is a package this one only recommends. So on a hypervisor
              # whose seed names no fabric, and on one where `velstra` was never
              # installed, this must read as "not for this machine" rather than
              # as a service that failed. Both are ExecCondition, which systemd
              # records as skipped.
              dpkg-deb --fsys-tarfile "$deb" | tar -xO ./lib/systemd/system/velstra-fabric-agent.service > fab
              grep -q "ExecCondition=.*has-role hypervisor" fab || {
                echo "the fabric unit is not conditional on the hypervisor role:" >&2
                cat fab >&2
                exit 1
              }
              grep -q "VELSTRA_FABRIC_CONTROL" fab || {
                echo "the fabric unit would start on a node whose cell has no fabric:" >&2
                cat fab >&2
                exit 1
              }
              grep -q "command -v velstra" fab || {
                echo "the fabric unit does not check that the agent is installed:" >&2
                cat fab >&2
                exit 1
              }
              # It must load before the node agent asks the orchestrator to turn
              # a tap into a tenant port: a port programmed against a data plane
              # that is not up yet is a guest with a wire and no rules.
              grep -q "Before=velstra-cloud-nodeagent.service" fab || {
                echo "the fabric unit does not order itself before the node agent:" >&2
                cat fab >&2
                exit 1
              }

              # The licence travels with the software or it has not been
              # conveyed. Debian Policy §12.5 makes this file mandatory and
              # lintian errors on its absence, but the older reason is the one
              # that matters: the AGPL binds whoever receives the program, and
              # somebody who receives it without its terms has been given no
              # terms.
              dpkg-deb --fsys-tarfile "$deb" | tar -xO ./usr/share/doc/velstra-cloud/copyright > cr
              grep -q "^Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/" cr
              grep -q "AGPL-3.0-or-later" cr || {
                echo "the copyright file does not name the package licence:" >&2
                cat cr >&2
                exit 1
              }
              # And the vendored wire contract, which is somebody else's file
              # under somebody else's terms. Sweeping it under the package
              # licence would be stating something untrue about it.
              grep -q "velstra-cloud-fabric/proto/vendor/velstra.proto" cr || {
                echo "the copyright file does not carry the vendored proto's own terms:" >&2
                cat cr >&2
                exit 1
              }

              # `postinst` runs as root on somebody else's machine. One that is
              # not executable is a package that half-installs.
              dpkg-deb --control "$deb" ctrl
              test -x ctrl/postinst
              test -x ctrl/prerm
              grep -q "velstra-cloud-node setup" ctrl/postinst
              # And it deliberately enables nothing: a unit started before the
              # seed exists is an agent pointing at no cell, retrying for ever.
              #
              # Anchored to the start of a line, because the script *says* it
              # does not enable anything — and a grep for the bare string
              # matches the sentence explaining its own absence. Which it did,
              # on the first run of this check.
              if grep -qE "^[[:space:]]*systemctl enable" ctrl/postinst; then
                echo "postinst enables units; nothing may start before there is a seed" >&2
                exit 1
              fi

              touch $out
            '';
        # The setup wizard, driven answer by answer, with the seed read back.
        #
        # No VM: this wizard writes one file and touches nothing else, which is
        # the whole reason it is separate from the appliance installer. So the
        # check is a pipe and a `grep` — a second, and it exercises the thing an
        # operator actually types.
        #   nix build .#checks.x86_64-linux.setup -L
        setup =
          pkgs.runCommand "velstra-cloud-setup-check" { } ''
            mkdir -p seed
            # region, cell, roles (control-plane + hypervisor + pool), API url,
            # node id, token, hypervisor, pool id, backend, store, other cells,
            # reachable-from, admin + password twice, fabric (no), gateway
            # (yes), confirm.
            #
            # Positional, so a question added anywhere above shifts every answer
            # below it — which is exactly what happened when the fabric question
            # arrived: `y` began answering "name a fabric?" instead of "write
            # this?", and the run derailed with no clue attached. It is worth
            # keeping positional rather than driving it with expect: this is the
            # cheapest check in the tree and it is the one that catches a
            # question nobody meant to add.
            ${velstra-cloud}/bin/velstra-cloud-node setup --dir "$PWD/seed" --nixos false <<'ANSWERS' > out 2>&1
            eu-north
            cell-7
            1 2 3
            https://cell-7.example:8443
            node-a
            ${lib.concatStrings (lib.replicate 32 "ab")}
            2
            nvme
            1
            10.0.0.1:2379
            cell-8=https://cell-8.example:8443
            2
            admin
            correcthorsebattery
            correcthorsebattery
            n
            y
            y
            ANSWERS

            seed=$(cat seed/node.env)
            echo "$seed"

            # Every answer, in the file, spelled the way a unit reads it.
            grep -qx "VELSTRA_REGION=eu-north" seed/node.env
            grep -qx "VELSTRA_CELL=cell-7" seed/node.env
            grep -qx "VELSTRA_ROLES=control-plane,hypervisor,pool" seed/node.env
            grep -qx "VELSTRA_NODE=node-a" seed/node.env
            grep -qx "VELSTRA_VMM=cloud-hypervisor" seed/node.env
            grep -qx "VELSTRA_POOL=nvme" seed/node.env
            grep -qx "VELSTRA_POOL_BACKEND=directory" seed/node.env
            grep -qx "VELSTRA_STORE=10.0.0.1:2379" seed/node.env
            grep -qx "VELSTRA_CELLS=cell-8=https://cell-8.example:8443" seed/node.env
            # The answer that decides whether this node's guests can be reached
            # at all. A cell with no fabric and no first hop runs guests that
            # boot, report Running, and can be logged into by nobody — so the
            # wizard asks, and the seed has to carry it.
            grep -qx "VELSTRA_LOCAL_NETWORK=1" seed/node.env
            # The cell's first administrator. Without one the API comes up,
            # serves the console and refuses every sign-in — it says so in a
            # warning, which is not where somebody looking at a login form is
            # looking. The Debian path had no way to supply it at all: the
            # wizard never asked, the seed never carried it, and the unit passed
            # nothing, so a fresh install produced a control plane nobody could
            # get into.
            grep -qx "VELSTRA_BOOTSTRAP_ADMIN=admin" seed/node.env

            # Where the API binds. The default is loopback, and a control plane
            # nobody can reach from another machine is the first thing somebody
            # hits and the last thing they think to look for: every unit green,
            # and the browser saying nothing answered.
            grep -qx "VELSTRA_LISTEN=0.0.0.0:8443" seed/node.env

            # The username is not a secret and the password is. Same split the
            # node token already makes, same mode.
            if grep -q "correcthorsebattery" seed/node.env; then
              echo "the bootstrap password is in the world-readable seed" >&2
              exit 1
            fi
            grep -q "correcthorsebattery" seed/bootstrap-password
            test "$(stat -c %a seed/bootstrap-password)" = 600

            # No fabric was named, so no key for one. A cell that programs no
            # overlay is a real way to run; what must not happen is a seed that
            # half-describes one.
            if grep -q "VELSTRA_FABRIC" seed/node.env; then
              echo "a cell that declined a fabric was given one anyway" >&2
              exit 1
            fi

            # The token is the one secret here, and it is not in the file every
            # unit reads.
            if grep -q "${lib.concatStrings (lib.replicate 32 "ab")}" seed/node.env; then
              echo "the token is in the world-readable seed" >&2
              exit 1
            fi
            grep -q "${lib.concatStrings (lib.replicate 32 "ab")}" seed/node-token
            test "$(stat -c %a seed/node-token)" = 600
            test "$(stat -c %a seed/node.env)" = 644

            # And the same wizard, answering the fabric question this time.
            #
            # Its own run rather than more asserts on the one above, because the
            # two answers produce genuinely different seeds and the interesting
            # one is what a *declined* fabric leaves out.
            mkdir -p fabric-seed
            ${velstra-cloud}/bin/velstra-cloud-node setup --dir "$PWD/fabric-seed" --nixos false <<'ANSWERS' > fabout 2>&1
            eu-north
            cell-7
            2
            https://cell-7.example:8443
            node-b
            ${lib.concatStrings (lib.replicate 32 "cd")}
            1
            y
            http://fab.example:50052
            http://fab.example:50051
            10.0.0.7
            eth1
            fc00:0:1::/64
            y
            ANSWERS
            cat fabric-seed/node.env

            grep -qx "VELSTRA_FABRIC=http://fab.example:50052" fabric-seed/node.env
            # The orchestrator and the agent-facing service are different
            # endpoints with different amounts of trust. A seed that carried one
            # twice would be a node talking to the wrong one.
            grep -qx "VELSTRA_FABRIC_CONTROL=http://fab.example:50051" fabric-seed/node.env
            grep -qx "VELSTRA_FABRIC_VTEP=10.0.0.7" fabric-seed/node.env
            grep -qx "VELSTRA_FABRIC_UNDERLAY=eth1" fabric-seed/node.env
            grep -qx "VELSTRA_FABRIC_SRV6_LOCATOR=fc00:0:1::/64" fabric-seed/node.env
            # And the question that only a fabric-less cell is asked is not
            # asked here — two things owning the far end of every tap is a
            # combination the agent refuses at startup, so the wizard must not
            # be able to produce it.
            if grep -q "VELSTRA_LOCAL_NETWORK" fabric-seed/node.env; then
              echo "the wizard offered a first hop to a cell that has a fabric" >&2
              exit 1
            fi
            # A hypervisor is told to enable the data plane; it is not one of the
            # roles, so this line only appears when a fabric was actually named.
            grep -q "systemctl enable --now velstra-fabric-agent" fabout

            # Told what to enable, and told what it cannot do for them.
            grep -q "systemctl enable --now velstra-cloud-nodeagent" out
            grep -q "systemctl enable --now velstra-cloud-poolagent" out
            grep -q "cannot mark itself a gateway" out

            touch $out
          '';
      };
    };
}
