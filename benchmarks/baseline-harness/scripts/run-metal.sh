#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Dedicated-hardware benchmark orchestration for AWS i7i.metal-24xl.
# Expects: Ubuntu 24.04, run as root, repo tarball at /root/hyphae.
set -euo pipefail

REPO=/root/hyphae
SCRATCH=/mnt/nvme/hyphae-bench
OUT=/root/bench-results
VALKEY_ARTIFACT_SHA256=19c23908e7d57e8d91ef85b41f5646307582f10f4f0fb999bbf89ed24ec9c983
VALKEY_ARTIFACT_URL=https://codeload.github.com/valkey-io/valkey/tar.gz/refs/tags/9.1.2
VALKEY_NO_CONFIG_SHA256=6f3abe771321dcd8d832bd3927a0ccdd0ef08270cea41dbe7dcfa2bfae361166
VALKEY_ALWAYS_CONFIG_SHA256=e32bcbdaec8a9698a8d3d19957a7ca54527819802f74c83efcd514215d76fa91
VALKEY_EVERYSEC_CONFIG_SHA256=07bc14fd2ea328b03641576e6f274ec05dc759540325d2fbc138b31504b6e879
VALKEY_ARCHIVE="${VALKEY_ARCHIVE:-/root/valkey-9.1.2.tar.gz}"
VALKEY_CC="${VALKEY_CC:-cc}"
VALKEY_CFLAGS="${VALKEY_CFLAGS:-}"
VALKEY_LDFLAGS="${VALKEY_LDFLAGS:-}"
VALKEY_MALLOC="${VALKEY_MALLOC:-jemalloc}"
VALKEY_OPTIMIZATION="${VALKEY_OPTIMIZATION:--O3}"
VALKEY_BUILD=
VALKEY_SERVER=
VALKEY_CLI=
VALKEY_STARTED=0

cleanup() {
  if [ "$VALKEY_STARTED" -eq 1 ] && [ -n "$VALKEY_CLI" ]; then
    "$VALKEY_CLI" -s /run/hyphae-valkey-no.sock shutdown nosave >/dev/null 2>&1 || true
    "$VALKEY_CLI" -s /run/hyphae-valkey-always.sock shutdown nosave >/dev/null 2>&1 || true
    "$VALKEY_CLI" -s /run/hyphae-valkey-everysec.sock shutdown nosave >/dev/null 2>&1 || true
  fi
  if [ -n "$VALKEY_BUILD" ]; then
    rm -rf "$VALKEY_BUILD"
  fi
}
trap cleanup EXIT

mkdir -p "$OUT"

echo "== host fingerprint =="
uname -a
lscpu | grep -E 'Model name|Socket|Core|Thread' || true
grep -c hypervisor /proc/cpuinfo || true

echo "== local NVMe setup =="
# i7i.metal carries local instance-store NVMe. Reuse /mnt/nvme when the
# operator already mounted it; otherwise format the largest disk that has
# neither a mountpoint nor partitions (the root EBS disk shows an empty
# mountpoint on its disk row while its partition is mounted, so partitioned
# disks are never candidates).
if mountpoint -q /mnt/nvme; then
  echo "reusing existing /mnt/nvme mount"
else
  DISK=$(lsblk -bdno NAME,SIZE,TYPE,MOUNTPOINT | awk '$3=="disk" && $4=="" {print $2, $1}' | sort -rn | awk '{print $2}' \
    | while read -r candidate; do
        if [ "$(lsblk -no MOUNTPOINT "/dev/$candidate" | grep -c .)" -eq 0 ] && [ "$(lsblk -no TYPE "/dev/$candidate" | grep -c part)" -eq 0 ]; then
          echo "$candidate"; break
        fi
      done)
  if [ -n "${DISK:-}" ]; then
    mkfs.ext4 -F "/dev/$DISK"
    mkdir -p /mnt/nvme
    mount -o noatime "/dev/$DISK" /mnt/nvme
  else
    echo "WARNING: no spare instance-store disk found; using root volume"
    mkdir -p /mnt/nvme
  fi
fi
mkdir -p "$SCRATCH"
lsblk

echo "== performance governor =="
if command -v cpupower >/dev/null 2>&1; then
  cpupower frequency-set -g performance || true
else
  for governor in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    echo performance > "$governor" 2>/dev/null || true
  done
fi
cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo "no cpufreq"

echo "== exact clean Hyphae source identity =="
source "$HOME/.cargo/env"
cd "$REPO"
test -z "$(git status --porcelain=v1 --untracked-files=all)"
export HYPHAE_SOURCE_PRE_COMMIT
HYPHAE_SOURCE_PRE_COMMIT=$(git rev-parse 'HEAD^{commit}')
export HYPHAE_SOURCE_PRE_TREE
HYPHAE_SOURCE_PRE_TREE=$(git rev-parse 'HEAD^{tree}')
export HYPHAE_SOURCE_PRE_CLEAN=true
HYPHAE_SOURCE_COMMIT="$HYPHAE_SOURCE_PRE_COMMIT"
export HYPHAE_RUSTC
HYPHAE_RUSTC=$(rustc --version)
export HYPHAE_RUSTC_VERBOSE
HYPHAE_RUSTC_VERBOSE=$(rustc -vV)
export HYPHAE_CARGO_VERSION
HYPHAE_CARGO_VERSION=$(cargo --version)
export HYPHAE_BUILD_PROFILE=release
test -z "$(compgen -A variable CARGO_PROFILE_)"
unset RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER
export RUSTFLAGS=
export CARGO_ENCODED_RUSTFLAGS=
export CARGO_INCREMENTAL=0
export HYPHAE_RUSTFLAGS_STATE=empty
export HYPHAE_CARGO_ENCODED_RUSTFLAGS_STATE=empty
export HYPHAE_RUSTC_WRAPPER_STATE=unset
export HYPHAE_RUSTC_WORKSPACE_WRAPPER_STATE=unset
export HYPHAE_CARGO_PROFILE_OVERRIDES_STATE=absent
export HYPHAE_CARGO_INCREMENTAL_STATE=disabled
export HYPHAE_BUILD_COMMAND="cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml"

echo "== authoritative dedicated-hardware qualification =="
IFS= read -r HARDWARE_PRODUCT_NAME < /sys/devices/virtual/dmi/id/product_name
test "$HARDWARE_PRODUCT_NAME" = "i7i.metal-24xl"
test "$(nproc)" -eq 96
PHYSICAL_CORES=$(lscpu -p=Socket,Core | grep -v '^#' | sort -u | wc -l | tr -d ' ')
SMT_THREADS=$(lscpu | awk -F: '$1 ~ /^Thread\(s\) per core/ {gsub(/[[:space:]]/, "", $2); print $2}')
SOCKETS=$(lscpu -p=Socket | grep -v '^#' | sort -u | wc -l | tr -d ' ')
test "$PHYSICAL_CORES" -eq 48
test "$SMT_THREADS" -eq 2
test "$SOCKETS" -eq 2
CPU_AFFINITY=$(awk '$1 == "Cpus_allowed_list:" {print $2}' /proc/self/status)
test "$CPU_AFFINITY" = "0-95"
CGROUP_PATH=$(awk -F: '$1 == "0" && $2 == "" {print $3}' /proc/self/cgroup)
test -n "$CGROUP_PATH"
IFS=' ' read -r CPU_QUOTA CPU_PERIOD < "/sys/fs/cgroup/${CGROUP_PATH#/}/cpu.max"
test "$CPU_QUOTA" = max
test "$CPU_PERIOD" -gt 0
if grep -qw hypervisor /proc/cpuinfo; then
  exit 1
fi
test -n "$(grep -m1 '^model name' /proc/cpuinfo)"
test -n "$(</proc/sys/kernel/osrelease)"
awk '/^MemTotal:/ { found=1; if ($2 < 765041050 || $2 > 845571686) exit 1 } END { if (!found) exit 1 }' /proc/meminfo
test "$(lscpu -p=CPU,CORE,SOCKET | grep -v '^#' | wc -l | tr -d ' ')" -eq 96
mountpoint -q /mnt/nvme
NVME_SOURCE=$(findmnt -n -o SOURCE --target /mnt/nvme)
case "$NVME_SOURCE" in
  /dev/nvme*) ;;
  *) exit 1 ;;
esac
NVME_DEVICE=$(lsblk -ndo PKNAME "$NVME_SOURCE")
if [ -z "$NVME_DEVICE" ]; then
  NVME_DEVICE=$(lsblk -ndo KNAME "$NVME_SOURCE")
fi
EC2_NVME_MODEL=$(lsblk -ndo MODEL "/dev/$NVME_DEVICE")
EC2_NVME_MODEL="${EC2_NVME_MODEL%"${EC2_NVME_MODEL##*[![:space:]]}"}"
test "$EC2_NVME_MODEL" = "Amazon EC2 NVMe Instance Storage"
NVME_DEVICE_ID=$(lsblk -ndo MAJ:MIN "/dev/$NVME_DEVICE")
NVME_FILESYSTEM=$(findmnt -n -o FSTYPE --target /mnt/nvme)
NVME_ROTATIONAL=$(<"/sys/class/block/$NVME_DEVICE/queue/rotational")
NVME_QUEUE_DEPTH=$(<"/sys/class/block/$NVME_DEVICE/queue/nr_requests")
[[ "$NVME_DEVICE_ID" =~ ^259:[0-9]+$ ]]
[[ "$NVME_FILESYSTEM" == ext4 || "$NVME_FILESYSTEM" == xfs ]]
test "$NVME_ROTATIONAL" -eq 0
test "$NVME_QUEUE_DEPTH" -gt 0
for governor in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
  test "$(<"$governor")" = performance
done
export HYPHAE_RECEIPT_AUTHORITY=authoritative-dedicated-hardware
export HYPHAE_HARDWARE_QUALIFICATION=aws-ec2-i7i.metal-24xl
export HYPHAE_EC2_NVME_MODEL="$EC2_NVME_MODEL"
export HYPHAE_NVME_DEVICE_ID="$NVME_DEVICE_ID"
export HYPHAE_NVME_FILESYSTEM="$NVME_FILESYSTEM"
export HYPHAE_NVME_ROTATIONAL=false
export HYPHAE_NVME_QUEUE_DEPTH="$NVME_QUEUE_DEPTH"

echo "== verify and build exact Valkey 9.1.2 external oracle =="
if [ ! -f "$VALKEY_ARCHIVE" ]; then
  curl --fail --location --output "$VALKEY_ARCHIVE" "$VALKEY_ARTIFACT_URL"
fi
test "$(sha256sum "$VALKEY_ARCHIVE" | cut -d ' ' -f 1)" = "$VALKEY_ARTIFACT_SHA256"
VALKEY_BUILD=$(mktemp -d /tmp/hyphae-valkey-9.1.2.XXXXXX)
tar -xzf "$VALKEY_ARCHIVE" --strip-components=1 -C "$VALKEY_BUILD"
make -C "$VALKEY_BUILD" -j"$(nproc)" \
  BUILD_TLS=no MALLOC="$VALKEY_MALLOC" OPTIMIZATION="$VALKEY_OPTIMIZATION" \
  CC="$VALKEY_CC" CFLAGS="$VALKEY_CFLAGS" LDFLAGS="$VALKEY_LDFLAGS"
VALKEY_SERVER="$VALKEY_BUILD/src/valkey-server"
VALKEY_CLI="$VALKEY_BUILD/src/valkey-cli"
"$VALKEY_SERVER" --version
IFS= read -r HYPHAE_VALKEY_COMPILER < <("$VALKEY_CC" --version)
export HYPHAE_VALKEY_COMPILER
export HYPHAE_VALKEY_BUILD_FLAGS="BUILD_TLS=no;MALLOC=$VALKEY_MALLOC;OPTIMIZATION=$VALKEY_OPTIMIZATION;CC=$VALKEY_CC;CFLAGS=$VALKEY_CFLAGS;LDFLAGS=$VALKEY_LDFLAGS"
HYPHAE_VALKEY_SERVER_SHA256=$(sha256sum "$VALKEY_SERVER" | cut -d ' ' -f 1)
export HYPHAE_VALKEY_SERVER_SHA256
RETAINED_VALKEY_SERVER="$OUT/valkey-server-9.1.2"
test ! -e "$RETAINED_VALKEY_SERVER"
install -m 0555 "$VALKEY_SERVER" "$RETAINED_VALKEY_SERVER"
test "$(sha256sum "$RETAINED_VALKEY_SERVER" | cut -d ' ' -f 1)" = "$HYPHAE_VALKEY_SERVER_SHA256"
export HYPHAE_VALKEY_RETAINED_SERVER_ARTIFACT="$RETAINED_VALKEY_SERVER"
export HYPHAE_VALKEY_SOURCE_ARCHIVE_URL="$VALKEY_ARTIFACT_URL"
export HYPHAE_VALKEY_SOURCE_ARCHIVE_SHA256="$VALKEY_ARTIFACT_SHA256"
VALKEY_SETUP_NONCE=$(</proc/sys/kernel/random/uuid)
export HYPHAE_VALKEY_SETUP_ID
HYPHAE_VALKEY_SETUP_ID=$(printf '%s:%s:%s\n' \
  "$HYPHAE_SOURCE_COMMIT" "$HYPHAE_VALKEY_SERVER_SHA256" "$VALKEY_SETUP_NONCE" \
  | sha256sum | cut -d ' ' -f 1)

echo "== build harness (release) =="
VALKEY_PROFILE_PATH=benchmarks/baseline-harness/valkey/application-core-v1.json
export HYPHAE_VALKEY_PROFILE_SHA256
HYPHAE_VALKEY_PROFILE_SHA256=$(sha256sum "$VALKEY_PROFILE_PATH" | cut -d ' ' -f 1)
python3 benchmarks/baseline-harness/valkey/check_profile.py \
  --executed-source-commit "$HYPHAE_SOURCE_PRE_COMMIT" --compact
export HYPHAE_VALKEY_SEMANTIC_BUNDLE_SHA256
HYPHAE_VALKEY_SEMANTIC_BUNDLE_SHA256=$(python3 \
  benchmarks/baseline-harness/valkey/check_profile.py \
  --executed-source-commit "$HYPHAE_SOURCE_PRE_COMMIT" --semantic-bundle-only)
export HYPHAE_VALKEY_CLAIM_SEMANTICS_SHA256
HYPHAE_VALKEY_CLAIM_SEMANTICS_SHA256=$(python3 \
  benchmarks/baseline-harness/valkey/check_profile.py --claim-seal-only)
cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml
HARNESS="$REPO/benchmarks/baseline-harness/target/release/hyphae-baseline-harness"
test -z "$(git status --porcelain=v1 --untracked-files=all)"
export HYPHAE_SOURCE_POST_COMMIT
HYPHAE_SOURCE_POST_COMMIT=$(git rev-parse 'HEAD^{commit}')
export HYPHAE_SOURCE_POST_TREE
HYPHAE_SOURCE_POST_TREE=$(git rev-parse 'HEAD^{tree}')
export HYPHAE_SOURCE_POST_CLEAN=true
test "$HYPHAE_SOURCE_POST_COMMIT" = "$HYPHAE_SOURCE_PRE_COMMIT"
test "$HYPHAE_SOURCE_POST_TREE" = "$HYPHAE_SOURCE_PRE_TREE"
python3 benchmarks/baseline-harness/valkey/check_profile.py \
  --executed-source-commit "$HYPHAE_SOURCE_POST_COMMIT" --compact
export HYPHAE_HARNESS_PRODUCT_BINARY_SHA256
HYPHAE_HARNESS_PRODUCT_BINARY_SHA256=$(sha256sum "$HARNESS" | cut -d ' ' -f 1)

echo "== Valkey 9.1.2 servers (no + always + everysec, UDS, no TCP) =="
VALKEY_CONFIG="$REPO/benchmarks/baseline-harness/valkey/config"
test "$(sha256sum "$VALKEY_CONFIG/valkey-no.conf" | cut -d ' ' -f 1)" = "$VALKEY_NO_CONFIG_SHA256"
test "$(sha256sum "$VALKEY_CONFIG/valkey-always.conf" | cut -d ' ' -f 1)" = "$VALKEY_ALWAYS_CONFIG_SHA256"
test "$(sha256sum "$VALKEY_CONFIG/valkey-everysec.conf" | cut -d ' ' -f 1)" = "$VALKEY_EVERYSEC_CONFIG_SHA256"
if pgrep -x valkey-server >/dev/null; then
  exit 1
fi
for stale in \
  /run/hyphae-valkey-no.sock /run/hyphae-valkey-always.sock /run/hyphae-valkey-everysec.sock \
  /run/hyphae-valkey-no.pid /run/hyphae-valkey-always.pid /run/hyphae-valkey-everysec.pid; do
  if [ -e "$stale" ] || [ -L "$stale" ]; then
    exit 1
  fi
done
rm -rf /mnt/nvme/valkey-no /mnt/nvme/valkey-always /mnt/nvme/valkey-everysec
mkdir -p /mnt/nvme/valkey-no /mnt/nvme/valkey-always /mnt/nvme/valkey-everysec
printf '%s:no\n' "$HYPHAE_VALKEY_SETUP_ID" > /mnt/nvme/valkey-no/.hyphae-fresh-setup
printf '%s:always\n' "$HYPHAE_VALKEY_SETUP_ID" > /mnt/nvme/valkey-always/.hyphae-fresh-setup
printf '%s:everysec\n' "$HYPHAE_VALKEY_SETUP_ID" > /mnt/nvme/valkey-everysec/.hyphae-fresh-setup
VALKEY_STARTED=1
"$VALKEY_SERVER" "$VALKEY_CONFIG/valkey-no.conf"
"$VALKEY_SERVER" "$VALKEY_CONFIG/valkey-always.conf"
"$VALKEY_SERVER" "$VALKEY_CONFIG/valkey-everysec.conf"
sleep 1
NO_PID=$(</run/hyphae-valkey-no.pid)
ALWAYS_PID=$(</run/hyphae-valkey-always.pid)
EVERYSEC_PID=$(</run/hyphae-valkey-everysec.pid)
test "$NO_PID" != "$ALWAYS_PID"
test "$NO_PID" != "$EVERYSEC_PID"
test "$ALWAYS_PID" != "$EVERYSEC_PID"
kill -0 "$NO_PID" "$ALWAYS_PID" "$EVERYSEC_PID"
export HYPHAE_VALKEY_NO_PID="$NO_PID"
export HYPHAE_VALKEY_ALWAYS_PID="$ALWAYS_PID"
export HYPHAE_VALKEY_EVERYSEC_PID="$EVERYSEC_PID"
"$VALKEY_CLI" -s /run/hyphae-valkey-no.sock ping
"$VALKEY_CLI" -s /run/hyphae-valkey-always.sock ping
"$VALKEY_CLI" -s /run/hyphae-valkey-everysec.sock ping

echo "== suite: sql =="
"$HARNESS" sql "$SCRATCH" "$OUT/sql.json" --scale full

echo "== suite: keyspace =="
"$HARNESS" keyspace "$SCRATCH" "$OUT/keyspace.json" --scale full \
  --valkey-no /run/hyphae-valkey-no.sock \
  --valkey-always /run/hyphae-valkey-always.sock \
  --valkey-everysec /run/hyphae-valkey-everysec.sock
python3 benchmarks/baseline-harness/valkey/check_receipt.py "$OUT/keyspace.json" \
  --expected-source-commit "$HYPHAE_SOURCE_POST_COMMIT" \
  --expected-source-tree "$HYPHAE_SOURCE_POST_TREE" \
  --harness "$HARNESS" \
  --valkey-binary-artifact "$RETAINED_VALKEY_SERVER" \
  --source-root "$REPO" \
  --require-authoritative

echo "== suite: lexical =="
"$HARNESS" lexical "$SCRATCH" "$OUT/lexical.json" --scale full

echo "== suite: ablation =="
"$HARNESS" ablation "$SCRATCH" "$OUT/ablation.json" --scale full

echo "== native reference smokes on the same metal =="
cargo build --release --locked -p hyphae-native-runtime --example group_commit_benchmark
./target/release/examples/group_commit_benchmark \
  "$HYPHAE_SOURCE_COMMIT" clean "$HYPHAE_RUSTC" > "$OUT/group-commit.json" || true

echo "== shutdown Valkey =="
"$VALKEY_CLI" -s /run/hyphae-valkey-no.sock shutdown nosave || true
"$VALKEY_CLI" -s /run/hyphae-valkey-always.sock shutdown nosave || true
"$VALKEY_CLI" -s /run/hyphae-valkey-everysec.sock shutdown nosave || true

echo "== done =="
ls -la "$OUT"
