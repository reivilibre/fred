{ pkgs, lib, config, inputs, ... }:

{
  cachix.enable = false;

  languages.rust = {
    enable = true;
  };

  packages = with pkgs; [
    clang
  ];

  # See full reference at https://devenv.sh/reference/options/
}
