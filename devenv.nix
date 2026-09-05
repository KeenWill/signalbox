{ pkgs, config, ... }:

let
  # Nix interpolates these values into shell text verbatim, and a double-quoted
  # shell word still performs parameter and command substitution — so a
  # checkout under a parent named `work$USER` or one holding a backtick would
  # be rewritten at runtime, not merely word-split. Every Nix-derived value
  # that reaches a command below is emitted as a single-quoted shell word
  # instead, which suppresses splitting and substitution alike. Nix-derived
  # values only: `$DEVENV_ROOT`, `$HOME`, and the shell variables the scripts
  # define are ordinary shell text and stay as they are.
  shellArg = pkgs.lib.escapeShellArg;

  # Dev-instance identity. The database name, role, and password are
  # local-only and loopback-bound; the password mirrors the convention the
  # committed integration suites already use for their disposable containers
  # (`signalbox-test-only`). No real credential material lives in this file
  # or anywhere else in the repository.
  devDatabase = "signalbox_dev";
  devRole = "signalbox_dev";
  devPassword = "signalbox-dev-only";

  # Base port for the dev cluster. Deliberately far from 5432 so a system or
  # container PostgreSQL keeps working alongside it; devenv allocates upward
  # from here if the port is busy, and the URL below uses the resolved value.
  devPortBase = 54341;
  devPort = config.processes.postgres.ports.main.value;

  # All mutable dev-instance state lives under one directory so `devenv up`
  # is reproducible and wiping is a single `rm -rf`.
  stateRoot = config.env.DEVENV_STATE + "/dev-instance";
  tlsRoot = "${stateRoot}/tls";
  authorityCertificate = "${tlsRoot}/dev-ca.crt";
  authorityKey = "${tlsRoot}/dev-ca.key";
  serverCertificate = "${tlsRoot}/server.crt";
  serverKey = "${tlsRoot}/server.key";

  # Generation staging. It sits inside `tlsRoot` so it is always on the same
  # filesystem as the published artifacts, which is what makes the `mv` below
  # a rename(2) rather than a copy — and therefore atomic.
  tlsStaging = "${tlsRoot}/.staging";

  # A process-scoped home for the daemon. `production_connection_options`
  # refuses to parse when `~/.pgpass` exists under the process home, and it
  # resolves that home with `std::env::home_dir()`, which on Unix returns
  # `$HOME` whenever it is set and nonempty. Pointing HOME at a devenv-owned
  # directory therefore makes the refusal deterministic instead of dependent
  # on whatever the developer keeps in their real home directory.
  daemonHome = "${stateRoot}/home";

  # Everything the dev instance publishes on the daemon's own PATH, and
  # nothing else. It holds one symlink, to the built MCP bridge, so a
  # developer who uncomments the example's `[claude_cli]` block can name that
  # program instead of deriving a path into `target/` by hand. Pointing PATH
  # at the build directory itself would publish every workspace binary.
  daemonBinDirectory = "${stateRoot}/bin";

  daemonConfigFile = "${stateRoot}/signalboxd.toml";
  daemonRuntimeConfigFile = "${daemonSocketDirectory}/signalboxd.toml";
  daemonTemplateConfigFile = "${stateRoot}/session-templates.toml";
  daemonBraveApiKeyFile = "${stateRoot}/brave-api-key";
  daemonBlobStagingDirectory = "${stateRoot}/blob-staging";
  daemonBlobPrimaryDirectory = "${stateRoot}/blobs-primary";
  blobStorageSupported = pkgs.stdenv.isLinux;

  # The daemon validates the socket's parent directory before binding: it must
  # be owned by the effective user and be mode exactly 0700, and no ancestor
  # may be group- or other-writable unless it is sticky and holds an
  # owner-matched child. It creates neither the directory nor those
  # permissions, so the process does that itself below. This lives under
  # DEVENV_RUNTIME rather than DEVENV_STATE because a Unix socket path is
  # limited to about 104 bytes and DEVENV_STATE sits inside the repository
  # checkout, whose path can be arbitrarily deep. On macOS the runtime
  # directory resolves under the sticky `/private/tmp`, which the ancestor
  # rule accepts for a directory the developer owns.
  daemonSocketDirectory = "${config.env.DEVENV_RUNTIME}/signalbox";
  daemonSocketPath = "${daemonSocketDirectory}/signalboxd.sock";

  # The certificate path below is derived from the checkout, so it can carry
  # characters a URL query reserves. SQLx parses the URL and percent-decodes
  # query values, so encode them here rather than trusting the path's shape:
  # a raw `&` would start another parameter, `#` would truncate the query, and
  # an accidental `%xx` would decode to something else entirely. `%` is
  # replaced first, and `builtins.replaceStrings` does not rescan what it
  # emits, so the escapes introduced here are not re-encoded.
  #
  # Tab, line feed, and carriage return are in the list for a different
  # reason, and they complete it. `url` 2.5.8 — the parser behind
  # `PgConnectOptions::from_str` — drops exactly those three anywhere in the
  # URL rather than encoding them (`Input::next` skips `ascii_tab_or_new_line`
  # unconditionally), so a path holding one would silently lose it. Every
  # other C0 control, DEL, and non-ASCII byte is in the parser's own `CONTROLS`
  # percent-encode set for the query component, so the parser encodes those
  # itself and `query_pairs` decodes them back unchanged.
  percentEncodeQueryValue = builtins.replaceStrings
    [ "%" "#" "&" "?" "+" "=" " " "\t" "\n" "\r" "\"" "<" ">" "\\" "^" "`" "{" "|" "}" ]
    [
      "%25"
      "%23"
      "%26"
      "%3F"
      "%2B"
      "%3D"
      "%20"
      "%09"
      "%0A"
      "%0D"
      "%22"
      "%3C"
      "%3E"
      "%5C"
      "%5E"
      "%60"
      "%7B"
      "%7C"
      "%7D"
    ];

  # Every connection parameter is stated in the URL, including the user name
  # and host the daemon refuses to let SQLx take from the process account or
  # the host filesystem. `sslrootcert` names the dev certificate authority
  # generated below: the daemon forces `sslmode=verify-full` unconditionally,
  # and SQLx adds this root to the platform trust store rather than replacing
  # it, so a loopback dev cluster can satisfy full verification without
  # touching SSL_CERT_FILE or SSL_CERT_DIR (which the daemon also refuses).
  # The role, password, and database name are literals defined above, so only
  # the checkout-derived certificate path needs encoding.
  databaseUrl =
    "postgres://${devRole}:${devPassword}@localhost:${toString devPort}"
    + "/${devDatabase}?sslrootcert=${percentEncodeQueryValue authorityCertificate}";

  # The exact ambient channels `production_connection_options` refuses. The
  # PostgreSQL service exports PGHOST, PGPORT, and PGDATA into the shared
  # environment for its own tooling, two of which are on this list, so the
  # daemon cannot simply inherit the devenv environment.
  refusedVariables = [
    "PGAPPNAME"
    "PGDATABASE"
    "PGHOST"
    "PGHOSTADDR"
    "PGOPTIONS"
    "PGPASSFILE"
    "PGPASSWORD"
    "PGPORT"
    "PGSSLCERT"
    "PGSSLKEY"
    "PGSSLMODE"
    "PGSSLROOTCERT"
    "PGUSER"
    "SSL_CERT_DIR"
    "SSL_CERT_FILE"
  ];
  scrub = builtins.concatStringsSep " " (map (name: "-u ${name}") refusedVariables);

  openssl = "${pkgs.openssl}/bin/openssl";
  # The client helper preserves its caller's directory, where rustup would not
  # discover the workspace toolchain file.
  workspaceRustToolchain =
    (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
  tomlPython = pkgs.python3.withPackages (pythonPackages: [ pythonPackages.tomlkit ]);
in

{
  # Developer environment for repository tooling. Enter with `devenv shell`,
  # or run a single command with `devenv shell -- <command> <args>`; direnv
  # users get it automatically after a one-time `direnv allow` (see .envrc).
  #
  # The Markdown toolchain is installed from the same fully frozen pin file
  # CI uses (tooling/requirements-mdformat.txt), so local mdformat output is
  # byte-identical to CI. Never run a system or Homebrew mdformat against
  # this repository: without the GFM plugin it silently corrupts GFM tables
  # under .mdformat.toml's wrap=80.

  # Shared compiler cache: sccache wraps rustc for cargo invocations inside
  # this environment, so dependency compilation is cached once per machine
  # and reused across checkouts and worktrees. The cache lives in sccache's
  # per-user default location (override with SCCACHE_DIR); workspace crates
  # keep incremental compilation and are passed through uncached. CI never
  # enters this environment — its caching is configured in
  # .github/workflows/rust.yml.
  packages = [
    pkgs.cargo-public-api
    pkgs.sccache
  ];
  env.RUSTC_WRAPPER = "sccache";

  # The terminal client and the managed daemon share one socket without the
  # developer having to derive its path from DEVENV_RUNTIME.
  env.SIGNALBOX_SOCKET_PATH = daemonSocketPath;

  # Build from the workspace so a caller project's Cargo configuration is not
  # discovered. The build and resolution run inside a command substitution, an
  # implicit subshell, so its `cd` never leaks: the exec below still runs from
  # the caller's original directory, and client arguments containing relative
  # paths keep their expected base. The executable path comes from Cargo's own
  # build output rather than an assumed target/debug: a developer with
  # `build.target` in their Cargo configuration, or `CARGO_BUILD_TARGET` in the
  # environment, gets the binary under target/<triple>/debug instead, and a
  # hard-coded target/debug path would then either fail outright or silently
  # exec a stale binary left over from an earlier build. Resolution and the
  # matching refusal for a target this host cannot run follow the same
  # approach as the dev-instance daemon launcher below, factored into
  # tooling/resolve-cargo-bin.sh so it has its own regression coverage
  # (tooling/test_resolve_cargo_bin.py).
  #
  # Known limitation, recorded rather than handled: the final `exec` below
  # runs the resolved binary directly, not through `cargo run`, so it does not
  # carry the runtime library search path `cargo run` adds. An ambient
  # `-C prefer-dynamic` (from `RUSTFLAGS` or inherited Cargo configuration)
  # then builds an executable this exec cannot start; it fails loudly with
  # exit 127 naming the missing shared object, rather than the silent
  # stale-binary failure this change fixes, so it is left as a known cost of
  # an unusual global setting instead of reproducing `cargo run`'s
  # environment here.
  scripts.signalbox = {
    description = "Run the Signalbox terminal client from the working tree.";
    exec = ''
      executable="$(
        cd "$DEVENV_ROOT" || exit $?
        env RUSTUP_TOOLCHAIN=${shellArg workspaceRustToolchain} \
          "$DEVENV_ROOT/tooling/resolve-cargo-bin.sh" \
            "$DEVENV_ROOT/Cargo.toml" "$DEVENV_ROOT/target" \
            signalbox-client signalbox
      )" || exit $?
      exec "$executable" "$@"
    '';
  };

  scripts.signalbox-materialize-config = {
    description = "Resolve the daemon exec-supervisor placeholder in a config copy.";
    exec = ''
      if [ "$#" -ne 3 ]; then
        echo "usage: signalbox-materialize-config <source> <destination> <exec-supervisor>" >&2
        exit 2
      fi
      ${tomlPython}/bin/python3 -c ${shellArg ''
        import sys
        from pathlib import Path

        import tomlkit

        source_path = Path(sys.argv[1])
        destination_path = Path(sys.argv[2])
        supervisor = sys.argv[3]
        content = source_path.read_text()
        document = tomlkit.parse(content)
        placeholder = "/usr/local/bin/signalbox-exec-supervisor"
        daemon_tools = document.get("daemon_tools")
        if daemon_tools is None:
            daemon_tools = tomlkit.table()
            daemon_tools["exec_supervisor_executable"] = supervisor
            document["daemon_tools"] = daemon_tools
            content = tomlkit.dumps(document)
        elif (
            isinstance(daemon_tools, dict)
            and daemon_tools.get("exec_supervisor_executable") == placeholder
        ):
            daemon_tools["exec_supervisor_executable"] = supervisor
            content = tomlkit.dumps(document)
        destination_path.write_text(content)
      ''} "$1" "$2" "$3"
    '';
  };

  languages.python = {
    enable = true;
    venv = {
      enable = true;
      requirements = ./tooling/requirements-mdformat.txt;
    };
  };

  # The dev instance: one PostgreSQL cluster and one signalboxd, started
  # together by `devenv up` under devenv's native process manager. This is a
  # development convenience only. The integration and end-to-end suites keep
  # provisioning their own disposable databases through testcontainers and
  # are not affected by anything below.
  services.postgres = {
    enable = true;

    # The same major version the integration suites pin for their container
    # image (`18.4-alpine3.23` in crates/persistence/tests).
    package = pkgs.postgresql_18;

    # TCP on loopback only. The daemon states `localhost` in its URL, and
    # the dev server certificate carries exactly that name, so full
    # verification succeeds without weakening the connection path.
    listen_addresses = "127.0.0.1";
    port = devPortBase;

    initialDatabases = [{
      name = devDatabase;
      user = devRole;
      pass = devPassword;
    }];

    settings = {
      ssl = true;
      ssl_cert_file = serverCertificate;
      ssl_key_file = serverKey;
    };

    # Unix-socket connections stay on trust so the service's own
    # initialisation tooling works; every TCP connection must be TLS and
    # must authenticate, which is what the daemon actually exercises.
    hbaConf = ''
      local   all   all                  trust
      hostssl all   all   127.0.0.1/32   scram-sha-256
      hostssl all   all   ::1/128        scram-sha-256
    '';
  };

  # Materialises everything the cluster and the daemon need before either
  # starts: the dev certificate authority and server certificate, the
  # process-scoped home, and the dev catalog copied from the checked-in
  # example. Every step is idempotent, so re-running `devenv up` reuses the
  # existing state and local edits to the copied catalog survive.
  tasks."signalbox:dev-instance" = {
    description = "Provision dev-instance TLS material, home, and catalog.";
    exec = ''
      set -euo pipefail

      mkdir -p ${shellArg tlsRoot} ${shellArg daemonHome} \
        ${shellArg daemonBlobStagingDirectory} ${shellArg daemonBlobPrimaryDirectory}
      chmod 700 ${shellArg tlsRoot} ${shellArg daemonBlobStagingDirectory} \
        ${shellArg daemonBlobPrimaryDirectory}

      # The pgpass refusal is a presence check against the process home. Fail
      # loudly rather than deleting a file the developer put here on purpose.
      if [ -e ${shellArg "${daemonHome}/.pgpass"} ]; then
        echo "dev instance:" ${shellArg "${daemonHome}/.pgpass"} \
             "exists; the daemon refuses to parse its database URL while it" \
             "does. Remove it." >&2
        exit 1
      fi

      # All four artifacts are required, not just the certificates: a run
      # interrupted between writing a certificate and its key would otherwise
      # be skipped here and then abort on the `chmod 600` below.
      #
      # Presence alone is only a sound gate because nothing partial is ever
      # published. Every artifact is generated under a staging directory and
      # renamed into place once it is complete, so an `openssl` run killed
      # midway — or one that runs out of space — leaves the staging copy
      # truncated and the published path untouched. A file that exists here
      # was written completely.
      if [ ! -f ${shellArg authorityCertificate} ] \
         || [ ! -f ${shellArg authorityKey} ] \
         || [ ! -f ${shellArg serverCertificate} ] \
         || [ ! -f ${shellArg serverKey} ]; then
        echo "dev instance: generating local TLS material under" \
             ${shellArg tlsRoot}

        # A leftover staging directory means a previous run died mid-generation;
        # its contents are unusable by construction, so start clean.
        rm -rf ${shellArg tlsStaging}
        mkdir -p ${shellArg tlsStaging}
        chmod 700 ${shellArg tlsStaging}

        ${shellArg openssl} req -x509 -newkey rsa:2048 -noenc -sha256 -days 3650 \
          -subj "/CN=signalbox devenv dev-instance authority" \
          -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
          -addext "keyUsage=critical,keyCertSign,cRLSign" \
          -keyout ${shellArg "${tlsStaging}/dev-ca.key"} \
          -out ${shellArg "${tlsStaging}/dev-ca.crt"} 2>/dev/null

        ${shellArg openssl} req -newkey rsa:2048 -noenc -sha256 \
          -subj "/CN=localhost" \
          -keyout ${shellArg "${tlsStaging}/server.key"} \
          -out ${shellArg "${tlsStaging}/server.csr"} 2>/dev/null

        printf '%s\n' \
          'basicConstraints=critical,CA:FALSE' \
          'keyUsage=critical,digitalSignature,keyEncipherment' \
          'extendedKeyUsage=serverAuth' \
          'subjectAltName=DNS:localhost' > ${shellArg "${tlsStaging}/server.ext"}

        ${shellArg openssl} x509 -req -in ${shellArg "${tlsStaging}/server.csr"} \
          -sha256 -days 3650 \
          -CA ${shellArg "${tlsStaging}/dev-ca.crt"} \
          -CAkey ${shellArg "${tlsStaging}/dev-ca.key"} \
          -CAcreateserial \
          -extfile ${shellArg "${tlsStaging}/server.ext"} \
          -out ${shellArg "${tlsStaging}/server.crt"} 2>/dev/null

        # Moded before publishing, so a published key is never even briefly
        # group- or world-readable.
        chmod 600 ${shellArg "${tlsStaging}/dev-ca.key"} \
                  ${shellArg "${tlsStaging}/server.key"}
        chmod 644 ${shellArg "${tlsStaging}/dev-ca.crt"} \
                  ${shellArg "${tlsStaging}/server.crt"}

        # Publish. Each `mv` is a rename within one filesystem, so every
        # destination holds either the previous artifact or the complete new
        # one — never a partial write. Removing the old set first keeps the
        # *set* consistent too: an interruption part-way through publishing
        # leaves at least one path missing, which the gate above turns into a
        # clean regeneration rather than a new authority paired with a server
        # certificate the old one signed.
        rm -f ${shellArg authorityCertificate} ${shellArg authorityKey} \
              ${shellArg serverCertificate} ${shellArg serverKey}
        mv -f ${shellArg "${tlsStaging}/dev-ca.key"} ${shellArg authorityKey}
        mv -f ${shellArg "${tlsStaging}/dev-ca.crt"} ${shellArg authorityCertificate}
        mv -f ${shellArg "${tlsStaging}/server.key"} ${shellArg serverKey}
        mv -f ${shellArg "${tlsStaging}/server.crt"} ${shellArg serverCertificate}

        # The CSR, the extension file, and openssl's serial file were only
        # ever inputs to the above and live entirely inside staging.
        rm -rf ${shellArg tlsStaging}
      fi

      # PostgreSQL refuses to start if the private key is group- or
      # world-readable.
      chmod 600 ${shellArg authorityKey} ${shellArg serverKey}
      chmod 644 ${shellArg authorityCertificate} ${shellArg serverCertificate}

      # Seeded verbatim from the checked-in example, so the copy starts with
      # that file's undated family names. The bridge relates a reported
      # identity to the configured one; the rule, including the dated snapshot
      # a family name resolves to, is stated under provider-target identity in
      # docs/spec/model-call-execution.md.
      if [ ! -f ${shellArg daemonConfigFile} ]; then
        echo "dev instance: seeding" ${shellArg daemonConfigFile} \
             "from config/signalboxd.example.toml"
        seeded_daemon_config=${shellArg daemonConfigFile}.seed-staging
        cp "$DEVENV_ROOT/config/signalboxd.example.toml" \
           "$seeded_daemon_config"
        # The example names deployment secret paths that do not exist here, and
        # a credential profile now carries its own path rather than reading one
        # from the process environment. Point the seeded copy at this
        # developer's key file once; the copy is theirs to edit afterwards.
        seed_key_file="''${SIGNALBOX_DEV_ANTHROPIC_API_KEY_FILE:-$HOME/.config/signalbox/anthropic-api-key}"
        ${tomlPython}/bin/python3 -c ${shellArg ''
          import sys
          from pathlib import Path

          import tomlkit

          config_path = Path(sys.argv[1])
          credential_path = str(Path(sys.argv[2]).absolute())
          document = tomlkit.parse(config_path.read_text())
          profiles = document.get("credential_profiles", [])
          profile = next(
              (entry for entry in profiles if entry.get("name") == "anthropic-primary"),
              None,
          )
          if profile is None:
              raise SystemExit("example config has no anthropic-primary profile")
          profile["file"] = credential_path
          if sys.argv[5] == "true":
              blob_storage = document.get("blob_storage")
              if blob_storage is None:
                  raise SystemExit("example config has no blob_storage table")
              blob_storage["staging_directory"] = str(Path(sys.argv[3]).absolute())
              stores = blob_storage.get("stores", [])
              primary = next(
                  (entry for entry in stores if entry.get("name") == "primary"),
                  None,
              )
              if primary is None:
                  raise SystemExit("example config has no primary blob store")
              primary["root_directory"] = str(Path(sys.argv[4]).absolute())
          else:
              document.pop("blob_storage", None)
          config_path.write_text(tomlkit.dumps(document))
        ''} "$seeded_daemon_config" "$seed_key_file" \
          ${shellArg daemonBlobStagingDirectory} ${shellArg daemonBlobPrimaryDirectory} \
          ${shellArg (pkgs.lib.boolToString blobStorageSupported)}
        chmod 644 "$seeded_daemon_config"
        mv -f "$seeded_daemon_config" ${shellArg daemonConfigFile}
      fi

      # Existing dev instances predate credential pools and are intentionally
      # user-editable, so do not rewrite them in place. Refuse the retired
      # shape with an exact recovery path instead of starting a daemon that can
      # only reject the retained catalog later.
      ${tomlPython}/bin/python3 -c ${shellArg ''
        import sys
        from collections.abc import Mapping
        from pathlib import Path

        import tomlkit

        config_path = Path(sys.argv[1])
        document = tomlkit.parse(config_path.read_text())
        profiles = document.get("credential_profiles", [])
        mappings = document.get("adapter_mappings", [])
        legacy_profile = any(
            isinstance(entry, Mapping)
            and "adapter" not in entry
            and "delivery" not in entry
            for entry in profiles
        )
        legacy_mapping = any(
            isinstance(entry, Mapping) and "credential_profile" in entry
            for entry in mappings
        )
        if legacy_profile or legacy_mapping:
            raise SystemExit(
                f"dev instance: {config_path} uses the retired pre-pool "
                "credential grammar; update credential_profiles and "
                "adapter_mappings to match config/signalboxd.example.toml, "
                "or move the file aside so the dev instance can reseed it"
            )
      ''} ${shellArg daemonConfigFile}

      if [ ! -f ${shellArg daemonTemplateConfigFile} ]; then
        echo "dev instance: seeding" ${shellArg daemonTemplateConfigFile} \
             "from config/session-templates.example.toml"
        cp "$DEVENV_ROOT/config/session-templates.example.toml" \
           ${shellArg daemonTemplateConfigFile}
        chmod 644 ${shellArg daemonTemplateConfigFile}
      fi
    '';
  };

  # The service points its own tooling at TCP whenever listen_addresses is
  # set, but this cluster accepts TCP only over TLS with a password — which
  # its readiness probe (`psql -c "SELECT 1" template1`) cannot supply, so the
  # cluster would never be reported ready and nothing downstream would start.
  # Send that tooling over the unix socket, where pg_hba grants trust. The
  # service sets this with mkDefault, so a plain definition wins. The daemon
  # is unaffected: it connects over TCP and never sees PGHOST, because the
  # scrub below removes it.
  env.PGHOST = "${config.env.DEVENV_RUNTIME}/postgres";

  # The cluster cannot start until its certificate exists.
  processes.postgres.after = [ "signalbox:dev-instance" ];

  processes.signalboxd = {
    after = [ "devenv:processes:postgres" ];

    # A crash is a signal worth reading during an experiment, not something
    # to loop on.
    restart.on = "never";

    exec = ''
      set -euo pipefail

      # Built with the full environment: cargo needs the real HOME for its
      # registry and the ambient certificate variables to reach crates.io.
      # Only the daemon itself runs scrubbed.
      #
      target_directory="$(
        cargo metadata --no-deps --format-version 1 |
          python3 -c "import json, sys; print(json.load(sys.stdin)['target_directory'])"
      )"

      # Resolve each separately packaged executable through Cargo's artifact
      # stream. The shared resolver also refuses a configured foreign target
      # rather than letting a later exec fail or select a stale host artifact
      # from an assumed target/debug layout.
      daemon_executable="$(
        "$DEVENV_ROOT/tooling/resolve-cargo-bin.sh" \
          "$DEVENV_ROOT/Cargo.toml" "$target_directory" \
          signalboxd signalboxd
      )"
      supervisor_executable="$(
        "$DEVENV_ROOT/tooling/resolve-cargo-bin.sh" \
          "$DEVENV_ROOT/Cargo.toml" "$target_directory" \
          signalbox-tools-exec signalbox-exec-supervisor
      )"
      bridge_executable="$(
        "$DEVENV_ROOT/tooling/resolve-cargo-bin.sh" \
          "$DEVENV_ROOT/Cargo.toml" "$target_directory" \
          signalbox-model-runtime-claude-cli signalbox-claude-mcp-bridge
      )"

      # Republished on every start so the link follows a rebuild that lands
      # the artifact somewhere else, and so a stale link from an earlier
      # layout never survives.
      mkdir -p ${shellArg daemonBinDirectory}
      ln -sfn "$bridge_executable" \
        ${shellArg "${daemonBinDirectory}/signalbox-claude-mcp-bridge"}

      # Recreated here rather than in the provisioning task because the
      # runtime directory does not survive between runs. Mode 0700 is exact:
      # the daemon rejects anything else, including the 0755 devenv gives its
      # own runtime directory.
      mkdir -p ${shellArg daemonSocketDirectory}
      chmod 700 ${shellArg daemonSocketDirectory}

      # The checked-in example carries an installation placeholder, while a
      # dev instance seeded before the exec suite existed has no daemon_tools
      # table. Materialize a runtime-only copy that resolves the placeholder or
      # adds the missing dev-managed table. An explicitly configured table is
      # preserved and remains subject to the ordinary fail-closed parser.
      signalbox-materialize-config \
        ${shellArg daemonConfigFile} ${shellArg daemonRuntimeConfigFile} \
        "$supervisor_executable"
      chmod 600 ${shellArg daemonRuntimeConfigFile}

      # Deployment-owned credential channels: one file per secret. The launcher
      # passes the two integration channels; the Anthropic key path lives in the
      # seeded model catalog instead, because a credential profile now carries
      # its own file. Naming a path that does not exist is deliberate and safe
      # here, because no file is read at startup. The read timing, what the
      # file's bytes mean, and the effect of an absent file are stated in the
      # credential lifecycle section of
      # docs/spec/configuration-and-credentials.md. The GitHub default resolves
      # against the developer's own home directory, not the process-scoped HOME
      # the exec below sets. Brave uses a devenv-state placeholder unless the
      # developer supplies an override.
      search_key_file_default=${shellArg daemonBraveApiKeyFile}
      search_key_file="''${SIGNALBOX_DEV_BRAVE_API_KEY_FILE:-$search_key_file_default}"
      token_file="''${SIGNALBOX_DEV_GITHUB_TOKEN_FILE:-$HOME/.config/signalbox/github-token}"

      exec env ${scrub} \
        HOME=${shellArg daemonHome} \
        PATH=${shellArg daemonBinDirectory}:"$PATH" \
        DATABASE_URL=${shellArg databaseUrl} \
        SIGNALBOX_CONFIG_FILE=${shellArg daemonRuntimeConfigFile} \
        SIGNALBOX_TEMPLATE_CONFIG_FILE=${shellArg daemonTemplateConfigFile} \
        BRAVE_API_KEY_FILE="$search_key_file" \
        GITHUB_TOKEN_FILE="$token_file" \
        SIGNALBOX_SOCKET_PATH=${shellArg daemonSocketPath} \
        "$daemon_executable"
    '';
  };
}
