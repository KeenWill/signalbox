#!/usr/bin/env bash
set -euo pipefail

NATIVE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPOSITORY_ROOT="$(cd "$NATIVE_ROOT/../.." && pwd)"

python3 "$NATIVE_ROOT/scripts/generate-plan-contract.py" --check

if ! command -v initdb >/dev/null 2>&1; then
	POSTGRES_INITDB="$REPOSITORY_ROOT/.devenv/profile/bin/initdb"
	if [[ -x "$POSTGRES_INITDB" ]]; then
		POSTGRES_BIN="$(dirname "$POSTGRES_INITDB")"
		PATH="$POSTGRES_BIN:$PATH"
		export PATH
	elif command -v devenv >/dev/null 2>&1; then
		POSTGRES_INITDB="$(
			devenv shell -- bash -lc 'command -v initdb' || true
		)"
		if [[ -x "$POSTGRES_INITDB" ]]; then
			POSTGRES_BIN="$(dirname "$POSTGRES_INITDB")"
			PATH="$POSTGRES_BIN:$PATH"
			export PATH
		fi
	fi
fi
if ! command -v initdb >/dev/null 2>&1; then
	echo "The real-server harness requires initdb, postgres, createdb, and pg_isready." >&2
	exit 1
fi

for required_command in postgres createdb pg_isready python3 openssl xcodebuild xcrun cargo; do
	if ! command -v "$required_command" >/dev/null 2>&1; then
		echo "The real-server harness requires $required_command." >&2
		exit 1
	fi
done

TEMPORARY_ROOT="$(mktemp -d /tmp/sb-native-real.XXXXXX)"
POSTGRES_PID=""
DAEMON_PID=""
POSTGRES_LOG="$TEMPORARY_ROOT/postgres.log"
DAEMON_LOG="$TEMPORARY_ROOT/signalboxd.log"

cleanup() {
	local status=$?
	trap - EXIT HUP INT TERM
	if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
		kill -TERM "$DAEMON_PID" 2>/dev/null || true
		wait "$DAEMON_PID" 2>/dev/null || true
	fi
	if [[ -n "$POSTGRES_PID" ]] && kill -0 "$POSTGRES_PID" 2>/dev/null; then
		kill -TERM "$POSTGRES_PID" 2>/dev/null || true
		wait "$POSTGRES_PID" 2>/dev/null || true
	fi
	if ((status != 0)); then
		if [[ -f "$DAEMON_LOG" ]]; then
			echo "signalboxd log:" >&2
			sed -n '1,240p' "$DAEMON_LOG" >&2
		fi
		if [[ -f "$POSTGRES_LOG" ]]; then
			echo "PostgreSQL log:" >&2
			sed -n '1,240p' "$POSTGRES_LOG" >&2
		fi
	fi
	rm -rf "$TEMPORARY_ROOT"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

choose_postgres_port() {
	python3 - <<'PY'
import socket

with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
}

POSTGRES_PORT=""
POSTGRES_DATA="$TEMPORARY_ROOT/postgres"
TLS_ROOT_CERTIFICATE="$TEMPORARY_ROOT/postgres-root.crt"
TLS_ROOT_KEY="$TEMPORARY_ROOT/postgres-root.key"
TLS_SERVER_CERTIFICATE="$TEMPORARY_ROOT/postgres-server.crt"
TLS_SERVER_KEY="$TEMPORARY_ROOT/postgres-server.key"
TLS_SERVER_REQUEST="$TEMPORARY_ROOT/postgres-server.csr"
TLS_SERVER_EXTENSIONS="$TEMPORARY_ROOT/postgres-server.ext"
openssl req \
	-x509 \
	-newkey rsa:2048 \
	-nodes \
	-keyout "$TLS_ROOT_KEY" \
	-out "$TLS_ROOT_CERTIFICATE" \
	-days 1 \
	-subj "/CN=Signalbox native harness root" \
	>/dev/null 2>&1
openssl req \
	-newkey rsa:2048 \
	-nodes \
	-keyout "$TLS_SERVER_KEY" \
	-out "$TLS_SERVER_REQUEST" \
	-subj "/CN=localhost" \
	>/dev/null 2>&1
cat >"$TLS_SERVER_EXTENSIONS" <<'EOF'
subjectAltName = DNS:localhost,IP:127.0.0.1
extendedKeyUsage = serverAuth
EOF
openssl x509 \
	-req \
	-in "$TLS_SERVER_REQUEST" \
	-CA "$TLS_ROOT_CERTIFICATE" \
	-CAkey "$TLS_ROOT_KEY" \
	-CAcreateserial \
	-out "$TLS_SERVER_CERTIFICATE" \
	-days 1 \
	-sha256 \
	-extfile "$TLS_SERVER_EXTENSIONS" \
	>/dev/null 2>&1
chmod 600 "$TLS_SERVER_KEY"

initdb \
	-D "$POSTGRES_DATA" \
	--auth=trust \
	--encoding=UTF8 \
	--no-locale \
	>"$POSTGRES_LOG" 2>&1

start_postgres() {
	local attempt
	local ready

	for attempt in {1..5}; do
		POSTGRES_PORT="$(choose_postgres_port)"
		postgres \
			-D "$POSTGRES_DATA" \
			-h 127.0.0.1 \
			-p "$POSTGRES_PORT" \
			-c ssl=on \
			-c "ssl_cert_file=$TLS_SERVER_CERTIFICATE" \
			-c "ssl_key_file=$TLS_SERVER_KEY" \
			>>"$POSTGRES_LOG" 2>&1 &
		POSTGRES_PID=$!
		ready=0

		for _ in {1..100}; do
			if ! kill -0 "$POSTGRES_PID" 2>/dev/null; then
				break
			fi
			if pg_isready -h 127.0.0.1 -p "$POSTGRES_PORT" >/dev/null 2>&1; then
				ready=1
				break
			fi
			sleep 0.1
		done
		if ((ready == 1)); then
			return 0
		fi

		if kill -0 "$POSTGRES_PID" 2>/dev/null; then
			kill -TERM "$POSTGRES_PID" 2>/dev/null || true
		fi
		wait "$POSTGRES_PID" 2>/dev/null || true
		POSTGRES_PID=""
		echo "Temporary PostgreSQL startup attempt $attempt failed." >&2
	done

	echo "Temporary PostgreSQL did not become ready after five startup attempts." >&2
	return 1
}

start_postgres
createdb -h 127.0.0.1 -p "$POSTGRES_PORT" signalbox_native_real

MODEL_CONFIGURATION="$TEMPORARY_ROOT/models.toml"
TEMPLATE_CONFIGURATION="$TEMPORARY_ROOT/templates.toml"
ANTHROPIC_CREDENTIAL="$TEMPORARY_ROOT/anthropic-api-key"
BRAVE_CREDENTIAL="$TEMPORARY_ROOT/brave-api-key"
GITHUB_CREDENTIAL="$TEMPORARY_ROOT/github-token"
cat >"$MODEL_CONFIGURATION" <<EOF
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "$ANTHROPIC_CREDENTIAL"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Synthetic real-server harness compaction prompt."

[web_fetch]
allowed_origins = []

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "synthetic-real-server-model"
max_output_tokens = 1024
context_window_tokens = 8192

[[aliases]]
alias_id = "30000000-0000-4000-8000-000000000001"
selection_id = "10000000-0000-4000-8000-000000000001"
EOF
sed -n \
	'/^\[numeric_bounds\]$/,/^# Omit this table/{ /^# Omit this table/!p; }' \
	"$REPOSITORY_ROOT/config/signalboxd.example.toml" >>"$MODEL_CONFIGURATION"
cat >"$TEMPLATE_CONFIGURATION" <<'EOF'
version = 1
EOF
: >"$ANTHROPIC_CREDENTIAL"
: >"$BRAVE_CREDENTIAL"
: >"$GITHUB_CREDENTIAL"

(
	cd "$REPOSITORY_ROOT"
	cargo build --locked -p signalboxd
)

SOCKET_PATH="$TEMPORARY_ROOT/signalboxd.sock"
mkdir "$TEMPORARY_ROOT/home"
DATABASE_USER="$(id -un)"
env \
	-u PGAPPNAME \
	-u PGDATABASE \
	-u PGHOST \
	-u PGHOSTADDR \
	-u PGOPTIONS \
	-u PGPASSFILE \
	-u PGPASSWORD \
	-u PGPORT \
	-u PGSSLCERT \
	-u PGSSLKEY \
	-u PGSSLMODE \
	-u PGSSLROOTCERT \
	-u PGUSER \
	-u SSL_CERT_DIR \
	-u SSL_CERT_FILE \
	HOME="$TEMPORARY_ROOT/home" \
	DATABASE_URL="postgres://$DATABASE_USER@127.0.0.1:$POSTGRES_PORT/signalbox_native_real?sslmode=verify-full&sslrootcert=$TLS_ROOT_CERTIFICATE" \
	SIGNALBOX_CONFIG_FILE="$MODEL_CONFIGURATION" \
	SIGNALBOX_TEMPLATE_CONFIG_FILE="$TEMPLATE_CONFIGURATION" \
	BRAVE_API_KEY_FILE="$BRAVE_CREDENTIAL" \
	GITHUB_TOKEN_FILE="$GITHUB_CREDENTIAL" \
	SIGNALBOX_SOCKET_PATH="$SOCKET_PATH" \
	"$REPOSITORY_ROOT/target/debug/signalboxd" \
	>"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!

for _ in {1..200}; do
	if [[ -S "$SOCKET_PATH" ]]; then
		break
	fi
	if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
		echo "signalboxd exited before binding its temporary socket." >&2
		exit 1
	fi
	sleep 0.1
done
if [[ ! -S "$SOCKET_PATH" ]]; then
	echo "signalboxd did not bind its temporary socket." >&2
	exit 1
fi

DERIVED_DATA_PATH="${SIGNALBOX_NATIVE_REAL_SERVER_DERIVED_DATA_PATH:-$NATIVE_ROOT/.derivedData-real-server}"
RESULT_BUNDLE_PATH="${SIGNALBOX_NATIVE_REAL_SERVER_RESULT_BUNDLE_PATH:-$DERIVED_DATA_PATH/Logs/Test/SignalboxNative-RealServer.xcresult}"
DESTINATION="${XCODE_DESTINATION:-platform=macOS,arch=$(uname -m)}"
BUILD_CMD=(
	xcodebuild
	-quiet
	-project "$NATIVE_ROOT/SignalboxNative.xcodeproj"
	-scheme SignalboxNative
	-configuration Debug
	-destination "$DESTINATION"
	-derivedDataPath "$DERIVED_DATA_PATH"
	CODE_SIGNING_ALLOWED=NO
	-parallel-testing-enabled NO
	build-for-testing
)

mkdir -p "$(dirname "$RESULT_BUNDLE_PATH")"
rm -rf "$RESULT_BUNDLE_PATH"
find "$DERIVED_DATA_PATH" -name '*.xctestrun' -delete 2>/dev/null || true
printf '+ %q ' "${BUILD_CMD[@]}"
printf '\n'
"${BUILD_CMD[@]}"

XCTESTRUN_PATHS="$(
	find "$DERIVED_DATA_PATH" -name '*.xctestrun' -type f -print
)"
if [[ -z "$XCTESTRUN_PATHS" ]] || [[ "$(printf '%s\n' "$XCTESTRUN_PATHS" | wc -l)" -ne 1 ]]; then
	echo "Expected exactly one generated xctestrun file." >&2
	exit 1
fi
XCTESTRUN_PATH="$XCTESTRUN_PATHS"
python3 - "$XCTESTRUN_PATH" "$SOCKET_PATH" <<'PY'
import plistlib
import sys

xctestrun_path, socket_path = sys.argv[1:]
with open(xctestrun_path, "rb") as source:
    document = plistlib.load(source)
matched = 0
for configuration in document["TestConfigurations"]:
    for target in configuration["TestTargets"]:
        if target["BlueprintName"] == "SignalboxNativeTests":
            target.setdefault("EnvironmentVariables", {})[
                "SIGNALBOX_SOCKET_PATH"
            ] = socket_path
            matched += 1
if matched != 1:
    raise SystemExit("Expected exactly one SignalboxNativeTests xctestrun target.")
with open(xctestrun_path, "wb") as destination:
    plistlib.dump(document, destination)
PY

TEST_CMD=(
	xcodebuild
	-quiet
	-xctestrun "$XCTESTRUN_PATH"
	-destination "$DESTINATION"
	-resultBundlePath "$RESULT_BUNDLE_PATH"
	-only-testing:SignalboxNativeTests/RealServerHarnessTests
	-parallel-testing-enabled NO
	test-without-building
)
printf '+ %q ' "${TEST_CMD[@]}"
printf '\n'
"${TEST_CMD[@]}"

summary="$(xcrun xcresulttool get test-results summary --path "$RESULT_BUNDLE_PATH" --compact)"
python3 - "$summary" <<'PY'
import json
import sys

summary = json.loads(sys.argv[1])
if (
    summary.get("result") != "Passed"
    or summary.get("passedTests", 0) < 1
    or summary.get("failedTests") != 0
    or summary.get("skippedTests") != 0
    or summary.get("totalTestCount") != summary.get("passedTests")
):
    print(json.dumps(summary, separators=(",", ":")), file=sys.stderr)
    raise SystemExit(65)
PY
