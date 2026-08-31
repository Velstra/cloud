# The Debian package, built from the same source as everything else.
#
# ## Why a .deb at all
#
# A sealed appliance is the right shape for a machine you flash and forget, and
# the wrong one for a machine somebody already runs. Plenty of fleets are
# Debian, and telling them "install NixOS first" is telling them to run a
# different platform. So the same binaries ship as a package, the same wizard
# writes the same seed, and the same units read it.
#
# ## Units are installed, never enabled
#
# The package puts four units on the machine and starts none of them. That is
# not caution, it is the same rule the appliance already follows: a unit is
# conditional on **its role being in the seed**, and a machine with no seed has
# no roles. A package that started an agent at install time would start one
# pointing at no cell, with no token, retrying for ever — and the first thing
# anybody would learn about this platform is a red unit.
#
# `velstra-cloud-node setup` asks the questions, writes the seed and says which
# units to enable. On Debian it can enable them; on NixOS it prints the module
# instead, because units there are a declaration and a wizard reaching into them
# would be fighting the operating system.
#
# ## What is deliberately not here
#
# No etcd, no QEMU, no Ceph. They are `Depends:` and `Recommends:`, resolved by
# apt against Debian's own packages — a platform that vendored its own copy of
# etcd would be a platform whose security updates are its own problem.
{
  pkgs,
  lib,
  velstra-cloud,
  version,
}:
let
  # One unit per role, each conditional on the role being in the seed.
  #
  # `ExecCondition` rather than `ConditionPathExists`: the question is not
  # whether a file is there but what it says, and systemd treats a non-zero
  # ExecCondition as "not for this machine" rather than as a failure — which is
  # exactly the difference between "this box is not a pool" and "the pool agent
  # is broken".
  # `/usr/bin`, never the store path.
  #
  # The binaries are *copied* into the package, and a unit that interpolated
  # `${velstra-cloud}/bin/...` would point at a `/nix/store` that does not exist
  # on the target — a package that installs cleanly and whose every unit then
  # fails with "no such file". Which is what the first build of this file did,
  # and why the check now greps the units for the store prefix.
  bin = name: "/usr/bin/${name}";

  roleGuard = role: ''
    ExecCondition=${bin "velstra-cloud-node"} has-role ${role}
  '';

  unit =
    {
      role,
      description,
      exec,
      pre ? "",
      after ? "network-online.target",
    }:
    ''
      [Unit]
      Description=${description}
      Wants=network-online.target
      After=${after}

      [Service]
      Type=simple
      Restart=on-failure
      RestartSec=5
      # The machine's own, and only the machine's own. The state directory
      # holds this machine's guests, and a cell whose machines share one
      # filesystem — which is what makes moving a guest possible at all — has
      # every agent there reading the same `node.env` and answering to the same
      # name. Found by building that arrangement on two real machines: the
      # second one to mount it renamed the first, and the next package upgrade
      # took the control plane down, because `has-role control-plane` was
      # reading a hypervisor's seed.
      #
      # Reading both and letting /etc win is not the fix — the keys /etc does
      # not mention would still come from the neighbour, so a control plane
      # would inherit a hypervisor's pool. `migrate-seed` in postinst moves an
      # older machine's seed here, so there is exactly one file to read.
      EnvironmentFile=-/etc/velstra/node.env
      ${pre}${roleGuard role}ExecStart=${exec}

      [Install]
      WantedBy=multi-user.target
    '';

  units = {
    # The API reads the bootstrap password out of its own 0600 file rather than
    # taking it on a command line: an argument is visible in `ps` to every user
    # on the machine, and this one is the cell's first administrator. `-f` so a
    # cell whose administrator already exists — every start after the first —
    # simply starts.
    "velstra-cloud-api.service" = unit {
      role = "control-plane";
      description = "Velstra Cloud API";
      exec = ''
        /bin/sh -c 'if [ -f /var/lib/velstra/bootstrap-password ]; then \
          VELSTRA_BOOTSTRAP_PASSWORD="$(cat /var/lib/velstra/bootstrap-password)"; \
          export VELSTRA_BOOTSTRAP_PASSWORD; fi; \
          : "''${VELSTRA_STORE_BACKUP_DIR:=/var/lib/velstra/store-backups}"; \
          export VELSTRA_STORE_BACKUP_DIR; \
          exec ${bin "velstra-cloud-api"}' '';
      # The backup dir defaults on rather than off: a cell whose store is gone
      # is a cell nobody will ever manage again, and the price of a day of
      # hourly snapshots is megabytes. The default lands on the machine's own
      # disk, which survives a crashed process and a bad upgrade but not the
      # disk itself — the seed can point VELSTRA_STORE_BACKUP_DIR somewhere
      # better, and docs/operations.md says to.
    };
    "velstra-cloud-controller.service" = unit {
      role = "control-plane";
      description = "Velstra Cloud controllers";
      exec = bin "velstra-cloud-controller";
    };
    # Both agents are handed their answers on the command line, built from the
    # seed — the same translation the NixOS modules do, for the same reason.
    #
    # They used to be started bare, on the assumption that `EnvironmentFile`
    # was enough. It is not: `--node`, `--cell`, `--region` and `--pool` are
    # required arguments with no environment fallback, so both units died
    # immediately with "the following required arguments were not provided" and
    # restarted for ever. The package installed, the units were enabled, and
    # neither agent ever ran.
    #
    # Written as a shell line rather than a static ExecStart because the answers
    # are only known at runtime, and because `--vmm-binary` has to be resolved
    # from the seed: a transient guest unit does not inherit this unit's PATH,
    # so the hypervisor must be named absolutely.
    #
    # `tok` in the command below is the node's own token, and it is the one thing
    # in the state directory that cannot be shared. A cell whose machines mount
    # one filesystem — which is what makes moving a guest possible at all, see
    # `--shared-state` — would otherwise have every agent reading the same token
    # and speaking as the same node. `/etc/velstra/node-token` wins when it is
    # there. It is a shell `[ -f ]` and not a comment because systemd joins these
    # continuations into a single line, where a `#` would swallow the rest.
    "velstra-cloud-nodeagent.service" = unit {
      role = "hypervisor";
      description = "Velstra Cloud node agent";
      # The metadata address, which the agent binds and treats as fatal — a cell
      # whose guests silently get no metadata is worse than a node that says so
      # at startup. NixOS gives it a networkd dummy interface; a Debian machine
      # may not run networkd at all, so the unit makes it itself.
      #
      # Without this the agent got as far as connecting to the store and then
      # died on "Cannot assign requested address", which is the third thing the
      # NixOS module did that this package did not.
      #
      # `-` on each line: re-running is normal (a restart, a second start after
      # a crash) and an interface that already exists is success, not failure.
      pre = ''
        ExecStartPre=-/usr/sbin/ip link add vmeta0 type dummy
        ExecStartPre=-/usr/sbin/ip addr add 169.254.169.254/32 dev vmeta0
        ExecStartPre=-/usr/sbin/ip link set vmeta0 up
      '';
      exec = ''
        /bin/sh -c 'case "''${VELSTRA_VMM:-}" in \
          qemu) vmm=/usr/bin/qemu-system-x86_64 ;; \
          cloud-hypervisor) vmm=/usr/bin/cloud-hypervisor ;; \
          fake) vmm= ;; \
          *) echo "node.env sets VELSTRA_VMM=''${VELSTRA_VMM:-} — expected qemu, cloud-hypervisor or fake" >&2; exit 1 ;; \
        esac; \
          tok=/var/lib/velstra/node-token; \
          [ -f /etc/velstra/node-token ] && tok=/etc/velstra/node-token; \
        fab=; \
        if [ -n "''${VELSTRA_FABRIC:-}" ]; then \
          fab="--datapath fabric --fabric $VELSTRA_FABRIC --fabric-vtep $VELSTRA_FABRIC_VTEP --fabric-underlay $VELSTRA_FABRIC_UNDERLAY"; \
          [ -n "''${VELSTRA_FABRIC_SRV6_LOCATOR:-}" ] && fab="$fab --fabric-srv6-locator $VELSTRA_FABRIC_SRV6_LOCATOR"; \
        fi; \
        exec ${bin "velstra-cloud-nodeagent"} \
          --node "$VELSTRA_NODE" --cell "$VELSTRA_CELL" --region "$VELSTRA_REGION" \
          --api "$VELSTRA_API_URL" --api-token-file "$tok" \
          --vmm "$VELSTRA_VMM" ''${vmm:+--vmm-binary "$vmm"} \
          --state-dir /var/lib/velstra \
          ''${VELSTRA_CONSOLE_LISTEN:+--console-listen "$VELSTRA_CONSOLE_LISTEN"} \
          ''${VELSTRA_CONSOLE_ADVERTISE:+--console-advertise "$VELSTRA_CONSOLE_ADVERTISE"} \
          ''${VELSTRA_SHARED_STATE:+--shared-state} \
          $fab' '';
    };
    # Two ways to run, and a token decides which. With one, this pool reads *and
    # writes* through the API — the only way it can live on a machine that is not
    # the control plane's, which has no etcd for it to open. The agent opened one
    # anyway and died with `invalid uri: empty string`, so the `pool + hypervisor`
    # answer the setup wizard offers produced a unit that could not start.
    #
    # Without a token it is the control plane's own machine and goes straight to
    # the store, as it always did.
    "velstra-cloud-poolagent.service" = unit {
      role = "pool";
      description = "Velstra Cloud storage pool agent";
      exec = ''
        /bin/sh -c 'tok=/var/lib/velstra/pool-token; \
        [ -f /etc/velstra/pool-token ] && tok=/etc/velstra/pool-token; \
        if [ -f "$tok" ]; then \
          exec ${bin "velstra-cloud-poolagent"} \
            --pool "$VELSTRA_POOL" --cell "$VELSTRA_CELL" --region "$VELSTRA_REGION" \
            --api "$VELSTRA_API_URL" --api-token-file "$tok" \
            --backend "$VELSTRA_POOL_BACKEND"; \
        fi; \
        exec ${bin "velstra-cloud-poolagent"} \
          --pool "$VELSTRA_POOL" --cell "$VELSTRA_CELL" --region "$VELSTRA_REGION" \
          --store "$VELSTRA_STORE" --backend "$VELSTRA_POOL_BACKEND"' '';
    };
    "velstra-fabric-agent.service" = fabricUnit;
  };

  # The fabric data plane, which is not one of the four.
  #
  # Two things separate it from the units above and each is a reason it is
  # written out by hand rather than folded into `unit`:
  #
  # It is gated on **two** conditions, not one — the hypervisor role AND a
  # fabric named in the seed. A cell without a fabric is a real way to run, and
  # on such a machine this must read as "not for this box", not as a failure.
  #
  # And its binary is not ours. `velstra` is the fabric agent; this package
  # does not ship it, because vendoring somebody else's eBPF data plane into a
  # Debian package would make its kernel compatibility our problem. It is a
  # `Recommends:`, and the condition below means an operator who has not
  # installed it gets a skipped unit rather than a red one.
  fabricUnit = ''
    [Unit]
    Description=Velstra Fabric data plane (eBPF/XDP)
    Wants=network-online.target
    After=network-online.target
    Before=velstra-cloud-nodeagent.service

    [Service]
    Type=simple
    Restart=on-failure
    RestartSec=2
    RuntimeDirectory=velstra
    RuntimeDirectoryMode=0700
    EnvironmentFile=-/etc/velstra/node.env
    ${roleGuard "hypervisor"}ExecCondition=/bin/sh -c 'command -v velstra >/dev/null || { echo "the fabric agent (velstra) is not installed; tenant networks here separate no traffic"; exit 1; }; grep -qE "^VELSTRA_FABRIC_CONTROL=." /etc/velstra/node.env || { echo "no VELSTRA_FABRIC_CONTROL in the seed: this cell has no data plane"; exit 1; }'
    ExecStart=/bin/sh -c 'exec velstra run --controller "$VELSTRA_FABRIC_CONTROL" --node-id "$VELSTRA_NODE"'
    AmbientCapabilities=CAP_BPF CAP_NET_ADMIN CAP_SYS_ADMIN
    CapabilityBoundingSet=CAP_BPF CAP_NET_ADMIN CAP_SYS_ADMIN
    NoNewPrivileges=true
    ProtectHome=true
    RestrictSUIDSGID=true
    LockPersonality=true

    [Install]
    WantedBy=multi-user.target
  '';

  # Debian's own name for this machine's architecture. Only amd64 is built here;
  # anything else is a cross-compile question and not a packaging one.
  debArch = "amd64";
in
pkgs.runCommand "velstra-cloud_${version}_${debArch}.deb"
  {
    nativeBuildInputs = [
      pkgs.dpkg
      pkgs.patchelf
    ];
    meta.description = "Velstra Cloud as a Debian package";
  }
  ''
    root=$PWD/pkg
    mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/lib/systemd/system" \
             "$root/var/lib/velstra" "$root/usr/share/doc/velstra-cloud"
    # Where a machine keeps who it is: its seed and its agent tokens. Shipped
    # so that the wizard, the postinst migration and an operator dropping a
    # pool token in all find it there, on a fresh install too. 0700 because a
    # token in it is a credential.
    install -d -m 0700 "$root/etc/velstra"

    # The binaries, copied rather than symlinked into the store — and then
    # pointed at Debian's own dynamic linker, which is the half that was
    # missing.
    #
    # Copying the file was necessary and not sufficient. An ELF binary names its
    # interpreter *inside itself*, and a Nix-built one names
    # `/nix/store/…-glibc-2.42-67/lib/ld-linux-x86-64.so.2`. On a machine with no
    # /nix that path does not exist, so the kernel refuses to start it and the
    # shell reports "cannot execute: required file not found" — about the
    # interpreter, not about the binary, which is sitting right there. The
    # package installed cleanly and not one of its five binaries could run.
    #
    # The comment this replaces claimed copying was what stopped the package
    # being "a Nix installation wearing a Debian filename". It was not, and
    # nothing checked: the deb check grepped the *units* for /nix/store and
    # never looked at the binaries. It does now.
    #
    # The rpath goes too — it points into the store as well, and an empty one
    # sends the loader to the system paths, which is where Debian's libraries
    # are.
    for b in velstra-cloud-api velstra-cloud-controller velstra-cloud-nodeagent \
             velstra-cloud-poolagent velstra-cloud-node; do
      cp ${velstra-cloud}/bin/$b "$root/usr/bin/$b"
      chmod 0755 "$root/usr/bin/$b"
      patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 \
               --set-rpath "" "$root/usr/bin/$b"
    done

    ${lib.concatStringsSep "\n" (
      lib.mapAttrsToList (name: text: ''
        cat > "$root/lib/systemd/system/${name}" <<'UNIT'
        ${text}
        UNIT
      '') units
    )}

    cat > "$root/DEBIAN/control" <<CONTROL
    Package: velstra-cloud
    Version: ${version}
    Section: admin
    Priority: optional
    Architecture: ${debArch}
    Maintainer: Velstra <noreply@velstra.invalid>
    Depends: systemd, libc6 (>= 2.39), iproute2, nftables, curl
    Recommends: qemu-system-x86, qemu-utils, etcd-server, etcd-client, ceph-common, velstra
    Description: Velstra Cloud — control plane, node agent and storage pool
     One package, four roles. Which of them this machine runs is decided by
     \`velstra-cloud-node setup\`, which writes /etc/velstra/node.env; every
     unit is conditional on its own role being named there, so installing this
     package starts nothing until somebody has said what the machine is for.
     .
     Carrying external traffic is deliberately not one of the roles here: that
     is what the cell believes about a machine, set on its node object by an
     operator. A registration token exists so a machine can report, and one that
     could also declare its holder a gateway would grant itself the cell's
     external traffic.
    CONTROL

    cat > "$root/DEBIAN/postinst" <<'POSTINST'
    #!/bin/sh
    set -e
    # Deliberately no `systemctl enable`. Every unit here is conditional on its
    # role being in /etc/velstra/node.env, and a machine that has just been
    # unpacked has no seed — so enabling them would start agents pointing at no
    # cell, with no token, retrying for ever.
    if [ "$1" = "configure" ]; then
      systemctl daemon-reload >/dev/null 2>&1 || true
      # An existing machine, brought up to what this package needs. The one that
      # matters today: a cell that grew TLS leaves an agent still speaking plain
      # HTTP to a TLS port, which stops the node following its cell entirely and
      # says so only as "invalid HTTP version parsed" in a journal. Safe to run
      # on a fresh install too — there is no seed yet, and it does nothing.
      # Also moves an older machine's seed out of the shared state directory
      # and into /etc, where it belongs to this machine and no other.
      velstra-cloud-node migrate-seed 2>/dev/null || true
      if [ ! -f /etc/velstra/node.env ]; then
        echo ""
        echo "velstra-cloud is installed and nothing is running."
        echo ""
        echo "One machine, the whole cell — for a laptop or a home server:"
        echo ""
        echo "    sudo velstra-cloud-node quickstart"
        echo ""
        echo "One machine in a larger cell, or an unattended install:"
        echo ""
        echo "    sudo velstra-cloud-node setup            # asks"
        echo "    sudo velstra-cloud-node setup --config …  # reads a seed"
        echo ""
      fi
    fi
    POSTINST
    chmod 0755 "$root/DEBIAN/postinst"

    cat > "$root/DEBIAN/prerm" <<'PRERM'
    #!/bin/sh
    set -e
    # The seed is not ours to remove: it holds which cell this machine belongs
    # to and the credential it was given. A purge that took it would make
    # reinstalling mean re-registering.
    if [ "$1" = "remove" ]; then
      for u in velstra-cloud-api velstra-cloud-controller \
               velstra-cloud-nodeagent velstra-cloud-poolagent \
               velstra-fabric-agent; do
        systemctl stop "$u" >/dev/null 2>&1 || true
      done
    fi
    PRERM
    chmod 0755 "$root/DEBIAN/prerm"

    cp ${../docs/install.md} "$root/usr/share/doc/velstra-cloud/install.md"

    # `copyright`, which Debian Policy §12.5 makes mandatory: every package must
    # carry a verbatim copy of its licence, in this file, at this path. It was
    # missing, and lintian errors on that — but the real point is smaller and
    # older than lintian: the AGPL is only conveyed if the person who receives
    # the software receives its terms with it.
    #
    # Machine-readable format 1.0, so the archive tooling can read it, and with
    # the vendored proto called out separately: it is MIT OR Apache-2.0 and a
    # copyright file that swept it under the package licence would be stating
    # something untrue about somebody else's file.
    cat > "$root/usr/share/doc/velstra-cloud/copyright" <<'COPYRIGHT'
    Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
    Upstream-Name: velstra-cloud
    Source: https://github.com/Velstra/cloud

    Files: *
    Copyright: 2026 Maximilian Brandt and contributors
    License: AGPL-3.0-or-later

    Files: velstra-cloud-fabric/proto/vendor/velstra.proto
    Copyright: 2026 Maximilian Brandt and contributors
    License: MIT or Apache-2.0
     The wire contract is vendored from the Velstra Fabric repository, where it
     is published under permissive terms because it is linked into both a
     GPL-licensed eBPF object and an AGPL control plane. Vendoring it here does
     not change that.

    License: AGPL-3.0-or-later
     This program is free software: you can redistribute it and/or modify it
     under the terms of the GNU Affero General Public License as published by
     the Free Software Foundation, either version 3 of the License, or (at your
     option) any later version.
     .
     This program is distributed in the hope that it will be useful, but
     WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY
     or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU Affero General Public
     License for more details.
     .
     You should have received a copy of the GNU Affero General Public License
     along with this program.  If not, see <https://www.gnu.org/licenses/>.
     .
     On Debian systems, the complete text of the GNU Affero General Public
     License version 3 can be found in "/usr/share/common-licenses/AGPL-3".
    COPYRIGHT

    dpkg-deb --root-owner-group --build "$root" "$out"
  ''
