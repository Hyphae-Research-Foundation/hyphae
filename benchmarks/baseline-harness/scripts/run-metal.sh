#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Dedicated-hardware benchmark orchestration for AWS i7i.metal-24xl.
# Expects: Ubuntu 24.04, run as root, repo tarball at /root/hyphae.
set -euo pipefail

REPO=/root/hyphae
SCRATCH=/mnt/nvme/hyphae-bench
OUT=/root/bench-results
mkdir -p "$OUT"

echo "== host fingerprint =="
uname -a
lscpu | grep -E 'Model name|Socket|Core|Thread' || true
grep -c hypervisor /proc/cpuinfo || true

echo "== local NVMe setup =="
# i7i.metal carries local instance-store NVMe. Find the largest unmounted disk.
DISK=$(lsblk -bdno NAME,SIZE,TYPE,MOUNTPOINT | awk '$3=="disk" && $4=="" {print $2, $1}' | sort -rn | head -1 | awk '{print $2}')
if [ -n "${DISK:-}" ]; then
  mkfs.ext4 -F "/dev/$DISK"
  mkdir -p /mnt/nvme
  mount -o noatime "/dev/$DISK" /mnt/nvme
else
  echo "WARNING: no spare instance-store disk found; using root volume"
  mkdir -p /mnt/nvme
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

echo "== build harness (release) =="
source "$HOME/.cargo/env"
cd "$REPO"
export HYPHAE_SOURCE_COMMIT="${HYPHAE_SOURCE_COMMIT:-unknown}"
export HYPHAE_RUSTC="$(rustc --version)"
cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml
HARNESS="$REPO/benchmarks/baseline-harness/target/release/hyphae-baseline-harness"

echo "== redis servers (strict + everysec, UDS, no TCP) =="
REDIS_DIR_A=/mnt/nvme/redis-strict
REDIS_DIR_B=/mnt/nvme/redis-everysec
mkdir -p "$REDIS_DIR_A" "$REDIS_DIR_B"
redis-server --port 0 --unixsocket /run/redis-strict.sock --dir "$REDIS_DIR_A" \
  --appendonly yes --appendfsync always --save '' --daemonize yes \
  --pidfile /run/redis-strict.pid
redis-server --port 0 --unixsocket /run/redis-everysec.sock --dir "$REDIS_DIR_B" \
  --appendonly yes --appendfsync everysec --save '' --daemonize yes \
  --pidfile /run/redis-everysec.pid
sleep 1
redis-cli -s /run/redis-strict.sock ping
redis-cli -s /run/redis-everysec.sock ping

echo "== suite: sql =="
"$HARNESS" sql "$SCRATCH" "$OUT/sql.json" --scale full

echo "== suite: keyspace =="
"$HARNESS" keyspace "$SCRATCH" "$OUT/keyspace.json" --scale full \
  --redis-strict /run/redis-strict.sock --redis-everysec /run/redis-everysec.sock

echo "== suite: lexical =="
"$HARNESS" lexical "$SCRATCH" "$OUT/lexical.json" --scale full

echo "== suite: ablation =="
"$HARNESS" ablation "$SCRATCH" "$OUT/ablation.json" --scale full

echo "== native reference smokes on the same metal =="
cargo build --release --locked -p hyphae-native-runtime --example group_commit_benchmark
./target/release/examples/group_commit_benchmark \
  "$HYPHAE_SOURCE_COMMIT" clean "$HYPHAE_RUSTC" > "$OUT/group-commit.json" || true

echo "== shutdown redis =="
redis-cli -s /run/redis-strict.sock shutdown nosave || true
redis-cli -s /run/redis-everysec.sock shutdown nosave || true

echo "== done =="
ls -la "$OUT"
