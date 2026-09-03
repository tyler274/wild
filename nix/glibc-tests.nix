{
  stdenvNoCC,
  glibc,
  linuxHeaders,
  python3,
  bison,
  texinfo,
  gawk,
  perl,
  gettext,
  gnum4,
  flex,
  gcc,
  binutils-unwrapped-all-targets,
  gnumake,
  coreutils,
  findutils,
  diffutils,
  gnused,
  gnugrep,
  gnutar,
  gzip,
  xz,
  writeShellApplication,
}:
let
  # Read-only upstream source from the same nixpkgs as the rest of the shell.
  # Out-of-tree GNU configure writes into WILD_GLIBC_BUILD, not here.
  glibcSrc = stdenvNoCC.mkDerivation {
    name = "glibc-${glibc.version}-src";
    src = glibc.src;
    dontConfigure = true;
    dontBuild = true;
    dontFixup = true;
    preferLocalBuild = true;
    installPhase = ''
      runHook preInstall
      mkdir -p "$out"
      cp -a . "$out/"
      runHook postInstall
    '';
  };

  headers = "${linuxHeaders}/include";

  # Unwrapped GCC 15: the Nix wrapper injects -D_FORTIFY_SOURCE=3 after any
  # -U_FORTIFY_SOURCE, which breaks glibc's syslog.c always_inline aliases.
  gccUnwrapped = "${gcc.cc}/bin/gcc";
  gxxUnwrapped = "${gcc.cc}/bin/g++";

  # Glibc configure only accepts GNU ld / gold / LLD version strings. Wild's
  # --version first line is GNU ld compatible, but the oracle objects are still
  # linked with GNU ld so the relink tests have something to diff against.
  wild-build-glibc = writeShellApplication {
    name = "wild-build-glibc";
    runtimeInputs = [
      coreutils
      findutils
      diffutils
      gnused
      gnugrep
      gawk
      gnutar
      gzip
      xz
      gnumake
      gnum4
      python3
      bison
      texinfo
      perl
      gettext
      flex
      binutils-unwrapped-all-targets
    ];
    text = ''
      usage() {
        echo "usage: wild-build-glibc [--force]" >&2
        echo "  GNU-configure and build libc.so / ld.so for Wild's opt-in glibc tests." >&2
        echo "  WILD_GLIBC_TREE, WILD_GLIBC_BUILD, WILD_GLIBC_HEADERS must be set." >&2
      }

      force=0
      while [ "$#" -gt 0 ]; do
        case "$1" in
          --force) force=1 ;;
          -h|--help)
            usage
            exit 0
            ;;
          *)
            usage
            exit 1
            ;;
        esac
        shift
      done

      tree=''${WILD_GLIBC_TREE:?WILD_GLIBC_TREE is not set}
      build=''${WILD_GLIBC_BUILD:?WILD_GLIBC_BUILD is not set}
      hdrs=''${WILD_GLIBC_HEADERS:?WILD_GLIBC_HEADERS is not set}

      if [ ! -f "$tree/configure" ] || [ ! -f "$tree/Makerules" ]; then
        echo "WILD_GLIBC_TREE=$tree is not a glibc source tree" >&2
        exit 1
      fi

      mkdir -p "$build"

      if [ "$force" -eq 0 ] && [ -f "$build/libc.so" ] && [ -f "$build/elf/ld.so" ]; then
        echo "glibc already built in $build (pass --force to rebuild)"
        exit 0
      fi

      export NIX_HARDENING_ENABLE=""
      export NIX_CFLAGS_COMPILE="-U_FORTIFY_SOURCE"
      export CC="${gccUnwrapped}"
      export CXX="${gxxUnwrapped}"
      unset LD

      cc_stamp=$build/.wild-build-glibc-cc
      if [ "$force" -eq 0 ] && [ -f "$cc_stamp" ] && [ "$(cat "$cc_stamp")" != "$CC" ]; then
        echo "compiler changed; reconfiguring"
        force=1
      fi

      if [ "$force" -eq 1 ] || [ ! -f "$build/config.make" ]; then
        echo "configuring glibc in $build with $CC"
        ( cd "$build" && "$tree/configure" \
            --prefix=/usr \
            --disable-werror \
            --disable-profile \
            --disable-build-nscd \
            --disable-nscd \
            --with-headers="$hdrs" )
        printf '%s\n' "$CC" > "$cc_stamp"
      fi

      echo "building glibc in $build"
      make -C "$build" -j"$(nproc)"

      if [ ! -f "$build/libc.so" ] || [ ! -f "$build/elf/ld.so" ]; then
        echo "build finished but $build is missing libc.so / elf/ld.so" >&2
        exit 1
      fi

      echo "glibc ready. Relink with Wild:"
      echo "  cargo test -p wild-linker --no-default-features --features fork,zstd --test integration_tests -- glibc"
    '';
  };
in
{
  inherit glibcSrc wild-build-glibc;
  version = glibc.version;

  packages = [
    python3
    bison
    texinfo
    gawk
    perl
    gettext
    gnum4
    flex
    wild-build-glibc
  ];

  shellHook = ''
    if [ -z "''${WILD_GLIBC_TREE:-}" ]; then
      export WILD_GLIBC_TREE="${glibcSrc}"
    fi
    if [ -z "''${WILD_GLIBC_BUILD:-}" ]; then
      export WILD_GLIBC_BUILD="$PWD/target/glibc-gnu"
    fi
    export WILD_GLIBC_HEADERS="''${WILD_GLIBC_HEADERS:-${headers}}"
  '';
}
