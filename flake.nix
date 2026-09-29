{
  description = "Run Cloudflare WARP clients side by side, and exit in the region you choose";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      each =
        function:
        lib.genAttrs systems (
          system:
          function (
            import nixpkgs {
              inherit system;
              config.allowUnfreePredicate = package: lib.getName package == "cloudflare-warp-headless";
            }
          )
        );
    in
    {
      packages = each (pkgs: {
        default = pkgs.callPackage ./nix/package.nix {
          cloudflare-warp = pkgs.cloudflare-warp.override { headless = true; };
        };
      });

      nixosModules.default = import ./nix/module.nix { inherit self; };

      checks = each (
        pkgs:
        let
          waywarp = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          source = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./src
            ];
          };
        in
        {
          package = waywarp;
          vm = pkgs.testers.runNixOSTest (
            import ./nix/test.nix {
              inherit self;
              warp-stub = pkgs.callPackage ./nix/warp-stub.nix { };
            }
          );
          clippy = waywarp.overrideAttrs (previous: {
            pname = "${previous.pname}-clippy";
            nativeBuildInputs = previous.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy --offline --all-targets -- -D warnings";
            doCheck = false;
            installPhase = "touch $out";
            postFixup = "";
          });
          format =
            pkgs.runCommand "waywarp-format"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                  pkgs.nixfmt
                ];
              }
              ''
                cp -r ${source} source && chmod -R +w source && (cd source && cargo fmt --check)
                nixfmt --check ${./flake.nix} ${./nix}/*.nix
                touch $out
              '';
        }
      );

      devShells = each (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
          ]
          ++ self.packages.${pkgs.stdenv.hostPlatform.system}.default.runtime;
        };
      });

      formatter = each (pkgs: pkgs.nixfmt);
    };
}
