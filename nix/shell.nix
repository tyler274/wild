{
  pkgs ? import <nixpkgs> { },
}:
let
  inherit (pkgs.callPackage ./wrappers.nix { }) gccWrapper gppWrapper clangWrapper;
  inherit (pkgs.llvmPackages) clang-tools lld;
  glibcTests = pkgs.callPackage ./glibc-tests.nix { };
in
pkgs.mkShell {
  packages = [
    pkgs.binutils-unwrapped-all-targets
    pkgs.cargo-chef
    clangWrapper
    clang-tools
    pkgs.taplo
    lld
    # llvm-config so Wild can auto-discover LLVMgold.so without --plugin.
    pkgs.llvmPackages.llvm.dev
    pkgs.glibc.out
    pkgs.glibc.static
    pkgs.rustup
    gccWrapper
    gppWrapper
  ]
  ++ glibcTests.packages;

  env.LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib ];

  # Unpack nixpkgs glibc, point WILD_GLIBC_* at it, and provide wild-build-glibc.
  # Override WILD_GLIBC_TREE / WILD_GLIBC_BUILD before `nix develop` to use another tree.
  inherit (glibcTests) shellHook;
}
