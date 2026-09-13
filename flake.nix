{
  description = "altdb development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config.android_sdk.accept_license = true;
            config.allowUnfree = true;
          };
          android = pkgs.androidenv.composeAndroidPackages {
            platformVersions = [ "30" ];
            abiVersions = [ "arm64-v8a" ];
            includeNDK = true;
            ndkVersions = [ "29.0.14206865" ];
            includeEmulator = false;
            includeSystemImages = false;
          };
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              android.androidsdk
              android-tools
              cargo-ndk
              cmake
              ninja
              python3
              rustup
              nodejs_24
              pkg-config
              git
              gh
            ];

            ANDROID_HOME = "${android.androidsdk}/libexec/android-sdk";
            ANDROID_SDK_ROOT = "${android.androidsdk}/libexec/android-sdk";
            CARGO_NDK_ANDROID_PLATFORM = "30";

            shellHook = ''
              export ANDROID_NDK_HOME="$(find -L "$ANDROID_HOME/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -n1)"
              export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"
              export PATH="$PATH:$HOME/.cargo/bin"
              echo "altdb dev shell"
              echo "  ANDROID_HOME=$ANDROID_HOME"
              echo "  ANDROID_NDK_HOME=$ANDROID_NDK_HOME"
              echo "  rust toolchain: $(awk -F'\"' '/^channel/ { print $2 }' rust-toolchain.toml 2>/dev/null || true)"
            '';
          };
        });
    };
}
