{
  lib,
  rustPlatform,
  makeWrapper,
  cloudflare-warp ? null,
  iproute2,
  nftables,
  # Helpers the supervisor runs, including after `waywarp up` exits; empty leaves them to PATH.
  runtime ? [
    cloudflare-warp
    iproute2
    nftables
  ],
}:
let
  manifest = (lib.importTOML ../Cargo.toml).package;
in
rustPlatform.buildRustPackage {
  pname = manifest.name;
  inherit (manifest) version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = lib.optional (runtime != [ ]) makeWrapper;
  postFixup = lib.optionalString (runtime != [ ]) ''
    wrapProgram $out/bin/waywarp --prefix PATH : ${lib.makeBinPath runtime}
  '';
  passthru = { inherit runtime; };
  meta = {
    inherit (manifest) description;
    license = lib.licenses.mit;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    mainProgram = "waywarp";
  };
}
