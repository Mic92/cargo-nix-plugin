{ stdenv }:
crateName: metadata: buildTests:
if !buildTests then
  ''
    runHook preInstall
    # always create $out even if we do not have binaries. We are detecting binary targets during compilation, if those are missing there is no way to only have $lib
    mkdir $out
    if [[ -s target/env ]]; then
      mkdir -p $lib
      cp target/env $lib/env
    fi
    if [[ -s target/link.final ]]; then
      mkdir -p $lib/lib
      cp target/link.final $lib/lib/link
    fi
    if [[ "$(ls -A target/lib)" ]]; then
      mkdir -p $lib/lib
      cp -r target/lib/* $lib/lib #*/
      for library in $lib/lib/*.so $lib/lib/*.dylib; do #*/
        ln -s $library $(echo $library | sed -e "s/-${metadata}//")
      done

      # [lib] name in Cargo.toml can differ from the crate name
      # (rustls-webpki → libwebpki.rlib, utf-8 → libutf8.rlib).
      # We only learn this at build time — the sparse index JSON has
      # no lib.name field. But dependents bake --extern NAME=PATH at
      # eval time using the crate name, so both the NAME and the
      # PATH are wrong.
      #
      # Two-part fix:
      #   1. Symlink the wrong filename to the right one (path fixup)
      #   2. Record the real lib name in a marker file so the
      #      dependent's read-crate-info can rewrite --extern NAME
      #      (same mechanism as proc-macro.marker)
      evalLibName=$(echo '${crateName}' | tr - _)
      if [[ -n "''${CRATE_NAME:-}" && "$CRATE_NAME" != "$evalLibName" ]]; then
        echo "$CRATE_NAME" > $lib/lib/lib-name
        for built in $lib/lib/lib$CRATE_NAME-${metadata}.*; do
          [[ -e $built ]] || continue
          ln -sf "$(basename "$built")" "$lib/lib/lib$evalLibName-${metadata}.''${built##*.}"
        done
      fi
    fi
    if [[ "$(ls -A target/build)" ]]; then # */
      mkdir -p $lib/lib
      cp -r target/build/* $lib/lib # */
    fi
    if [[ -d target/bin ]]; then
      if [[ "$(ls -A target/bin)" ]]; then
        mkdir -p $out/bin
        cp -rP target/bin/* $out/bin # */
      fi
    fi
    runHook postInstall
  ''
else
  # for tests we just put them all in the output. No execution.
  ''
    runHook preInstall

    mkdir -p $out/tests
    if [ -e target/bin ]; then
      find target/bin/ -type f -executable -exec cp {} $out/tests \;
    fi
    if [ -e target/lib ]; then
      find target/lib/ -type f \! -name '*.rlib' \
        -a \! -name '*${stdenv.hostPlatform.extensions.library}' \
        -a \! -name '*.d' \
        -executable \
        -print0 | xargs --no-run-if-empty --null install --target $out/tests;
    fi

    runHook postInstall
  ''
