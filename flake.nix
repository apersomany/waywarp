{
  description = "Run Cloudflare WARP clients side by side, and exit in the region you choose";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      forSystems =
        systems: function:
        lib.genAttrs systems (
          system:
          function (
            import nixpkgs {
              inherit system;
              config.allowUnfreePredicate = package: lib.getName package == "cloudflare-warp-headless";
            }
          )
        );
      each = forSystems [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = each (pkgs: {
        default = pkgs.callPackage ./nix/package.nix {
          cloudflare-warp = pkgs.cloudflare-warp.override { headless = true; };
        };
        # A self-contained binary for release, run with the host's WARP, iproute2, and nftables.
        static = pkgs.pkgsStatic.callPackage ./nix/package.nix {
          runtime = [ ];
          doCheck = false;
        };
      });

      nixosModules.default = import ./nix/module.nix { inherit self; };

      checks = forSystems [ "x86_64-linux" ] (
        pkgs:
        let
          waywarp = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          checked = waywarp.overrideAttrs (previous: {
            nativeBuildInputs = previous.nativeBuildInputs ++ [ pkgs.clippy ];
            preCheck = (previous.preCheck or "") + ''
              cargo clippy --offline --release --all-targets \
                --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} -- -D warnings
            '';
          });
          source = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./src
              ./tests
            ];
          };
        in
        {
          package = checked;
          clippy = checked;
          installer = pkgs.callPackage ./nix/installer-test.nix { };
          vm = pkgs.testers.runNixOSTest (
            import ./nix/test.nix {
              inherit self;
              warp-stub = pkgs.callPackage ./nix/warp-stub.nix { };
            }
          );
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
