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
  # Shared libgcc lives in gcc's lib output, which unwrapped `gcc -print-file-name`
  # does not search. Glibc tests link `-lgcc_s`.
  libgccLib = "${gcc.cc.lib}/lib";

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
      export LIBRARY_PATH="${libgccLib}''${LIBRARY_PATH:+:$LIBRARY_PATH}"
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
      echo "Then: wild-glibc-check"
    '';
  };

  # Swap Wild-linked glibc DSOs into the GNU build and run a `make test`
  # subset. Always restores the GNU oracles so relink diffs stay valid.
  wild-glibc-check = writeShellApplication {
    name = "wild-glibc-check";
    runtimeInputs = [
      coreutils
      gnumake
      gnused
      gnugrep
      python3
    ];
    text = ''
      set -euo pipefail
      build=''${WILD_GLIBC_BUILD:?WILD_GLIBC_BUILD is not set}
      repo=''${WILD_REPO:-$PWD}
      artifacts=$repo/wild/tests/build/elf/x86_64
      libc_wild=$artifacts/glibc-libc/libc.so.wild
      ldso_wild=$artifacts/glibc-ldso/ld.so.wild

      if [ ! -f "$libc_wild" ] || [ ! -f "$ldso_wild" ]; then
        echo "missing Wild relink artifacts. Run:" >&2
        echo "  cargo test -p wild-linker --no-default-features --features fork,zstd --test integration_tests -- glibc" >&2
        exit 1
      fi

      tests=(
        elf/tst-tls1
        elf/tst-tls9
        elf/tst-tls13
        elf/tst-tlsalign
        elf/tst-gnu2-tls1
        elf/tst-array1
        elf/tst-array2
        elf/tst-array3
        elf/tst-array4
        elf/tst-array5
        elf/tst-main1
        elf/tst-initorder
        elf/tst-initorder2
        elf/tst-align
        elf/tst-auxv
        elf/tst-dlopen-self
        elf/ifuncmain1
        elf/tst-relr
        elf/tst-relr2
        malloc/tst-malloc
        malloc/tst-malloc-usable
        stdlib/tst-strtol
        stdlib/tst-strtod
        string/test-strcpy
        string/test-strlen
        string/test-memcpy
        string/test-memcmp
        nptl/tst-mutex1
        nptl/tst-basic1
        nptl/tst-stack1
        stdio-common/tst-printf
        stdio-common/tst-sprintf
        math/basic-test
        rt/tst-timer
        elf/reldep
        elf/next
        elf/nodelete
        elf/tst-audit1
        elf/ifuncmain5
        elf/ifuncmain9
        nptl/tst-cond1
        nptl/tst-once1
        nptl/tst-join1
        nptl/tst-exit1
        posix/bug-getopt1
        posix/bug-regex1
        io/test-stat
        io/test-utime
        time/tst-clock
        time/tst-strptime
        signal/tst-signal
        signal/tst-raise
        dirent/opendir-tst1
        misc/tst-dirname
        misc/tst-tsearch
        nss/tst-getpw
        nss/test-netdb
        resolv/tst-aton
        resolv/tst-inet_ntop
        setjmp/tst-setjmp
        stdlib/test-canon
        string/test-strcmp
        malloc/tst-mallocfork
      )

      restore() {
        local dest
        for dest in \
          "$build/libc.so" \
          "$build/elf/ld.so" \
          "$build/math/libm.so" \
          "$build/resolv/libresolv.so" \
          "$build/mathvec/libmvec.so" \
          "$build/nptl/libpthread.so" \
          "$build/dlfcn/libdl.so" \
          "$build/rt/librt.so"
        do
          if [ -f "$dest.gnu-oracle" ]; then
            cp -a "$dest.gnu-oracle" "$dest"
          fi
        done
      }
      trap restore EXIT

      swap_dso() {
        local wild=$1 dest=$2 soname=$3
        if [ ! -f "$wild" ]; then
          return 0
        fi
        if [ ! -f "$dest.gnu-oracle" ]; then
          cp -a "$dest" "$dest.gnu-oracle"
        fi
        cp -a "$wild" "$dest"
        ln -sfn "$(basename "$dest")" "$(dirname "$dest")/$soname"
      }

      swap_dso "$libc_wild" "$build/libc.so" libc.so.6
      swap_dso "$ldso_wild" "$build/elf/ld.so" ld-linux-x86-64.so.2
      swap_dso "$artifacts/glibc-libm/libm.so.wild" "$build/math/libm.so" libm.so.6
      swap_dso "$artifacts/glibc-libresolv/libresolv.so.wild" "$build/resolv/libresolv.so" libresolv.so.2
      swap_dso "$artifacts/glibc-libmvec/libmvec.so.wild" "$build/mathvec/libmvec.so" libmvec.so.1
      swap_dso "$artifacts/glibc-libpthread/libpthread.so.wild" "$build/nptl/libpthread.so" libpthread.so.0
      swap_dso "$artifacts/glibc-libdl/libdl.so.wild" "$build/dlfcn/libdl.so" libdl.so.2
      swap_dso "$artifacts/glibc-librt/librt.so.wild" "$build/rt/librt.so" librt.so.1

      export LIBRARY_PATH="${libgccLib}''${LIBRARY_PATH:+:$LIBRARY_PATH}"
      export NIX_HARDENING_ENABLE=""
      export CC="${gccUnwrapped}"
      export CXX="${gxxUnwrapped}"

      pass=0
      fail=0
      failed=()
      for t in "''${tests[@]}"; do
        rm -f "$build/$t.out" "$build/$t.test-result"
        if make -C "$build" test "t=$t"; then
          pass=$((pass + 1))
        else
          fail=$((fail + 1))
          failed+=("$t")
        fi
      done

      echo "wild-glibc-check: $pass passed, $fail failed"
      if [ "$fail" -ne 0 ]; then
        echo "failed: ''${failed[*]}" >&2
        exit 1
      fi
    '';
  };
in
{
  inherit glibcSrc wild-build-glibc wild-glibc-check;
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
    wild-glibc-check
  ];

  shellHook = ''
    if [ -z "''${WILD_GLIBC_TREE:-}" ]; then
      export WILD_GLIBC_TREE="${glibcSrc}"
    fi
    if [ -z "''${WILD_GLIBC_BUILD:-}" ]; then
      export WILD_GLIBC_BUILD="$PWD/target/glibc-gnu"
    fi
    export WILD_GLIBC_HEADERS="''${WILD_GLIBC_HEADERS:-${headers}}"
    # Unwrapped GCC cannot find libgcc_s; glibc tests need it to link.
    export LIBRARY_PATH="${libgccLib}''${LIBRARY_PATH:+:$LIBRARY_PATH}"
  '';
}
