#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed semantic checker for Native hardware profile v1."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


SCHEMA = "hyphae-native-hardware-profile-v1"
DIGEST = re.compile(r"^[0-9a-f]{64}$")
CPU_LIST = re.compile(r"^[0-9]+(?:-[0-9]+)?(?:,[0-9]+(?:-[0-9]+)?)*$")


class HardwareProfileValidationError(ValueError):
    """A hardware profile violates its frozen semantic contract."""


def fail(message: str) -> None:
    raise HardwareProfileValidationError(message)


def require_object(value: Any, field: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{field} must be an object")
    actual = set(value)
    if actual != keys:
        fail(f"{field} keys differ: missing={sorted(keys - actual)} extra={sorted(actual - keys)}")
    return value


def require_integer(value: Any, field: str, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{field} must be an integer >= {minimum}")
    return value


def require_optional_integer(value: Any, field: str, minimum: int = 0) -> int | None:
    if value is None:
        return None
    return require_integer(value, field, minimum)


def require_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        fail(f"{field} must be a non-empty string")
    return value


def require_unique_strings(value: Any, field: str) -> list[str]:
    if not isinstance(value, list):
        fail(f"{field} must be an array")
    strings = [require_string(item, f"{field}[{index}]") for index, item in enumerate(value)]
    if len(strings) != len(set(strings)):
        fail(f"{field} contains duplicates")
    return strings


def parse_cpu_list(value: str, field: str) -> set[int]:
    if CPU_LIST.fullmatch(value) is None:
        fail(f"{field} is not a canonical CPU list")
    cpus: set[int] = set()
    for component in value.split(","):
        if "-" in component:
            start_text, end_text = component.split("-", 1)
            start, end = int(start_text), int(end_text)
            if start >= end:
                fail(f"{field} has a non-increasing range")
            values = range(start, end + 1)
        else:
            values = (int(component),)
        for cpu in values:
            if cpu in cpus:
                fail(f"{field} contains overlapping processors")
            cpus.add(cpu)
    if format_cpu_list(cpus) != value:
        fail(f"{field} is not canonically ordered")
    return cpus


def format_cpu_list(cpus: set[int]) -> str:
    ordered = sorted(cpus)
    ranges: list[tuple[int, int]] = []
    for cpu in ordered:
        if not ranges or cpu != ranges[-1][1] + 1:
            ranges.append((cpu, cpu))
        else:
            ranges[-1] = (ranges[-1][0], cpu)
    return ",".join(str(start) if start == end else f"{start}-{end}" for start, end in ranges)


def validate_cpu(value: Any) -> tuple[dict[int, dict[str, Any]], set[int]]:
    cpu = require_object(
        value,
        "cpu",
        {
            "architecture",
            "logical_processors_available",
            "physical_cores_visible",
            "smt_threads_per_core",
            "sockets_visible",
            "numa_nodes_visible",
            "affinity",
            "quota_millicores",
            "instruction_sets",
            "caches",
            "processor_topology",
            "frequency_governors",
        },
    )
    require_string(cpu["architecture"], "cpu.architecture")
    require_integer(cpu["logical_processors_available"], "cpu.logical_processors_available", 1)
    physical = require_optional_integer(cpu["physical_cores_visible"], "cpu.physical_cores_visible", 1)
    require_optional_integer(cpu["smt_threads_per_core"], "cpu.smt_threads_per_core", 1)
    sockets = require_optional_integer(cpu["sockets_visible"], "cpu.sockets_visible", 1)
    require_optional_integer(cpu["numa_nodes_visible"], "cpu.numa_nodes_visible", 1)
    affinity_text = require_string(cpu["affinity"], "cpu.affinity")
    affinity = parse_cpu_list(affinity_text, "cpu.affinity") if CPU_LIST.fullmatch(affinity_text) else set()
    require_optional_integer(cpu["quota_millicores"], "cpu.quota_millicores", 1)
    require_unique_strings(cpu["instruction_sets"], "cpu.instruction_sets")
    require_unique_strings(cpu["frequency_governors"], "cpu.frequency_governors")

    caches = cpu["caches"]
    if not isinstance(caches, list):
        fail("cpu.caches must be an array")
    cache_keys = []
    for index, raw in enumerate(caches):
        cache = require_object(
            raw,
            f"cpu.caches[{index}]",
            {"level", "kind", "size_bytes", "line_size_bytes", "shared_cpu_list"},
        )
        key = (
            require_integer(cache["level"], f"cpu.caches[{index}].level", 1),
            require_string(cache["kind"], f"cpu.caches[{index}].kind"),
            require_integer(cache["size_bytes"], f"cpu.caches[{index}].size_bytes", 1),
            require_optional_integer(
                cache["line_size_bytes"], f"cpu.caches[{index}].line_size_bytes", 1
            ),
            require_string(cache["shared_cpu_list"], f"cpu.caches[{index}].shared_cpu_list"),
        )
        cache_keys.append(key)
    if len(cache_keys) != len(set(cache_keys)):
        fail("cpu.caches contains duplicate domains")

    topology = cpu["processor_topology"]
    if not isinstance(topology, list):
        fail("cpu.processor_topology must be an array")
    processors: dict[int, dict[str, Any]] = {}
    logical_order: list[int] = []
    for index, raw in enumerate(topology):
        processor = require_object(
            raw,
            f"cpu.processor_topology[{index}]",
            {"logical_id", "core_id", "socket_id", "numa_node_id", "thread_siblings"},
        )
        logical = require_integer(processor["logical_id"], f"cpu.processor_topology[{index}].logical_id")
        require_integer(processor["core_id"], f"cpu.processor_topology[{index}].core_id")
        require_integer(processor["socket_id"], f"cpu.processor_topology[{index}].socket_id")
        require_optional_integer(
            processor["numa_node_id"], f"cpu.processor_topology[{index}].numa_node_id"
        )
        parse_cpu_list(processor["thread_siblings"], f"cpu.processor_topology[{index}].thread_siblings")
        if logical in processors:
            fail("cpu.processor_topology repeats a logical processor")
        processors[logical] = processor
        logical_order.append(logical)
    if logical_order != sorted(logical_order):
        fail("cpu.processor_topology must be ordered by logical_id")
    if affinity and set(processors) != affinity:
        fail("cpu.processor_topology differs from process affinity")
    if processors:
        core_count = len({(item["socket_id"], item["core_id"]) for item in processors.values()})
        socket_count = len({item["socket_id"] for item in processors.values()})
        if physical != core_count or sockets != socket_count:
            fail("CPU core or socket counts disagree with processor_topology")
        for logical, processor in processors.items():
            siblings = parse_cpu_list(processor["thread_siblings"], "processor.thread_siblings")
            if logical not in siblings or not siblings.issubset(processors):
                fail("processor thread_siblings is incomplete or references an invisible CPU")
            placement = (processor["socket_id"], processor["core_id"])
            if any(
                (processors[sibling]["socket_id"], processors[sibling]["core_id"]) != placement
                for sibling in siblings
            ):
                fail("processor thread_siblings crosses a physical core")
    return processors, affinity


def validate_memory(value: Any, processors: dict[int, dict[str, Any]]) -> list[dict[str, Any]]:
    memory = require_object(
        value,
        "memory",
        {
            "total_bytes",
            "available_bytes",
            "page_size_bytes",
            "huge_page_size_bytes",
            "huge_pages_total",
            "numa_nodes",
        },
    )
    total = require_optional_integer(memory["total_bytes"], "memory.total_bytes", 1)
    available = require_optional_integer(memory["available_bytes"], "memory.available_bytes")
    if total is not None and available is not None and available > total:
        fail("memory.available_bytes exceeds total_bytes")
    require_optional_integer(memory["page_size_bytes"], "memory.page_size_bytes", 1)
    require_optional_integer(memory["huge_page_size_bytes"], "memory.huge_page_size_bytes", 1)
    require_optional_integer(memory["huge_pages_total"], "memory.huge_pages_total")
    nodes = memory["numa_nodes"]
    if not isinstance(nodes, list):
        fail("memory.numa_nodes must be an array")
    ids: list[int] = []
    seen_cpus: set[int] = set()
    for index, raw in enumerate(nodes):
        node = require_object(
            raw,
            f"memory.numa_nodes[{index}]",
            {"id", "cpu_list", "total_bytes", "available_bytes"},
        )
        node_id = require_integer(node["id"], f"memory.numa_nodes[{index}].id")
        node_cpus = parse_cpu_list(node["cpu_list"], f"memory.numa_nodes[{index}].cpu_list")
        if seen_cpus & node_cpus:
            fail("memory.numa_nodes contains overlapping CPU lists")
        seen_cpus |= node_cpus
        require_optional_integer(node["total_bytes"], f"memory.numa_nodes[{index}].total_bytes", 1)
        require_optional_integer(node["available_bytes"], f"memory.numa_nodes[{index}].available_bytes")
        ids.append(node_id)
    if ids != sorted(set(ids)):
        fail("memory.numa_nodes must have unique ordered IDs")
    if processors:
        placement = {logical: processor["numa_node_id"] for logical, processor in processors.items()}
        for node in nodes:
            for cpu in parse_cpu_list(node["cpu_list"], "memory.numa_nodes.cpu_list"):
                if cpu not in processors or placement[cpu] != node["id"]:
                    fail("NUMA CPU lists disagree with processor_topology")
    return nodes


def validate_profile(profile: Any) -> None:
    root = require_object(
        profile,
        "profile",
        {"schema", "fingerprint", "cpu", "memory", "storage", "operating_system"},
    )
    if root["schema"] != SCHEMA:
        fail("profile schema is not Native hardware profile v1")
    fingerprint = require_string(root["fingerprint"], "fingerprint")
    if DIGEST.fullmatch(fingerprint) is None:
        fail("fingerprint must be a lowercase BLAKE3 digest")
    processors, _ = validate_cpu(root["cpu"])
    nodes = validate_memory(root["memory"], processors)
    visible_nodes = root["cpu"]["numa_nodes_visible"]
    if nodes and visible_nodes != len(nodes):
        fail("cpu.numa_nodes_visible disagrees with memory.numa_nodes")

    storage = require_object(
        root["storage"],
        "storage",
        {
            "path",
            "filesystem",
            "device",
            "mount_options",
            "rotational",
            "queue_depth",
            "discard_max_bytes",
        },
    )
    require_string(storage["path"], "storage.path")
    for field in ("filesystem", "device"):
        if storage[field] is not None:
            require_string(storage[field], f"storage.{field}")
    require_unique_strings(storage["mount_options"], "storage.mount_options")
    if storage["rotational"] is not None and not isinstance(storage["rotational"], bool):
        fail("storage.rotational must be boolean or null")
    require_optional_integer(storage["queue_depth"], "storage.queue_depth")
    require_optional_integer(storage["discard_max_bytes"], "storage.discard_max_bytes")

    operating_system = require_object(
        root["operating_system"],
        "operating_system",
        {"family", "kernel_release", "virtualization", "local_transports"},
    )
    require_string(operating_system["family"], "operating_system.family")
    require_string(operating_system["kernel_release"], "operating_system.kernel_release")
    require_string(operating_system["virtualization"], "operating_system.virtualization")
    require_unique_strings(operating_system["local_transports"], "operating_system.local_transports")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", required=True, type=Path)
    args = parser.parse_args()
    try:
        with args.profile.open(encoding="utf-8") as handle:
            validate_profile(json.load(handle))
    except (OSError, json.JSONDecodeError, HardwareProfileValidationError) as error:
        print(f"native hardware profile check failed: {error}")
        return 1
    print(f"native hardware profile check passed: {args.profile}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
