{
  description = "piano — a sampled grand piano played on the computer keyboard (gpui)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;

        # The Accurate-Salamander Grand Piano v6.2a (Chisato Yamauchi, CC-BY 3.0), a remaster
        # of Alexander Holm's Salamander Grand Piano v3 (a Yamaha C5, CC-BY 3.0): 48 kHz/24-bit
        # FLAC, 16 velocity layers. The app reads it straight from the zip (784 MB).
        samples = pkgs.fetchurl {
          url = "https://archive.org/download/httpswww.ir.isas.jaxa.jpcyamauchaccuratesalamander/AccurateSalamanderGrandPianoV6.2a_48khz24bit_flac.zip";
          sha256 = "125k00w3mzvjm41wd4027fgfb24yz73ianrx2xl6lw8lqzz81y10";
        };

        # pipewire-rs generates bindings with bindgen, so it needs libclang + headers.
        nativeBuildInputs = with pkgs; [ pkg-config rustPlatform.bindgenHook ];
        # libxkbcommon: gpui links it directly.
        buildInputs = with pkgs; [ pipewire libxkbcommon ];

        # gpui loads these at runtime with dlopen.
        gpuLibs = with pkgs; [ vulkan-loader wayland libxkbcommon ];
        # Use nixpkgs' Mesa Vulkan drivers: the host's are built against a different libc,
        # which is what usually breaks Nix GPU apps on non-NixOS systems.
        vkDriverFiles = lib.concatMapStringsSep ":" (f: "${pkgs.mesa}/share/vulkan/icd.d/${f}") [
          "intel_icd.x86_64.json"
          "radeon_icd.x86_64.json"
          "nouveau_icd.x86_64.json"
          "lvp_icd.x86_64.json"
        ];

        piano = pkgs.rustPlatform.buildRustPackage {
          pname = "piano";
          version = "0.1.0";
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./packaging ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          inherit buildInputs;
          nativeBuildInputs = nativeBuildInputs ++ [ pkgs.makeWrapper ];
          # The tests that need the real samples run against them here too.
          PIANO_SAMPLES = samples;
          # GPU libraries go in the rpath rather than LD_LIBRARY_PATH so they don't leak into
          # child processes. (postFixup, because fixup shrinks the rpath to what's linked,
          # dropping dlopen'd libs.)
          postFixup = ''
            patchelf --add-rpath ${lib.makeLibraryPath gpuLibs} $out/bin/piano
            wrapProgram $out/bin/piano \
              --set-default PIANO_SAMPLES ${samples} \
              --set-default VK_DRIVER_FILES ${vkDriverFiles}

            install -Dm644 packaging/piano.desktop $out/share/applications/piano.desktop
            substituteInPlace $out/share/applications/piano.desktop --replace-fail @piano@ $out/bin/piano
            install -Dm644 packaging/piano.svg $out/share/icons/hicolor/scalable/apps/piano.svg
          '';
          meta.mainProgram = "piano";
        };
      in
      {
        packages.default = piano;
        packages.samples = samples;
        apps.default = flake-utils.lib.mkApp { drv = piano; };

        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          nativeBuildInputs = nativeBuildInputs
            ++ (with pkgs; [ cargo rustc rust-analyzer clippy rustfmt ]);
          LD_LIBRARY_PATH = lib.makeLibraryPath gpuLibs;
          VK_DRIVER_FILES = vkDriverFiles;
          PIANO_SAMPLES = samples;
        };
      });
}
