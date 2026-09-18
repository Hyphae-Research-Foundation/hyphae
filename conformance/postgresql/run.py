#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Run the pinned PostgreSQL Application Core v1 oracle in an isolated container."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import platform
import re
import stat
import subprocess
import sys
import time
import uuid
from pathlib import Path
from urllib.parse import urlencode
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[2]
PROFILE_PATH = Path(__file__).with_name("application-core-v1.json")
sys.path.insert(0, str(ROOT))

from tools.check_postgresql_application_core import (  # noqa: E402
    CASES_DIRECTORY,
    GateFailure,
    resolve_contained_file,
    validate_profile,
)
from tools.check_postgresql_application_core_receipt import (  # noqa: E402
    INDEX_MEDIA_TYPES,
    MANIFEST_MEDIA_TYPES,
    MAX_OCI_DOCUMENT_BYTES,
    RECEIPT_EVIDENCE_CLASS,
    authenticated_oci_document,
    build_execution_admission,
    code_authorities,
    repository_source_identity,
    validate_execution_admission,
    validate_oci_container,
    validate_receipt,
)


DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
OCI_ACCEPT = ", ".join(sorted(INDEX_MEDIA_TYPES | MANIFEST_MEDIA_TYPES))
MAX_TOKEN_BYTES = 128 * 1024
FORBIDDEN_OUTPUT_ROOTS = (Path("/dev"), Path("/proc"), Path("/sys"))


def run(command: list[str], input_text: str | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command, input=input_text, text=True, capture_output=True, check=False
    )


def psql_command(docker: str, container: str, *arguments: str) -> list[str]:
    return [
        docker,
        "exec",
        "--interactive",
        container,
        "psql",
        "-X",
        "--no-align",
        "--tuples-only",
        "--quiet",
        "--set=ON_ERROR_STOP=1",
        "--username=postgres",
        "--dbname=postgres",
        *arguments,
    ]


def read_bounded(response: object, maximum: int, label: str) -> bytes:
    body = response.read(maximum + 1)
    if len(body) > maximum:
        raise GateFailure(f"{label} exceeds {maximum} bytes")
    return body


def validate_output_path(root: Path, output: Path | None) -> Path:
    if output is None:
        raise GateFailure("receipt publication requires an explicit --output path")
    if any(not hasattr(os, name) for name in ("O_DIRECTORY", "O_NOFOLLOW")):
        raise GateFailure("secure no-follow receipt publication is unavailable on this platform")
    if ".." in output.parts:
        raise GateFailure("receipt output must not contain '..'")
    target = Path(os.path.abspath(os.fspath(output)))
    root_resolved = root.resolve(strict=True)
    try:
        target.relative_to(root_resolved)
    except ValueError:
        pass
    else:
        raise GateFailure("receipt output must be outside the source repository")
    for forbidden in FORBIDDEN_OUTPUT_ROOTS:
        if target == forbidden or forbidden in target.parents:
            raise GateFailure(f"receipt output under {forbidden} is forbidden")

    current = Path(target.anchor)
    for component in target.parent.parts[1:]:
        current /= component
        try:
            metadata = os.lstat(current)
        except FileNotFoundError as error:
            raise GateFailure("receipt output parent directory must already exist") from error
        if stat.S_ISLNK(metadata.st_mode):
            raise GateFailure("receipt output path must not contain symlink components")
        if not stat.S_ISDIR(metadata.st_mode):
            raise GateFailure("receipt output parent must be a normal directory")

    try:
        metadata = os.lstat(target)
    except FileNotFoundError:
        return target
    if stat.S_ISLNK(metadata.st_mode):
        raise GateFailure("receipt output target must not be a symlink")
    if not stat.S_ISREG(metadata.st_mode):
        raise GateFailure("receipt output target must not be a directory, FIFO, socket, or device")
    raise GateFailure("receipt output target already exists; exclusive creation is required")


def open_directory_no_follow(directory: Path) -> int:
    required = ("O_DIRECTORY", "O_NOFOLLOW")
    if any(not hasattr(os, name) for name in required):
        raise GateFailure("secure no-follow receipt publication is unavailable on this platform")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0)
    descriptor = os.open(directory.anchor, flags)
    try:
        for component in directory.parts[1:]:
            child = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
            if not stat.S_ISDIR(os.fstat(descriptor).st_mode):
                raise GateFailure("receipt output parent must remain a normal directory")
        return descriptor
    except Exception:
        os.close(descriptor)
        raise


def write_receipt_file(root: Path, output: Path | None, encoded: str) -> Path:
    target = validate_output_path(root, output)
    parent_descriptor = open_directory_no_follow(target.parent)
    file_descriptor: int | None = None
    created_identity: tuple[int, int] | None = None
    try:
        flags = (
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | os.O_NOFOLLOW
            | getattr(os, "O_CLOEXEC", 0)
        )
        file_descriptor = os.open(
            target.name, flags, 0o600, dir_fd=parent_descriptor
        )
        metadata = os.fstat(file_descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise GateFailure("opened receipt output is not a regular file")
        created_identity = (metadata.st_dev, metadata.st_ino)
        handle = os.fdopen(file_descriptor, "w", encoding="utf-8", newline="\n")
        file_descriptor = None
        with handle:
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
    except Exception as error:
        if file_descriptor is not None:
            os.close(file_descriptor)
        if created_identity is not None:
            try:
                current = os.stat(target.name, dir_fd=parent_descriptor, follow_symlinks=False)
                if (current.st_dev, current.st_ino) == created_identity:
                    os.unlink(target.name, dir_fd=parent_descriptor)
            except FileNotFoundError:
                pass
        if isinstance(error, GateFailure):
            raise
        raise GateFailure(f"could not publish receipt to a new regular file: {error}") from error
    finally:
        os.close(parent_descriptor)
    return target


def fetch_authenticated_oci_document(
    registry: str, repository: str, digest: str
) -> bytes:
    token_url = "https://auth.docker.io/token?" + urlencode(
        {
            "service": "registry.docker.io",
            "scope": f"repository:{repository}:pull",
        }
    )
    try:
        with urlopen(token_url, timeout=30) as response:
            token_document = json.loads(read_bounded(response, MAX_TOKEN_BYTES, "registry token"))
        token = token_document.get("token") if isinstance(token_document, dict) else None
        if not isinstance(token, str) or not token:
            raise GateFailure("registry token response is malformed")
        url = f"https://{registry}/v2/{repository}/manifests/{digest}"
        request = Request(
            url,
            headers={"Accept": OCI_ACCEPT, "Authorization": f"Bearer {token}"},
        )
        with urlopen(request, timeout=60) as response:
            content_digest = response.headers.get("Docker-Content-Digest")
            raw = read_bounded(response, MAX_OCI_DOCUMENT_BYTES, "OCI manifest")
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise GateFailure(f"could not authenticate OCI manifest {digest}: {error}") from error
    if (
        content_digest != digest
        or hashlib.sha256(raw).hexdigest() != digest.removeprefix("sha256:")
    ):
        raise GateFailure("registry OCI response does not match its requested content digest")
    return raw


def verify_environment(
    docker: str, container: str, profile: dict[str, object]
) -> dict[str, object]:
    postgresql = profile["postgresql"]
    assert isinstance(postgresql, dict)
    configuration = postgresql["configuration"]
    assert isinstance(configuration, dict)
    settings = configuration["settings"]
    assert isinstance(settings, dict)
    expressions = [
        "current_setting('server_version_num')",
        "current_setting('server_encoding')",
        "current_setting('client_encoding')",
        "(SELECT datcollate FROM pg_database WHERE datname = current_database())",
        "(SELECT datctype FROM pg_database WHERE datname = current_database())",
        "current_setting('TimeZone')",
        *(f"current_setting('{name}')" for name in settings),
    ]
    query = "SELECT " + ", ".join(expressions)
    completed = run(psql_command(docker, container, "--command", query))
    expected = [
        postgresql["server_version_num"],
        configuration["server_encoding"],
        configuration["client_encoding"],
        configuration["lc_collate"],
        configuration["lc_ctype"],
        configuration["timezone"],
        *settings.values(),
    ]
    observed = completed.stdout.strip().split("|")
    if completed.returncode != 0 or observed != expected:
        raise GateFailure(
            "PostgreSQL environment differs from the pinned profile: "
            f"expected={expected!r}, observed={observed!r}, stderr={completed.stderr[-1000:]!r}"
        )
    return {
        "server_version_num": observed[0],
        "server_encoding": observed[1],
        "client_encoding": observed[2],
        "lc_collate": observed[3],
        "lc_ctype": observed[4],
        "timezone": observed[5],
        "settings": dict(zip(settings, observed[6:], strict=True)),
    }


def inspect_resolved_container(
    docker: str, container: str, container_pin: dict[str, object]
) -> dict[str, object]:
    reference = container_pin["reference"]
    index_digest = container_pin["manifest_digest"]
    registry = container_pin["registry"]
    repository = container_pin["repository"]
    image_completed = run([docker, "image", "inspect", reference])
    container_completed = run([docker, "container", "inspect", container])
    if image_completed.returncode != 0 or container_completed.returncode != 0:
        raise GateFailure("could not inspect the resolved PostgreSQL image and container")
    try:
        images = json.loads(image_completed.stdout)
        containers = json.loads(container_completed.stdout)
        if not isinstance(images, list) or len(images) != 1 or not isinstance(images[0], dict):
            raise ValueError("image inspect must return one object")
        if (
            not isinstance(containers, list)
            or len(containers) != 1
            or not isinstance(containers[0], dict)
        ):
            raise ValueError("container inspect must return one object")
        image = images[0]
        running_container = containers[0]
        descriptor = image.get("Descriptor")
        if descriptor is not None and not isinstance(descriptor, dict):
            raise ValueError("local image descriptor is malformed")
        image_digest = image.get("Id")
        image_os = image.get("Os")
        image_architecture = image.get("Architecture")
        image_variant = image.get("Variant")
        if (
            not isinstance(image_digest, str)
            or DIGEST.fullmatch(image_digest) is None
            or not isinstance(image_os, str)
            or not image_os
            or not isinstance(image_architecture, str)
            or not image_architecture
            or (image_variant is not None and (not isinstance(image_variant, str) or not image_variant))
            or running_container.get("Image") != image_digest
            or running_container.get("Platform") != image_os
        ):
            raise ValueError("resolved image identity or platform is inconsistent")
        repo_digests = image.get("RepoDigests")
        if not isinstance(repo_digests, list) or not any(
            isinstance(value, str) and value.endswith(f"@{index_digest}")
            for value in repo_digests
        ):
            raise ValueError("resolved image is not bound to the pinned index digest")
    except (TypeError, ValueError, json.JSONDecodeError) as error:
        raise GateFailure(f"invalid resolved PostgreSQL image identity: {error}") from error

    index_raw = fetch_authenticated_oci_document(registry, repository, index_digest)
    index_document = authenticated_oci_document(
        base64.b64encode(index_raw).decode(), index_digest, len(index_raw), "index"
    )
    if (
        index_document.get("schemaVersion") != 2
        or index_document.get("mediaType") not in INDEX_MEDIA_TYPES
        or not isinstance(index_document.get("manifests"), list)
    ):
        raise GateFailure("pinned OCI index metadata is invalid")
    members = [
        member
        for member in index_document["manifests"]
        if isinstance(member, dict)
        and member.get("mediaType") in MANIFEST_MEDIA_TYPES
        and isinstance(member.get("platform"), dict)
        and member["platform"].get("os") == image_os
        and member["platform"].get("architecture") == image_architecture
        and (
            image_variant is None
            or member["platform"].get("variant") == image_variant
        )
    ]
    if len(members) != 1:
        raise GateFailure("local image child is not a platform member of the pinned OCI index")
    member = members[0]
    resolved_platform = member.get("platform")
    resolved_manifest_digest = member.get("digest")
    resolved_manifest_media_type = member.get("mediaType")
    if (
        not isinstance(resolved_manifest_digest, str)
        or DIGEST.fullmatch(resolved_manifest_digest) is None
        or resolved_manifest_digest == index_digest
        or type(member.get("size")) is not int
        or member["size"] <= 0
    ):
        raise GateFailure("pinned OCI child descriptor is malformed")
    if descriptor is not None and (
        descriptor.get("digest") != resolved_manifest_digest
        or descriptor.get("mediaType") != resolved_manifest_media_type
        or (descriptor.get("size") is not None and descriptor.get("size") != member["size"])
        or (
            descriptor.get("platform") is not None
            and descriptor.get("platform") != resolved_platform
        )
    ):
        raise GateFailure("local image descriptor differs from the pinned OCI index member")

    manifest_raw = fetch_authenticated_oci_document(
        registry, repository, resolved_manifest_digest
    )
    manifest_document = authenticated_oci_document(
        base64.b64encode(manifest_raw).decode(),
        resolved_manifest_digest,
        len(manifest_raw),
        "child manifest",
    )
    config = manifest_document.get("config")
    if not isinstance(config, dict):
        raise GateFailure("resolved child manifest omitted its image config descriptor")
    evidence = {
        "reference": reference,
        "registry": registry,
        "repository": repository,
        "index": {
            "digest": index_digest,
            "media_type": index_document["mediaType"],
            "size": len(index_raw),
            "document_base64": base64.b64encode(index_raw).decode(),
        },
        "resolved_manifest": {
            "digest": resolved_manifest_digest,
            "media_type": resolved_manifest_media_type,
            "size": member["size"],
            "document_base64": base64.b64encode(manifest_raw).decode(),
            "config": {
                "digest": config.get("digest"),
                "media_type": config.get("mediaType"),
                "size": config.get("size"),
            },
        },
        "image_digest": image_digest,
        "platform": resolved_platform,
    }
    authenticated_child, authenticated_config = validate_oci_container(evidence, container_pin)
    if authenticated_child != resolved_manifest_digest or authenticated_config != image_digest:
        raise GateFailure("authenticated OCI child is not the recorded local image identity")
    return evidence


def execute_profile(
    docker: str, profile_path: Path, raw_execution_admission: object
) -> dict[str, object]:
    execution_admission = validate_execution_admission(raw_execution_admission)
    profile_bytes = profile_path.read_bytes()
    profile = json.loads(profile_bytes)
    audit = validate_profile(ROOT, profile)
    postgresql = profile["postgresql"]
    configuration = postgresql["configuration"]
    settings = configuration["settings"]
    container_name = f"hyphae-pg-core-{uuid.uuid4().hex[:12]}"
    host = {
        "os": platform.system().lower(),
        "architecture": platform.machine().lower(),
    }
    if not host["os"] or not host["architecture"]:
        raise GateFailure("host operating system and architecture must be observable")
    start = [
        docker,
        "run",
        "--detach",
        "--rm",
        "--name",
        container_name,
        "--network=none",
        "--env=POSTGRES_HOST_AUTH_METHOD=trust",
        f"--env=POSTGRES_INITDB_ARGS={configuration['initdb_args']}",
        "--env=TZ=UTC",
        postgresql["container"]["reference"],
        "-c",
        f"client_encoding={configuration['client_encoding']}",
        "-c",
        f"timezone={configuration['timezone']}",
    ]
    for name, value in settings.items():
        start.extend(["-c", f"{name}={value}"])
    started = run(start)
    if started.returncode != 0:
        run([docker, "rm", "--force", container_name])
        raise GateFailure(f"could not start PostgreSQL container: {started.stderr[-1000:]}")
    receipt: dict[str, object] | None = None
    failure: Exception | None = None
    cleanup_failure: GateFailure | None = None
    try:
        for _ in range(120):
            ready = run([docker, "exec", container_name, "pg_isready", "--quiet"])
            if ready.returncode == 0:
                break
            time.sleep(0.5)
        else:
            raise GateFailure("PostgreSQL did not become ready within 60 seconds")
        resolved_container = inspect_resolved_container(
            docker,
            container_name,
            postgresql["container"],
        )
        observed_configuration = verify_environment(docker, container_name, profile)
        results: list[dict[str, object]] = []
        for case in profile["cases"]:
            oracle = resolve_contained_file(
                ROOT, case["oracle"], CASES_DIRECTORY, f"case {case['id']} oracle"
            )
            oracle_bytes = oracle.read_bytes()
            oracle_sha256 = hashlib.sha256(oracle_bytes).hexdigest()
            if oracle_sha256 != case["oracle_sha256"]:
                raise GateFailure(f"case {case['id']} oracle bytes changed after validation")
            completed = run(
                psql_command(docker, container_name, "--file=-"),
                oracle_bytes.decode("utf-8"),
            )
            marker = f"ok:{case['id']}"
            lines = [line.strip() for line in completed.stdout.splitlines() if line.strip()]
            passed = completed.returncode == 0 and lines[-1:] == [marker]
            diagnostic = "" if passed else completed.stderr[-1000:]
            results.append(
                {
                    "id": case["id"],
                    "status": "observed-passed" if passed else "observed-failed",
                    "exit_code": completed.returncode,
                    "oracle_sha256": oracle_sha256,
                    "stdout": completed.stdout,
                    "stdout_sha256": hashlib.sha256(completed.stdout.encode()).hexdigest(),
                    "diagnostic": diagnostic,
                    "diagnostic_sha256": hashlib.sha256(diagnostic.encode()).hexdigest(),
                }
            )
        passed_count = sum(result["status"] == "observed-passed" for result in results)
        receipt = {
            "schema": "hyphae-postgresql-application-core-receipt-v2",
            "status": "observed-passed" if passed_count == len(results) else "observed-failed",
            "profile_sha256": hashlib.sha256(profile_bytes).hexdigest(),
            "postgresql_version": postgresql["version"],
            "evidence_class": RECEIPT_EVIDENCE_CLASS,
            "claim_authority": False,
            "claims": [],
            "closure_declared": False,
            "execution_admission": execution_admission,
            "source": repository_source_identity(ROOT),
            "code_authorities": code_authorities(ROOT),
            "host": host,
            "container": resolved_container,
            "configuration": configuration,
            "observed_configuration": observed_configuration,
            "case_count": len(results),
            "passed_count": passed_count,
            "profile_claim_status": audit["parity_status"],
            "cleanup": "pending",
            "results": results,
        }
    except Exception as error:
        failure = error
    finally:
        removed = run([docker, "rm", "--force", container_name])
        if removed.returncode != 0:
            cleanup_failure = GateFailure(
                f"could not remove PostgreSQL container: {removed.stderr[-1000:]}"
            )
    if failure is not None:
        if cleanup_failure is not None:
            raise GateFailure(f"{failure}; additionally, {cleanup_failure}") from failure
        raise failure
    if cleanup_failure is not None:
        raise cleanup_failure
    if receipt is None:
        raise GateFailure("PostgreSQL run produced no receipt")
    receipt["cleanup"] = "container-removed"
    validate_receipt(ROOT, profile_path, receipt)
    return receipt


def main(arguments: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--profile", type=Path, default=PROFILE_PATH)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--operator", required=True)
    parser.add_argument("--attest-non-claim-authoritative", action="store_true")
    args = parser.parse_args(arguments)
    profile_path = args.profile if args.profile.is_absolute() else ROOT / args.profile
    try:
        validate_output_path(ROOT, args.output)
        admission = build_execution_admission(
            args.operator, args.attest_non_claim_authoritative
        )
        receipt = execute_profile(args.docker, profile_path, admission)
        encoded = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
        write_receipt_file(ROOT, args.output, encoded)
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"PostgreSQL Application Core v1 run failed: {error}", file=sys.stderr)
        return 2
    return 0 if receipt["status"] == "observed-passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
