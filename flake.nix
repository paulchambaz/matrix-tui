{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};

      buildPkgs = with pkgs; [
        cmake
        perl
        pkg-config
        scdoc
      ];

      devPkgs = with pkgs; [
        cargo
        cargo-watch
        rustfmt
        clippy
        rustc
        just
        cargo-tarpaulin
      ];
    in {
      packages.default = pkgs.rustPlatform.buildRustPackage {
        pname = "matrix-tui";
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = buildPkgs;
        dontUseCmakeConfigure = true;

        postInstall = ''
          mkdir -p $out/share/man/man1
          SOURCE_DATE_EPOCH=${toString self.lastModified} scdoc < mx.1.scd > $out/share/man/man1/mx.1
        '';

        meta = {
          description = "Keyboard-driven terminal client for Matrix";
          homepage = "https://github.com/paulchambaz/matrix-tui";
          license = pkgs.lib.licenses.gpl3Only;
          mainProgram = "mx";
        };
      };

      devShells.default = pkgs.mkShell {
        nativeBuildInputs = buildPkgs;
        buildInputs = devPkgs;
      };
    });
}
