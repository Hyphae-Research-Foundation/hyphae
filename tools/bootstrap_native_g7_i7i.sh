#!/usr/bin/env bash
set -euo pipefail
# SPDX-License-Identifier: AGPL-3.0-only

exec > >(tee /var/log/hyphae-g7-bootstrap.log | logger -t hyphae-g7-bootstrap -s 2>/dev/console) 2>&1

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  build-essential ca-certificates clang cmake curl git jq libclang-dev \
  libssl-dev linux-tools-common "linux-tools-$(uname -r)" pkg-config xfsprogs
systemctl disable --now apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service || true

printf '%s\n' 'kernel.perf_event_paranoid = -1' \
  > /etc/sysctl.d/99-hyphae-g7-perf.conf
sysctl -w kernel.perf_event_paranoid=-1
test "$(sysctl -n kernel.perf_event_paranoid)" = "-1"
perf_output="$(sudo -u ubuntu mktemp)"
sudo -u ubuntu perf stat --no-big-num -x ';' \
  -e cycles,cache-misses,minor-faults,major-faults \
  -o "$perf_output" -- \
  python3 -c 'sum(index * index for index in range(1000000))'
python3 - "$perf_output" <<'PY'
import sys
from pathlib import Path

expected = {"cycles", "cache-misses", "minor-faults", "major-faults"}
measured = {}
for line in Path(sys.argv[1]).read_text(encoding="utf-8").splitlines():
    fields = line.split(";")
    if len(fields) < 3 or fields[2].strip() not in expected:
        continue
    raw = fields[0].strip()
    if raw.startswith("<"):
        raise SystemExit(f"perf event unavailable: {fields[2].strip()}={raw}")
    measured[fields[2].strip()] = int(raw)
if set(measured) != expected or measured["cycles"] <= 0:
    raise SystemExit(f"perf event preflight incomplete: {measured}")
PY
rm -f "$perf_output"

for governor in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
  if [[ -w "$governor" ]]; then
    printf '%s\n' performance > "$governor"
  fi
done

data_device=""
for candidate in /dev/nvme*n1; do
  [[ -b "$candidate" ]] || continue
  block="$(basename "$candidate")"
  model="$(tr -d ' ' < "/sys/block/$block/device/model" 2>/dev/null || true)"
  if [[ "$model" == AmazonEC2NVMeInstanceStorage ]]; then
    data_device="$candidate"
    break
  fi
done
[[ -n "$data_device" ]]
mkfs.xfs -f "$data_device"
install -d -m 0755 /mnt/hyphae-g7
mount -o noatime,nodiratime "$data_device" /mnt/hyphae-g7
printf '%s /mnt/hyphae-g7 xfs noatime,nodiratime,nofail 0 2\n' \
  "UUID=$(blkid -s UUID -o value "$data_device")" >> /etc/fstab
chown ubuntu:ubuntu /mnt/hyphae-g7

install -d -m 0755 /etc/hyphae
cpu_model="$(lscpu -J | jq -r '.lscpu[] | select(.field == "Model name:") | .data')"
socket_count="$(lscpu -p=Socket | grep -v '^#' | sort -u | wc -l | tr -d ' ')"
physical_cores="$(lscpu -p=Socket,Core | grep -v '^#' | sort -u | wc -l | tr -d ' ')"
logical_processors="$(nproc)"
ram_bytes="$(awk '/MemTotal:/ {printf "%.0f\\n", $2 * 1024}' /proc/meminfo)"
affinity="$(taskset -pc $$ | sed 's/.*: //')"
storage_model="$(xargs < "/sys/block/$(basename "$data_device")/device/model")"
jq -n \
  --arg cpu "$cpu_model" \
  --arg topology "$socket_count sockets / $physical_cores physical cores / $logical_processors logical processors" \
  --argjson ram_bytes "$ram_bytes" \
  --arg storage "$storage_model" \
  --arg affinity "$affinity" \
  '{
    dedicated: true,
    cpu: $cpu,
    topology: $topology,
    ram_bytes: $ram_bytes,
    storage: $storage,
    filesystem: "xfs",
    governor: "performance",
    affinity: $affinity,
    priority: "normal",
    background_services: "disabled",
    virtualization: "none"
  }' > /etc/hyphae/g7-hardware.json

install -d -o ubuntu -g ubuntu -m 0755 /opt/actions-runner
curl -fsSLo /tmp/actions-runner.tar.gz \
  https://github.com/actions/runner/releases/download/v2.336.0/actions-runner-linux-x64-2.336.0.tar.gz
printf '%s  %s\n' \
  '04cf0be1aff4c3ec3554466c39124ca250e3effd8873bb7e8d68535aa9505d5d' \
  /tmp/actions-runner.tar.gz | sha256sum --check --strict
tar -xzf /tmp/actions-runner.tar.gz -C /opt/actions-runner
rm -f /tmp/actions-runner.tar.gz
chown -R ubuntu:ubuntu /opt/actions-runner

sudo -u ubuntu /opt/actions-runner/config.sh \
  --unattended \
  --url https://github.com/celiumsai/hyphae \
  --token __RUNNER_TOKEN__ \
  --name __RUNNER_NAME__ \
  --labels hyphae-g7,dedicated \
  --ephemeral \
  --work _work
cd /opt/actions-runner
./svc.sh install ubuntu
./svc.sh start
