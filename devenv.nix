{ pkgs, config, ... }:

let
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

  # A process-scoped home for the daemon. `production_connection_options`
  # refuses to parse when `~/.pgpass` exists under the process home, and it
  # resolves that home with `std::env::home_dir()`, which on Unix returns
  # `$HOME` whenever it is set and nonempty. Pointing HOME at a devenv-owned
  # directory therefore makes the refusal deterministic instead of dependent
  # on whatever the developer keeps in their real home directory.
  daemonHome = "${stateRoot}/home";

  daemonConfigFile = "${stateRoot}/signalboxd.toml";

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
  percentEncodeQueryValue = builtins.replaceStrings
    [ "%" "#" "&" "?" "+" "=" " " "\"" "<" ">" "\\" "^" "`" "{" "|" "}" ]
    [
      "%25"
      "%23"
      "%26"
      "%3F"
      "%2B"
      "%3D"
      "%20"
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
  packages = [ pkgs.sccache ];
  env.RUSTC_WRAPPER = "sccache";

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
    # Every interpolated path is quoted: DEVENV_STATE sits inside the
    # checkout, so a directory name containing whitespace anywhere above the
    # repository would otherwise word-split these commands.
    exec = ''
      set -euo pipefail

      mkdir -p "${tlsRoot}" "${daemonHome}"
      chmod 700 "${tlsRoot}"

      # The pgpass refusal is a presence check against the process home. Fail
      # loudly rather than deleting a file the developer put here on purpose.
      if [ -e "${daemonHome}/.pgpass" ]; then
        echo "dev instance: ${daemonHome}/.pgpass exists; the daemon refuses" \
             "to parse its database URL while it does. Remove it." >&2
        exit 1
      fi

      if [ ! -f "${authorityCertificate}" ] || [ ! -f "${serverCertificate}" ]; then
        echo "dev instance: generating local TLS material under ${tlsRoot}"
        rm -f "${authorityCertificate}" "${authorityKey}" \
              "${serverCertificate}" "${serverKey}"

        "${openssl}" req -x509 -newkey rsa:2048 -noenc -sha256 -days 3650 \
          -subj "/CN=signalbox devenv dev-instance authority" \
          -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
          -addext "keyUsage=critical,keyCertSign,cRLSign" \
          -keyout "${authorityKey}" -out "${authorityCertificate}" 2>/dev/null

        "${openssl}" req -newkey rsa:2048 -noenc -sha256 \
          -subj "/CN=localhost" \
          -keyout "${serverKey}" -out "${tlsRoot}/server.csr" 2>/dev/null

        printf '%s\n' \
          'basicConstraints=critical,CA:FALSE' \
          'keyUsage=critical,digitalSignature,keyEncipherment' \
          'extendedKeyUsage=serverAuth' \
          'subjectAltName=DNS:localhost' > "${tlsRoot}/server.ext"

        "${openssl}" x509 -req -in "${tlsRoot}/server.csr" -sha256 -days 3650 \
          -CA "${authorityCertificate}" -CAkey "${authorityKey}" -CAcreateserial \
          -extfile "${tlsRoot}/server.ext" -out "${serverCertificate}" 2>/dev/null

        rm -f "${tlsRoot}/server.csr" "${tlsRoot}/server.ext"
      fi

      # PostgreSQL refuses to start if the private key is group- or
      # world-readable.
      chmod 600 "${authorityKey}" "${serverKey}"
      chmod 644 "${authorityCertificate}" "${serverCertificate}"

      # Seeded verbatim from the checked-in example, which already states the
      # dated canonical model id. Keep it that way when editing the copy:
      # `provider_model` must be the exact identity the provider echoes back,
      # and an undated alias makes a call commit ambiguously, stops the daemon,
      # and wedges the session it was serving.
      if [ ! -f "${daemonConfigFile}" ]; then
        echo "dev instance: seeding ${daemonConfigFile} from config/signalboxd.example.toml"
        cp "$DEVENV_ROOT/config/signalboxd.example.toml" "${daemonConfigFile}"
        chmod 644 "${daemonConfigFile}"
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
      # The artifact path comes from Cargo's own build output rather than an
      # assumed $target_dir/debug: a developer with `build.target` in their
      # Cargo configuration, or CARGO_BUILD_TARGET in the environment — both
      # inherited here, because this build is deliberately unscrubbed — gets
      # the executable under $target_dir/<triple>/debug instead. Diagnostics
      # still render to stderr; only the JSON artifact stream is captured.
      daemon_executable="$(
        cargo build --package signalboxd --bin signalboxd \
          --message-format=json-render-diagnostics |
          python3 -c "import json, sys; print([message['executable'] for message in map(json.loads, sys.stdin) if message.get('executable')][-1])"
      )"

      # Recreated here rather than in the provisioning task because the
      # runtime directory does not survive between runs. Mode 0700 is exact:
      # the daemon rejects anything else, including the 0755 devenv gives its
      # own runtime directory.
      mkdir -p "${daemonSocketDirectory}"
      chmod 700 "${daemonSocketDirectory}"

      # Deployment-owned credential channel: a file whose bytes are the key.
      # It is read lazily, per model call, never at startup — so the daemon
      # boots, migrates, scans, and serves the process socket with this file
      # absent. Only model calls fail, and they fail as an unavailable
      # credential.
      key_file="''${SIGNALBOX_DEV_ANTHROPIC_API_KEY_FILE:-$HOME/.config/signalbox/anthropic-api-key}"

      exec env ${scrub} \
        HOME="${daemonHome}" \
        DATABASE_URL="${databaseUrl}" \
        SIGNALBOX_CONFIG_FILE="${daemonConfigFile}" \
        ANTHROPIC_API_KEY_FILE="$key_file" \
        SIGNALBOX_SOCKET_PATH="${daemonSocketPath}" \
        "$daemon_executable"
    '';
  };
}
