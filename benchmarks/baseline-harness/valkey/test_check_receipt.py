#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import hashlib
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

import check_profile
import check_receipt


def summary(label: str, operations: int) -> dict:
    return {
        "label": label,
        "operations": operations,
        "wall_nanos": 100,
        "ops_per_second": operations * 1_000_000_000.0 / 100,
        "latency_nanos": {
            "total": operations,
            "mean": 1,
            "p50": 1,
            "p95": 2,
            "p99": 2,
            "p999": 2,
            "max": 2,
        },
    }


def storage(model: str = "unqualified") -> dict:
    return {
        "model": model,
        "device": "259:7" if model != "unqualified" else "unqualified",
        "filesystem": "xfs" if model != "unqualified" else "unqualified",
        "rotational": False if model != "unqualified" else None,
        "queue_depth": 1023 if model != "unqualified" else None,
    }


class SavedReceiptTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.profile = json.loads(check_profile.DEFAULT_PROFILE.read_text(encoding="utf-8"))

    def external_lane(self, lane: dict, sequence: str) -> dict:
        effective = {
            "appendonly": lane["appendonly"],
            "appendfsync": lane["appendfsync"],
            "port": "0",
            "unixsocket": lane["socket"],
            "maxmemory-policy": "noeviction",
            "maxmemory": "0",
            "databases": "1",
            "save": "",
            "protected-mode": "yes",
            "daemonize": "yes",
            "supervised": "no",
            "dir": lane["dir"],
        }
        source_file = f"/root/hyphae/{lane['config']}"
        executable = "/tmp/hyphae-valkey/src/valkey-server"
        writes = 3 if lane["name"] == "always" else 5
        process_id = {"no": "40", "always": "41", "everysec": "42"}[lane["name"]]
        run_id = {"no": "7", "always": "8", "everysec": "9"}[lane["name"]] * 40
        reads = check_receipt.read_key_sequence_sha256(1, 11, 7)
        return {
            "schema": check_receipt.VALKEY_RECEIPT_SCHEMA,
            "engine": "valkey-external",
            "lane": lane["name"],
            "version": check_profile.EXPECTED_VERSION,
            "authority": {
                "application_core": {
                    "schema": self.profile["schema"],
                    "profile": self.profile["profile"],
                    "profile_version": self.profile["profile_version"],
                    "status": self.profile["status"],
                    "foundation_only": self.profile["foundation_only"],
                    "claim_semantics_sha256": self.profile["claim_semantics_seal"]["sha256"],
                    "profile_sha256": check_profile.EXPECTED_PROFILE_SHA256,
                },
                "oracle_source_commit": self.profile["oracle"]["source_commit"],
                "hyphae_semantic_authority": self.profile["hyphae_authority"],
                "executed_source_binding": {
                    "policy": self.profile["executed_source_binding"]["policy"],
                    "source_commit": "a" * 40,
                    "source_tree": "b" * 40,
                    "semantic_bundle_sha256": self.profile["executed_source_binding"][
                        "semantic_bundle_sha256"
                    ],
                },
            },
            "external_identity": {
                "source_archive": {
                    "url": self.profile["oracle"]["artifact"]["url"],
                    "sha256": self.profile["oracle"]["artifact"]["sha256"],
                },
                "build": {
                    "compiler": "cc 15.2.0",
                    "flags": "BUILD_TLS=no;MALLOC=jemalloc;OPTIMIZATION=-O3;CC=cc;CFLAGS=;LDFLAGS=",
                    "server_binary": executable,
                    "runner_expected_server_binary_sha256": "6" * 64,
                    "executed_server_binary_sha256": "6" * 64,
                    "retained_server_artifact": "/tmp/retained-valkey-server",
                    "retained_server_artifact_sha256": "6" * 64,
                },
                "configuration": {
                    "source_file": source_file,
                    "source_sha256": lane["sha256"],
                    "effective_sha256": check_receipt.effective_config_sha256(effective),
                    "effective": effective,
                },
                "runtime": {
                    "server_name": "valkey",
                    "redis_version": check_profile.EXPECTED_VERSION,
                    "valkey_version": check_profile.EXPECTED_VERSION,
                    "valkey_release_stage": "ga",
                    "redis_git_sha1": "00000000",
                    "redis_git_dirty": "0",
                    "redis_build_id": "build-1",
                    "server_mode": "standalone",
                    "os": "Linux",
                    "arch_bits": "64",
                    "gcc_version": "15.2.0",
                    "process_id": process_id,
                    "process_supervised": "no",
                    "run_id": run_id,
                    "tcp_port": "0",
                    "executable": executable,
                    "config_file": source_file,
                },
            },
            "setup_identity": {
                "state": "fresh-recreated-directory-and-server",
                "setup_id": "8" * 64,
                "data_directory": lane["dir"],
                "marker_sha256": check_receipt.setup_marker_sha256("8" * 64, lane["name"]),
                "initial_dbsize": 0,
                "loaded_dbsize": 11,
                "dataset_sha256": check_receipt.initial_dataset_sha256(11),
                "process_id": process_id,
                "started_pid": process_id,
                "run_id": run_id,
            },
            "transport": "unix domain socket",
            "persistence": {
                "appendonly": lane["appendonly"],
                "appendfsync": lane["appendfsync"],
            },
            "connection_alive": True,
            "get_hits": 7,
            "get": summary("get_uds", 7),
            "read_key_sequence_sha256": reads,
            "write_key_sequence_sha256": sequence,
            "set": summary("set_uds", writes),
        }

    def valid_receipt(self) -> dict:
        lanes = {lane["name"]: lane for lane in self.profile["configuration_authority"]["lanes"]}
        always_sequence = check_receipt.write_key_sequence_sha256(
            1 ^ check_receipt.ALWAYS_WRITE_DOMAIN, 11, 3
        )
        no_sequence = check_receipt.write_key_sequence_sha256(
            1 ^ check_receipt.NO_WRITE_DOMAIN, 11, 5
        )
        dataset = check_receipt.initial_dataset_sha256(11)
        reads = check_receipt.read_key_sequence_sha256(1, 11, 7)
        return {
            "schema": check_receipt.RECEIPT_SCHEMA,
            "evidence_authority": {
                "status": "non-authoritative-diagnostic",
                "hardware_qualification": "test-fixture",
                "storage": storage(),
            },
            "hyphae_execution": {
                "source": {
                    "pre_build": {
                        "commit": "a" * 40,
                        "tree": "b" * 40,
                        "worktree_clean": True,
                    },
                    "post_build": {
                        "commit": "a" * 40,
                        "tree": "b" * 40,
                        "worktree_clean": True,
                    },
                    "stable_during_build": True,
                },
                "build": {
                    "rustc": "rustc 1.96.0",
                    "rustc_verbose": "rustc 1.96.0\nrelease: 1.96.0\nbinary: rustc",
                    "cargo": "cargo 1.96.0",
                    "profile": "release",
                    "rustflags": "empty",
                    "cargo_encoded_rustflags": "empty",
                    "rustc_wrapper": "unset",
                    "rustc_workspace_wrapper": "unset",
                    "cargo_profile_overrides": "absent",
                    "cargo_incremental": "disabled",
                    "command": "cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml",
                    "executable": "/root/hyphae/benchmarks/baseline-harness/target/release/hyphae-baseline-harness",
                    "runner_expected_sha256": "c" * 64,
                    "executed_harness_product_sha256": "c" * 64,
                    "embedded_product": True,
                },
            },
            "environment": {
                "os": "linux",
                "arch": "x86_64",
                "cpu_model": "test",
                "logical_cpus": 96,
                "memory_total_kib": 768 * 1024 * 1024,
                "cpu_topology": {
                    "logical_cpus": 96,
                    "physical_cores": 48,
                    "sockets": 2,
                    "threads_per_core": 2,
                    "logical_topology_entries": 96,
                    "complete_smt_siblings": True,
                },
                "cpu_affinity": "0-95",
                "cpu_quota": {"state": "unlimited", "millicores": None},
                "kernel": "test",
                "hardware_product_name": "i7i.metal-24xl",
                "benchmark_storage_source": "/dev/nvme1n1",
                "scaling_governor": "performance",
                "scaling_governors": ["performance"],
                "scaling_governor_cpu_count": 96,
                "hypervisor_flag": False,
            },
            "results": {
                "keyspace": {
                    "workload": {"keys": 11, "gets": 7, "strict_sets": 3, "relaxed_sets": 5, "seed": 1},
                    "hyphae": {
                        "engine": "hyphae-native-embedded",
                        "transport": "none (in-process library call)",
                        "always_comparison": {
                            "lane": "always",
                            "durability": "strict",
                            "persistence_acknowledgement": "fsync_per_commit",
                            "setup_identity": {
                                "state": "fresh-created-database",
                                "data_directory": "/mnt/nvme/hyphae-always",
                                "initial_probe_absent": True,
                                "loaded_probe_matches": True,
                                "loaded_keys": 11,
                                "dataset_sha256": dataset,
                            },
                            "get_hits": 7,
                            "get": summary("get_latest", 7),
                            "read_key_sequence_sha256": reads,
                            "write_key_sequence_sha256": always_sequence,
                            "set": summary("set_strict_fsync_per_commit", 3),
                        },
                        "no_comparison": {
                            "lane": "no",
                            "durability": "memory",
                            "persistence_acknowledgement": "none",
                            "setup_identity": {
                                "state": "fresh-created-database",
                                "data_directory": "/mnt/nvme/hyphae-no",
                                "initial_probe_absent": True,
                                "loaded_probe_matches": True,
                                "loaded_keys": 11,
                                "dataset_sha256": dataset,
                            },
                            "get_hits": 7,
                            "get": summary("get_latest", 7),
                            "read_key_sequence_sha256": reads,
                            "write_key_sequence_sha256": no_sequence,
                            "set": summary("set_memory_no_fsync_ack", 5),
                        },
                    },
                    "valkey_no": self.external_lane(lanes["no"], no_sequence),
                    "valkey_always": self.external_lane(lanes["always"], always_sequence),
                    "valkey_everysec": self.external_lane(lanes["everysec"], no_sequence),
                }
            },
        }

    def assert_tamper_rejected(self, mutate, expected: str) -> None:
        receipt = copy.deepcopy(self.valid_receipt())
        mutate(receipt)
        failures = check_receipt.validate_receipt(
            receipt,
            self.profile,
            expected_source_commit="a" * 40,
            expected_source_tree="b" * 40,
            expected_hyphae_binary_sha256="c" * 64,
        )
        self.assertTrue(any(expected in failure for failure in failures), failures)

    def test_complete_saved_receipt_is_valid(self) -> None:
        receipt = self.valid_receipt()
        self.assertEqual(check_receipt.validate_receipt(receipt, self.profile), [])

    def test_rejects_incomplete_or_extended_schema(self) -> None:
        self.assert_tamper_rejected(lambda value: value.pop("environment"), "receipt fields differ")
        self.assert_tamper_rejected(lambda value: value.update(unreviewed=True), "receipt fields differ")

    def test_rejects_hyphae_source_and_executable_tampering(self) -> None:
        self.assert_tamper_rejected(
            lambda value: value["hyphae_execution"]["source"]["post_build"].update(
                worktree_clean=False
            ),
            "worktree_clean must be true",
        )
        self.assert_tamper_rejected(
            lambda value: value["hyphae_execution"]["source"]["post_build"].update(
                commit="d" * 40
            ),
            "source commit differs from expected source",
        )
        self.assert_tamper_rejected(
            lambda value: value["hyphae_execution"]["build"].update(runner_expected_sha256="d" * 64),
            "Hyphae harness/product binary identity differs",
        )
        self.assert_tamper_rejected(
            lambda value: value["hyphae_execution"]["build"].update(
                profile="debug", rustflags="-C target-cpu=native"
            ),
            "sanitized build inputs differ",
        )

    def test_rejects_profile_and_semantic_authority_tampering(self) -> None:
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["authority"][
                "application_core"
            ].update(profile_sha256="d" * 64),
            "authority.application_core differs",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["authority"][
                "hyphae_semantic_authority"
            ].update(source_commit="d" * 40),
            "hyphae_semantic_authority differs",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["authority"][
                "executed_source_binding"
            ].update(semantic_bundle_sha256="d" * 64),
            "executed_source_binding differs",
        )

    def test_non_authoritative_receipts_are_explicit_and_cannot_be_promoted(self) -> None:
        receipt = self.valid_receipt()
        receipt["evidence_authority"] = {
            "status": "non-authoritative-diagnostic",
            "hardware_qualification": "local-unqualified",
            "storage": storage(),
        }
        self.assertEqual(check_receipt.validate_receipt(receipt, self.profile), [])
        failures = check_receipt.validate_receipt(
            receipt, self.profile, require_authoritative=True
        )
        self.assertTrue(any("non-authoritative" in failure for failure in failures))

    def test_authoritative_receipts_require_exact_production_workload(self) -> None:
        receipt = self.valid_receipt()
        receipt["evidence_authority"] = {
            "status": "authoritative-dedicated-hardware",
            "hardware_qualification": "aws-ec2-i7i.metal-24xl",
            "storage": storage("Amazon EC2 NVMe Instance Storage"),
        }
        failures = check_receipt.validate_receipt(receipt, self.profile)
        self.assertTrue(any("exact production workload" in failure for failure in failures))

    def test_authoritative_receipts_require_complete_metal_identity(self) -> None:
        def authoritative(value) -> None:
            value["evidence_authority"] = {
                "status": "authoritative-dedicated-hardware",
                "hardware_qualification": "aws-ec2-i7i.metal-24xl",
                "storage": storage("Amazon EC2 NVMe Instance Storage"),
            }

        mutations = [
            (lambda value: value["environment"].update(os="Linux"), "normalized linux"),
            (lambda value: value["environment"].update(arch="amd64"), "normalized x86_64"),
            (lambda value: value["environment"].update(hypervisor_flag=True), "hypervisor"),
            (lambda value: value["environment"].update(cpu_model=None), "cpu_model"),
            (lambda value: value["environment"].update(kernel=None), "kernel"),
            (lambda value: value["environment"].update(memory_total_kib=None), "memory_total_kib"),
            (
                lambda value: value["environment"].update(
                    memory_total_kib=700 * 1024 * 1024
                ),
                "memory qualification",
            ),
            (lambda value: value["environment"].update(cpu_topology=None), "cpu_topology"),
            (
                lambda value: value["environment"]["cpu_topology"].update(
                    physical_cores=47
                ),
                "physical core count",
            ),
            (
                lambda value: value["environment"]["cpu_topology"].update(
                    threads_per_core=1
                ),
                "SMT width",
            ),
            (lambda value: value["environment"].update(cpu_affinity="0-94"), "affinity"),
            (
                lambda value: value["environment"].update(
                    cpu_quota={"state": "limited", "millicores": 96_000}
                ),
                "quota",
            ),
            (
                lambda value: value["environment"].update(
                    scaling_governors=["powersave"], scaling_governor_cpu_count=1
                ),
                "performance governors",
            ),
            (
                lambda value: value["evidence_authority"]["storage"].update(
                    model="Amazon Elastic Block Store"
                ),
                "hardware qualification differs",
            ),
            (
                lambda value: value["evidence_authority"]["storage"].update(device="8:0"),
                "not NVMe",
            ),
            (
                lambda value: value["evidence_authority"]["storage"].update(rotational=True),
                "non-rotational",
            ),
            (
                lambda value: value["evidence_authority"]["storage"].update(queue_depth=0),
                "queue_depth",
            ),
        ]
        for mutate, expected in mutations:
            with self.subTest(expected=expected):
                receipt = self.valid_receipt()
                authoritative(receipt)
                mutate(receipt)
                failures = check_receipt.validate_receipt(receipt, self.profile)
                self.assertTrue(any(expected in failure for failure in failures), failures)

    def test_rejects_reused_valkey_process_or_run_identity(self) -> None:
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"][
                "external_identity"
            ]["runtime"].update(process_id="41"),
            "distinct process_id",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"][
                "external_identity"
            ]["runtime"].update(run_id="8" * 40),
            "distinct run_id",
        )
        def alias_pid(value) -> None:
            lane = value["results"]["keyspace"]["valkey_no"]
            lane["external_identity"]["runtime"]["process_id"] = "040"
            lane["setup_identity"]["process_id"] = "040"
            lane["setup_identity"]["started_pid"] = "040"

        self.assert_tamper_rejected(alias_pid, "runtime.process_id is invalid")

    def test_rejects_external_source_build_and_binary_tampering(self) -> None:
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["external_identity"]["source_archive"].update(sha256="d" * 64),
            "source_archive differs",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["external_identity"]["build"].update(flags="BUILD_TLS=yes"),
            "flags omits BUILD_TLS=no",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["external_identity"]["build"].update(executed_server_binary_sha256="d" * 64),
            "binary digests differ",
        )

    def test_validator_hashes_the_retained_valkey_artifact(self) -> None:
        receipt = self.valid_receipt()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "valkey-server"
            artifact.write_bytes(b"retained-valkey-binary")
            digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
            for name in ("no", "always", "everysec"):
                build = receipt["results"]["keyspace"][f"valkey_{name}"][
                    "external_identity"
                ]["build"]
                build["runner_expected_server_binary_sha256"] = digest
                build["executed_server_binary_sha256"] = digest
                build["retained_server_artifact"] = str(artifact)
                build["retained_server_artifact_sha256"] = digest
            receipt_path = root / "receipt.json"
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            check_receipt.check_receipt(
                receipt_path, valkey_binary_artifact=artifact
            )
            artifact.write_bytes(b"tampered-valkey-binary")
            with self.assertRaisesRegex(
                check_receipt.ReceiptError, "differs from retained Valkey artifact"
            ):
                check_receipt.check_receipt(
                    receipt_path, valkey_binary_artifact=artifact
                )

    def test_authoritative_receipt_cannot_bypass_source_root_without_flag(self) -> None:
        receipt = self.valid_receipt()
        receipt["evidence_authority"] = {
            "status": "authoritative-dedicated-hardware",
            "hardware_qualification": "aws-ec2-i7i.metal-24xl",
            "storage": storage("Amazon EC2 NVMe Instance Storage"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "valkey-server"
            artifact.write_bytes(b"retained-valkey-binary")
            receipt_path = root / "receipt.json"
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            with self.assertRaisesRegex(
                check_receipt.ReceiptError, "authoritative validation requires --source-root"
            ):
                check_receipt.check_receipt(
                    receipt_path,
                    valkey_binary_artifact=artifact,
                    require_authoritative=False,
                )

    def test_validator_verifies_commit_tree_cleanliness_and_committed_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(
                ["git", "-C", str(root), "config", "user.name", "Receipt Test"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(root), "config", "user.email", "receipt@example.invalid"],
                check=True,
            )
            authority = root / "authority"
            authority.write_bytes(b"committed authority")
            subprocess.run(["git", "-C", str(root), "add", "authority"], check=True)
            subprocess.run(
                ["git", "-C", str(root), "commit", "-q", "-m", "fixture"], check=True
            )
            commit = subprocess.run(
                ["git", "-C", str(root), "rev-parse", "HEAD"],
                check=True,
                stdout=subprocess.PIPE,
                text=True,
            ).stdout.strip()
            tree = subprocess.run(
                ["git", "-C", str(root), "rev-parse", "HEAD^{tree}"],
                check=True,
                stdout=subprocess.PIPE,
                text=True,
            ).stdout.strip()
            self.assertEqual(
                check_receipt.verify_committed_paths(root, commit, tree, ["authority"]), []
            )
            authority.write_bytes(b"substituted authority")
            failures = check_receipt.verify_committed_paths(
                root, commit, tree, ["authority"]
            )
            self.assertTrue(any("not clean" in failure for failure in failures))
            self.assertTrue(any("bytes differ" in failure for failure in failures))
            failures = check_receipt.verify_committed_paths(
                root, commit, "0" * 40, ["authority"]
            )
            self.assertTrue(any("committed Git tree" in failure for failure in failures))

    def test_rejects_config_and_effective_config_tampering(self) -> None:
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_always"]["external_identity"]["configuration"].update(source_sha256="d" * 64),
            "source digest differs",
        )

        def mutate_effective(value) -> None:
            configuration = value["results"]["keyspace"]["valkey_always"]["external_identity"]["configuration"]
            configuration["effective"]["appendfsync"] = "everysec"
            configuration["effective_sha256"] = check_receipt.effective_config_sha256(configuration["effective"])

        self.assert_tamper_rejected(mutate_effective, "effective appendfsync differs")

    def test_rejects_key_sequence_and_setup_tampering(self) -> None:
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"].update(write_key_sequence_sha256="d" * 64),
            "Memory and Valkey no write-key sequences differ",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["setup_identity"].update(initial_dbsize=1),
            "cardinality proof differs",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"].update(
                read_key_sequence_sha256="d" * 64
            ),
            "read-key sequences differ",
        )

    def test_rejects_nonfinite_throughput_and_invalid_percentiles(self) -> None:
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["get"].update(
                ops_per_second=float("nan")
            ),
            "must be finite",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["get"].update(
                ops_per_second=10**1000
            ),
            "must be finite",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["get"].update(
                ops_per_second=1.0
            ),
            "differs from operations/wall_nanos",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["get"][
                "latency_nanos"
            ].update(p50=3, p95=2),
            "percentile ordering differs",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["get"][
                "latency_nanos"
            ].update(max=101),
            "max exceeds wall_nanos",
        )
        self.assert_tamper_rejected(
            lambda value: value["results"]["keyspace"]["valkey_no"]["get"][
                "latency_nanos"
            ].update(total=14),
            "mean/total rounding differs",
        )

    def test_malformed_types_return_failures_without_exceptions(self) -> None:
        mutations = [
            lambda value: value["results"]["keyspace"].update(workload="bad"),
            lambda value: value.update(hyphae_execution=[]),
            lambda value: value["results"]["keyspace"]["valkey_no"].update(
                external_identity=[]
            ),
            lambda value: value["results"]["keyspace"]["valkey_no"]["get"].update(
                latency_nanos="bad"
            ),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                receipt = copy.deepcopy(self.valid_receipt())
                mutate(receipt)
                failures = check_receipt.validate_receipt(receipt, self.profile)
                self.assertTrue(failures)

    def test_rejects_duplicate_json_keys(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "duplicate.json"
            path.write_text('{"schema":"one","schema":"two"}', encoding="utf-8")
            with self.assertRaisesRegex(check_receipt.ReceiptError, "duplicate JSON key"):
                check_receipt.load_receipt(path)

    def test_rejects_nonfinite_json_numbers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            for constant in ("NaN", "Infinity", "-Infinity"):
                path = Path(directory) / f"{constant}.json"
                path.write_text(f'{{"value":{constant}}}', encoding="utf-8")
                with self.assertRaisesRegex(check_receipt.ReceiptError, "non-finite"):
                    check_receipt.load_receipt(path)


if __name__ == "__main__":
    unittest.main()
