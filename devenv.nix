{ pkgs, lib, config, inputs, ... }:

{
  cachix.enable = false;

  languages.rust = {
    enable = true;
  };

  packages = with pkgs; [
    clang
    cmake
    protobuf

    # compilation dependencies (speculated)
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    xorg.libX11
    xorg.libxcb
    libxkbcommon

    alsa-lib


    # for runtime?
    vulkan-loader

    mold
  ];

  env = {
    PROTOC = "${pkgs.protobuf}/bin/protoc";

    LD_LIBRARY_PATH="${pkgs.vulkan-loader}/lib";
  };

  # See full reference at https://devenv.sh/reference/options/
}
