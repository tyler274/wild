{
  pkgs ? import <nixpkgs> { },
}:
pkgs.mkShell {
  nativeBuildInputs = [
    (pkgs.writeShellApplication {
      name = "gcc";
      text = ''${pkgs.lib.getExe pkgs.gcc} "$@" -B${pkgs.binutils-unwrapped-all-targets}/bin '';
    })
    (pkgs.writeShellApplication {
      name = "g++";
      text = ''${pkgs.lib.getExe' pkgs.gcc "g++"} "$@" -B${pkgs.binutils-unwrapped-all-targets}/bin '';
    })
    pkgs.binutils-unwrapped-all-targets
    pkgs.cargo-chef
    (pkgs.writeShellApplication {
      name = "clang";
      text = ''${pkgs.lib.getExe pkgs.clang} "$@" -B${
        pkgs.llvmPackages.libllvm.lib or pkgs.llvmPackages.libllvm
      }/lib -B${pkgs.binutils-unwrapped-all-targets}/bin '';
    })
    (pkgs.writeShellApplication {
      name = "clang++";
      text = ''${pkgs.lib.getExe' pkgs.clang "clang++"} "$@" -B${
        pkgs.llvmPackages.libllvm.lib or pkgs.llvmPackages.libllvm
      }/lib -B${pkgs.binutils-unwrapped-all-targets}/bin '';
    })
    pkgs.llvmPackages.clang-tools
    pkgs.llvmPackages.lld
    pkgs.llvmPackages.llvm.dev
    pkgs.glibc.out
    pkgs.glibc.static
    pkgs.rustup
  ];

  LD_LIBRARY_PATH = "${pkgs.stdenv.cc.cc.lib}/lib";
}
