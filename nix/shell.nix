{
  pkgs ? import <nixpkgs> { },
}:
let
  inherit (pkgs) lib;
  # rustup nightly (1.98+) ships LLVM 22. Use the matching clang / LLVMgold /
  # lld / lldb so rustc `-Clinker-plugin-lto` and Clang ThinLTO share a major.
  llvmPkgs = pkgs.llvmPackages_22;
  inherit (pkgs.callPackage ./wrappers.nix { llvmPackages = llvmPkgs; })
    gccWrapper
    gppWrapper
    clangWrapper
    ;
  inherit (llvmPkgs) clang-tools lld lldb;
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
    lldb
    # llvm-config so Wild can auto-discover LLVMgold.so without --plugin.
    llvmPkgs.llvm.dev
    pkgs.glibc.out
    pkgs.glibc.static
    pkgs.rustup
    gccWrapper
    gppWrapper

    # Four-way diffs, dynamic mimalloc, and pkg-config for `-lmimalloc`.
    pkgs.mold
    pkgs.mimalloc
    pkgs.pkg-config

    # Debug / inspect ELF output and allocator behaviour.
    pkgs.gdb
    pkgs.elfutils
    pkgs.valgrind
    pkgs.strace
    pkgs.file

    # BENCHMARKING.md (`hyperfine`) and sampling profiles of Wild itself.
    pkgs.hyperfine
    pkgs.samply

    # Pack `vmlinux` object tarballs; kernel rebuilds for LTO slices.
    pkgs.zstd
    pkgs.bc
    pkgs.rsync
    pkgs.openssl
    pkgs.ncurses
    pkgs.zlib
    pkgs.pahole
    pkgs.linuxHeaders

    # Userspace package trees (python / gcc / llvm) when those env vars are set.
    pkgs.cmake
    pkgs.ninja
    pkgs.git
  ]
  ++ glibcTests.packages;

  env.LD_LIBRARY_PATH = lib.makeLibraryPath [
    pkgs.stdenv.cc.cc.lib
    pkgs.mimalloc
  ];

  # Unpack nixpkgs glibc, point WILD_GLIBC_* at it, and provide wild-build-glibc.
  # Override WILD_GLIBC_TREE / WILD_GLIBC_BUILD before `nix develop` to use another tree.
  shellHook = glibcTests.shellHook + ''
    if ! command -v cargo-kani >/dev/null 2>&1; then
      echo "Kani is not in this shell (not packaged in nixpkgs)."
      echo "  cargo install --locked kani-verifier && cargo kani setup"
      echo "  ./scripts/kani.sh   # no-ops locally if cargo-kani is missing"
    fi
  '';
}
