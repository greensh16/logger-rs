#!/bin/bash
#
# Cross-compile a fully static Linux binary, from macOS or Linux.
#
# The result links against musl rather than glibc, so it runs on any Gadi node
# regardless of the host's glibc version.

set -euo pipefail

TARGET="x86_64-unknown-linux-musl"
BIN_NAME="hpc-telemetry"
TARGET_BIN="target/${TARGET}/release/${BIN_NAME}"

echo "Building ${BIN_NAME} for ${TARGET}..."

# The musl cross-linker is only needed when building from macOS; on Linux the
# musl-tools package provides the linker under a different name.
if [[ "$(uname -s)" == "Darwin" ]] && ! command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
    echo "Error: musl-cross toolchain not found." >&2
    echo "Install with: brew install filosottile/musl-cross/musl-cross" >&2
    exit 1
fi

if ! rustup target list --installed | grep -qx "${TARGET}"; then
    echo "Adding ${TARGET} target..."
    rustup target add "${TARGET}"
fi

cargo build --release --target "${TARGET}"

# Copy alongside the native build for convenience.
#
# target/release/ does not exist on a checkout that has only ever been
# cross-compiled, and the bare `cp` this script used to do then failed with a
# confusing "No such file or directory" at the very end of an otherwise
# successful build.
mkdir -p target/release
cp "${TARGET_BIN}" target/release/

echo
echo "Build complete."
file "${TARGET_BIN}"
ls -lh "${TARGET_BIN}"

# A dynamically linked result would defeat the point of the exercise.
if file "${TARGET_BIN}" | grep -q "dynamically linked"; then
    echo "Warning: binary is dynamically linked; it may not run on Gadi." >&2
fi

cat <<EOF

Ready to deploy. Stage the binary and the shell library together — they are
versioned as a pair, and a module that puts one on PATH without the other
fails at job start with a confusing "not found":

  scp ${TARGET_BIN} hpc-telemetry.sh \\
      gadi.nci.org.au:/g/data/gb02/hpc-telemetry/<version>/bin/

Then point the modulefile at the new version. In a PBS job script users write:

  module use /g/data/gb02/modules
  module load hpc-telemetry
  source hpc-telemetry.sh

  telemetry_start
      <your commands>
  telemetry_stop
EOF
