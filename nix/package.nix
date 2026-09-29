{
  lib,
  rustPlatform,
  makeWrapper,
  cloudflare-warp,
  iproute2,
  nftables,
}:
let
  manifest = (lib.importTOML ../Cargo.toml).package;
  # Helpers the supervisor runs, including after `waywarp up` exits.
  runtime = [
    cloudflare-warp
    iproute2
    nftables
  ];
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
  nativeBuildInputs = [ makeWrapper ];
  postFixup = ''
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
