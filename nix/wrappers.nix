{
  lib,
  stdenv,
  makeBinaryWrapper,
  gcc,
  llvmPackages,
  binutils-unwrapped-all-targets,
}:
# https://github.com/NixOS/nixpkgs/blob/86539fa7196facc6f5c1d49394d1509a6f6c2916/pkgs/by-name/wi/wild/adapterTest.nix#L19-L59
# These wrappers are REQUIRED for the Wild test suite to pass
#
# Write a wrapper for GCC that passes -B to *unwrapped* binutils.
# This ensures that if -fuse-ld=bfd is used, gcc picks up unwrapped ld.bfd
# instead of the hardcoded wrapper search directory.
# We pass it last because apparently gcc likes picking ld from the *first* -B,
# which we want our wild target directory to be if passed.
let
  inherit (llvmPackages) clang;
  # Split `lib` output when present; otherwise the llvm package root.
  llvmLib = llvmPackages.libllvm.lib or llvmPackages.libllvm;
in
{
  gccWrapper = stdenv.mkDerivation {
    inherit (gcc) name;
    dontUnpack = true;
    dontConfigure = true;
    dontInstall = true;

    buildInputs = [ makeBinaryWrapper ];
    buildPhase = ''
      runHook preBuild

      makeWrapper ${lib.getExe gcc} $out/bin/gcc \
        --append-flag -B${binutils-unwrapped-all-targets}/bin

      runHook postBuild
    '';

  };

  gppWrapper = stdenv.mkDerivation {
    dontUnpack = true;
    dontConfigure = true;
    dontInstall = true;

    name = "g++-wrapped";
    buildInputs = [ makeBinaryWrapper ];
    buildPhase = ''
      runHook preBuild

      makeWrapper ${lib.getExe' gcc "g++"} $out/bin/g++ \
        --append-flag -B${binutils-unwrapped-all-targets}/bin

      runHook postBuild
    '';
  };

  # Nix's clang wrapper does not search LLVM's libdir for LLVMgold.so, so
  # `clang -flto -fuse-ld=bfd` (the LTO plugin probe, plus thin/fat LTO) fails
  # unless we add that directory with -B. Keep clang and the gold plugin on the
  # same llvmPackages set.
  clangWrapper = stdenv.mkDerivation {
    name = "clang-wrapped";
    dontUnpack = true;
    dontConfigure = true;
    dontInstall = true;

    buildInputs = [ makeBinaryWrapper ];
    buildPhase = ''
      runHook preBuild

      makeWrapper ${lib.getExe clang} $out/bin/clang \
        --append-flag -B${llvmLib}/lib \
        --append-flag -B${binutils-unwrapped-all-targets}/bin

      makeWrapper ${lib.getExe' clang "clang++"} $out/bin/clang++ \
        --append-flag -B${llvmLib}/lib \
        --append-flag -B${binutils-unwrapped-all-targets}/bin

      runHook postBuild
    '';
  };
}
