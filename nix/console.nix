# The React console, built offline from its own lockfile.
#
# `importNpmLock` rather than `buildNpmPackage`, for the reason the Rust half
# of this flake uses `cargoLock.lockFile` rather than `cargoHash`: every one of
# the lockfile's entries carries its own `integrity`, so nix can turn each one
# into a fixed-output fetch and there is no second hash for somebody to forget
# to update. A `npmDepsHash` is a number that must be recomputed by hand, with
# network, every time a dependency moves — and the failure mode when it is not
# is a build that fetches the wrong tree and says nothing.
#
# The output is the Vite build's `dist/` and nothing else: an `index.html`, the
# hashed files under `assets/`, and two svgs. It is shipped as files rather
# than compiled into the API binary, because `dist/` is not in git and every
# `cargo build` in this repository — CI's own gate among them — would otherwise
# need node and a network to compile a Rust crate.
{
  pkgs,
  lib,
  version,
}:

pkgs.stdenv.mkDerivation (finalAttrs: {
  pname = "velstra-cloud-console-react";
  inherit version;

  # Only the console's own tree. The rest of the repository changing must not
  # rebuild 511 npm packages.
  src = lib.cleanSourceWith {
    src = ../velstra-cloud-console-react;
    filter =
      path: type:
      let
        name = baseNameOf path;
      in
      !(builtins.elem name [
        "node_modules"
        "dist"
      ]);
  };

  nativeBuildInputs = [
    pkgs.nodejs
    pkgs.importNpmLock.npmConfigHook
  ];

  npmDeps = pkgs.importNpmLock {
    npmRoot = ../velstra-cloud-console-react;
  };

  buildPhase = ''
    runHook preBuild
    npm run build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    # The tree as the API will serve it: `$out/index.html`, `$out/assets/…`.
    # No extra directory level, so the path a request asks for is the path on
    # disk with one prefix stripped.
    mkdir -p "$out"
    cp -r dist/. "$out/"
    test -f "$out/index.html"
    runHook postInstall
  '';

  # Nothing to strip or patch: it is JavaScript, CSS and two svgs.
  dontFixup = true;

  meta = {
    description = "The Velstra Cloud operator console, built";
    platforms = lib.platforms.all;
  };
})
