#!/usr/bin/env bash

SIMULATOR_DEFAULT_MIN_IOS_VERSION="${SIMULATOR_DEFAULT_MIN_IOS_VERSION:-26.4}"

simulator_list_available() {
	xcrun simctl list devices available
}

simulator_destination_device_id() {
	local destination="${1:-}"

	if [[ "$destination" =~ (^|,)id=([^,]+) ]]; then
		printf '%s\n' "${BASH_REMATCH[2]}"
		return 0
	fi

	return 1
}

simulator_destination_device_name() {
	local destination="${1:-}"

	if [[ "$destination" =~ (^|,)name=([^,]+) ]]; then
		printf '%s\n' "${BASH_REMATCH[2]}"
		return 0
	fi

	return 1
}

simulator_destination_runtime_os() {
	local destination="${1:-}"
	local os

	if [[ "$destination" =~ (^|,)OS=([^,]+) ]]; then
		os="${BASH_REMATCH[2]}"
		case "$os" in
		latest | Latest | LATEST)
			return 1
			;;
		esac
		if [[ "$os" =~ ^([0-9]+\.[0-9]+)\. ]]; then
			printf '%s\n' "${BASH_REMATCH[1]}"
		else
			printf '%s\n' "$os"
		fi
		return 0
	fi

	return 1
}

simulator_find_iphone_ids_by_name() {
	local name="$1"
	local os="${2:-}"
	local device_list="$3"

	printf '%s\n' "$device_list" | awk -v name="$name" -v os="$os" '
    /^-- / { runtime = $0 }
    /\((Booted|Shutdown)\)/ && (os == "" || index(runtime, os)) {
      line = $0
      sub(/^[[:space:]]*/, "", line)
      metadata = line
      sub(/ \((Booted|Shutdown)\).*$/, "", metadata)
      device_name = metadata
      sub(/ \([A-F0-9-]+\)$/, "", device_name)
      if (device_name != name) {
        next
      }
      id = metadata
      sub(/^.* \(/, "", id)
      sub(/\)$/, "", id)
      print id
    }
  '
}

simulator_find_device_ids_by_name() {
	simulator_find_iphone_ids_by_name "$@"
}

simulator_find_newest_compatible_device_ids_by_name() {
	local name="$1"
	local min_os="${2:-$SIMULATOR_DEFAULT_MIN_IOS_VERSION}"
	local device_list="$3"

	printf '%s\n' "$device_list" | awk -v name="$name" -v min_os="$min_os" '
    /^-- iOS / {
      runtime = $3
      split(runtime, version, ".")
      runtime_major = version[1] + 0
      runtime_minor = version[2] + 0
      split(min_os, min_version, ".")
      min_major = min_version[1] + 0
      min_minor = min_version[2] + 0
      next
    }
    /\((Booted|Shutdown)\)/ && (runtime_major > min_major || (runtime_major == min_major && runtime_minor >= min_minor)) {
      line = $0
      sub(/^[[:space:]]*/, "", line)
      metadata = line
      sub(/ \((Booted|Shutdown)\).*$/, "", metadata)
      device_name = metadata
      sub(/ \([A-F0-9-]+\)$/, "", device_name)
      if (device_name != name) {
        next
      }
      if (runtime_major > best_major || (runtime_major == best_major && runtime_minor > best_minor)) {
        best_major = runtime_major
        best_minor = runtime_minor
        booted = ""
        shutdown = ""
      }
      if (runtime_major == best_major && runtime_minor == best_minor) {
        id = metadata
        sub(/^.* \(/, "", id)
        sub(/\)$/, "", id)
        if (/\(Booted\)/) {
          booted = booted id "\n"
        } else {
          shutdown = shutdown id "\n"
        }
      }
    }
    END {
      printf "%s%s", booted, shutdown
    }
  '
}

simulator_resolve_device_id_by_name() {
	local name="$1"
	local runtime_os="${2:-}"
	local min_os="${3:-$SIMULATOR_DEFAULT_MIN_IOS_VERSION}"
	local device_list
	local exact_matches

	device_list="$(simulator_list_available)"
	if [[ -n "$runtime_os" ]]; then
		exact_matches="$(simulator_find_device_ids_by_name "$name" "$runtime_os" "$device_list")"
		if [[ -n "$exact_matches" ]]; then
			printf '%s\n' "$exact_matches" | head -n 1
			return 0
		fi
	fi

	simulator_find_newest_compatible_device_ids_by_name "$name" "$min_os" "$device_list" | head -n 1
}

simulator_find_newest_compatible_iphone_ids() {
	local min_os="${1:-$SIMULATOR_DEFAULT_MIN_IOS_VERSION}"
	local device_list="$2"

	printf '%s\n' "$device_list" | awk -v min_os="$min_os" '
    /^-- iOS / {
      runtime = $3
      split(runtime, version, ".")
      runtime_major = version[1] + 0
      runtime_minor = version[2] + 0
      split(min_os, min_version, ".")
      min_major = min_version[1] + 0
      min_minor = min_version[2] + 0
      next
    }
    /iPhone/ && /\((Booted|Shutdown)\)/ && (runtime_major > min_major || (runtime_major == min_major && runtime_minor >= min_minor)) {
      if (runtime_major > best_major || (runtime_major == best_major && runtime_minor > best_minor)) {
        best_major = runtime_major
        best_minor = runtime_minor
        booted = ""
        shutdown = ""
      }
      if (runtime_major == best_major && runtime_minor == best_minor) {
        id = $(NF - 1)
        gsub(/[()]/, "", id)
        if (/\(Booted\)/) {
          booted = booted id "\n"
        } else {
          shutdown = shutdown id "\n"
        }
      }
    }
    END {
      printf "%s%s", booted, shutdown
    }
  '
}

simulator_resolve_iphone_ids() {
	local min_os="${1:-$SIMULATOR_DEFAULT_MIN_IOS_VERSION}"
	local device_list
	local device_name
	local device_os

	if [[ -n "${XCODE_SIMULATOR_ID:-}" ]]; then
		printf '%s\n' "$XCODE_SIMULATOR_ID"
		return 0
	fi

	if [[ -n "${XCODE_DESTINATION:-}" ]]; then
		if simulator_destination_device_id "$XCODE_DESTINATION"; then
			return 0
		fi

		if device_name="$(simulator_destination_device_name "$XCODE_DESTINATION")"; then
			device_os="$(simulator_destination_runtime_os "$XCODE_DESTINATION" || true)"
			device_list="$(simulator_list_available)"
			simulator_find_iphone_ids_by_name "$device_name" "$device_os" "$device_list"
			return 0
		fi
	fi

	device_list="$(simulator_list_available)"
	simulator_find_newest_compatible_iphone_ids "$min_os" "$device_list"
}

simulator_xcode_destination_for_id() {
	local device_id="$1"
	printf 'platform=iOS Simulator,id=%s\n' "$device_id"
}
