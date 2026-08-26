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
      EnvironmentFile=-/var/lib/velstra/node.env
      ${roleGuard role}ExecStart=${exec}

      [Install]
      WantedBy=multi-user.target
    '';

  units = {
    "velstra-cloud-api.service" = unit {
      role = "control-plane";
      description = "Velstra Cloud API";
      exec = bin "velstra-cloud-api";
    };
    "velstra-cloud-controller.service" = unit {
      role = "control-plane";
      description = "Velstra Cloud controllers";
      exec = bin "velstra-cloud-controller";
    };
    "velstra-cloud-nodeagent.service" = unit {
      role = "hypervisor";
      description = "Velstra Cloud node agent";
      exec = bin "velstra-cloud-nodeagent";
    };
    "velstra-cloud-poolagent.service" = unit {
      role = "pool";
      description = "Velstra Cloud storage pool agent";
      exec = bin "velstra-cloud-poolagent";
    };
  };

  # Debian's own name for this machine's architecture. Only amd64 is built here;
  # anything else is a cross-compile question and not a packaging one.
  debArch = "amd64";
in
pkgs.runCommand "velstra-cloud_${version}_${debArch}.deb"
  {
    nativeBuildInputs = [ pkgs.dpkg ];
    meta.description = "Velstra Cloud as a Debian package";
  }
  ''
    root=$PWD/pkg
    mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/lib/systemd/system" \
             "$root/var/lib/velstra" "$root/usr/share/doc/velstra-cloud"

    # The binaries, copied rather than symlinked into the store: a .deb that
    # depended on /nix existing on the target would be a Nix installation
    # wearing a Debian filename.
    for b in velstra-cloud-api velstra-cloud-controller velstra-cloud-nodeagent \
             velstra-cloud-poolagent velstra-cloud-node; do
      cp ${velstra-cloud}/bin/$b "$root/usr/bin/$b"
      chmod 0755 "$root/usr/bin/$b"
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
    Depends: systemd
    Recommends: qemu-system-x86, qemu-utils, etcd-server, ceph-common
    Description: Velstra Cloud — control plane, node agent and storage pool
     One package, four roles. Which of them this machine runs is decided by
     \`velstra-cloud-node setup\`, which writes /var/lib/velstra/node.env; every
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
    # role being in /var/lib/velstra/node.env, and a machine that has just been
    # unpacked has no seed — so enabling them would start agents pointing at no
    # cell, with no token, retrying for ever.
    if [ "$1" = "configure" ]; then
      systemctl daemon-reload >/dev/null 2>&1 || true
      if [ ! -f /var/lib/velstra/node.env ]; then
        echo ""
        echo "velstra-cloud is installed and nothing is running."
        echo "Say what this machine is for:"
        echo ""
        echo "    sudo velstra-cloud-node setup"
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
               velstra-cloud-nodeagent velstra-cloud-poolagent; do
        systemctl stop "$u" >/dev/null 2>&1 || true
      done
    fi
    PRERM
    chmod 0755 "$root/DEBIAN/prerm"

    cp ${../docs/install.md} "$root/usr/share/doc/velstra-cloud/install.md"

    dpkg-deb --root-owner-group --build "$root" "$out"
  ''
