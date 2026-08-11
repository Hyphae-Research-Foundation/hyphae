#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Generate dependency-free TypeScript and Python models from JSON Schema."""

from __future__ import annotations

import argparse
import hashlib
import json
import keyword
import re
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SCHEMA_DIR = ROOT / "contracts" / "json-schema"
OUTPUTS = {
    "v1": (
        ROOT / "sdks" / "typescript" / "src" / "generated.ts",
        ROOT / "sdks" / "python" / "src" / "hyphae_sdk" / "generated.py",
    ),
    "v2": (
        ROOT / "sdks" / "typescript" / "src" / "v2" / "generated.ts",
        ROOT / "sdks" / "python" / "src" / "hyphae_sdk" / "v2" / "generated.py",
    ),
}


def load_models(version: str) -> tuple[dict[str, dict[str, Any]], str]:
    models: dict[str, dict[str, Any]] = {}
    digest = hashlib.sha256()
    paths = sorted(SCHEMA_DIR.glob(f"*-{version}.schema.json"))
    if version == "v2":
        # `native-v2` is the aggregate operation vocabulary. Its embedded
        # `$defs` intentionally reference the canonical standalone v2 models;
        # standalone definitions win so each model has one generation source.
        paths.sort(key=lambda path: path.name == "native-v2.schema.json")
    if not paths:
        raise ValueError(f"no {version} schemas found")
    documents: list[tuple[Path, dict[str, Any], str]] = []
    root_names: dict[str, str] = {}
    for path in paths:
        encoded = path.read_bytes()
        digest.update(path.name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(encoded)
        schema = json.loads(encoded)
        if schema.get("$schema") != "https://json-schema.org/draft/2020-12/schema":
            raise ValueError(f"{path.name} is not JSON Schema 2020-12")
        title = schema.get("title")
        if not isinstance(title, str):
            raise ValueError(f"{path.name} has no title")
        root_name = model_identifier(title)
        previous = root_names.setdefault(path.name, root_name)
        if previous != root_name:
            raise ValueError(f"conflicting root model name for {path.name}")
        documents.append((path, schema, root_name))
    for path, schema, root_name in documents:
        root = {
            key: value
            for key, value in schema.items()
            if key not in {"$schema", "$defs", "title"}
        }
        add_model(models, root_name, normalize_model_references(root, root_names), path.name)
        for name, definition in schema.get("$defs", {}).items():
            add_model(
                models,
                model_identifier(name),
                normalize_model_references(definition, root_names),
                path.name,
            )
    return models, digest.hexdigest()


def add_model(
    models: dict[str, dict[str, Any]],
    name: str,
    schema: dict[str, Any],
    source: str,
) -> None:
    existing = models.get(name)
    if existing is not None and canonical(existing) != canonical(schema):
        if source == "native-v2.schema.json":
            return
        raise ValueError(f"conflicting model {name} in {source}")
    models[name] = schema


def canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def model_identifier(value: str) -> str:
    identifier = pascal(value)
    if not identifier:
        raise ValueError(f"schema model name has no identifier: {value!r}")
    if identifier[0].isdigit():
        identifier = f"Model{identifier}"
    if keyword.iskeyword(identifier):
        identifier = f"{identifier}Model"
    return identifier


def normalize_model_references(value: Any, root_names: dict[str, str]) -> Any:
    if isinstance(value, list):
        return [normalize_model_references(item, root_names) for item in value]
    if not isinstance(value, dict):
        return value
    normalized = {
        key: normalize_model_references(child, root_names)
        for key, child in value.items()
    }
    reference = value.get("$ref")
    if isinstance(reference, str):
        normalized["$ref"] = normalize_model_reference(reference, root_names)
    return normalized


def normalize_model_reference(reference: str, root_names: dict[str, str]) -> str:
    document, separator, fragment = reference.partition("#")
    if fragment.startswith("/$defs/"):
        return f"#/$defs/{model_identifier(fragment.rsplit('/', 1)[-1])}"
    if separator and fragment:
        return reference
    if document:
        root_name = root_names.get(Path(document).name)
        if root_name is not None:
            return f"#/$defs/{root_name}"
    return reference


def description(schema: dict[str, Any]) -> str | None:
    value = schema.get("description")
    return value.strip() if isinstance(value, str) and value.strip() else None


def ts_comment(text: str | None, indent: str = "") -> list[str]:
    if text is None:
        return []
    text = text.replace("*/", "* /")
    return [f"{indent}/** {text} */"]


def ts_type(schema: Any, indent: int = 0) -> str:
    if schema is True or schema == {}:
        return "JsonValue"
    if schema is False:
        return "never"
    if not isinstance(schema, dict):
        raise ValueError("invalid schema node")
    reference = schema.get("$ref")
    if isinstance(reference, str):
        return reference.rsplit("/", 1)[-1]
    if "const" in schema:
        return json.dumps(schema["const"], ensure_ascii=False)
    if isinstance(schema.get("enum"), list):
        return " | ".join(json.dumps(value, ensure_ascii=False) for value in schema["enum"])
    if isinstance(schema.get("anyOf"), list):
        return " | ".join(ts_type(item, indent) for item in schema["anyOf"])
    if isinstance(schema.get("oneOf"), list):
        return " | ".join(ts_type(item, indent) for item in schema["oneOf"])
    schema_type = schema.get("type")
    if isinstance(schema_type, list):
        return " | ".join(ts_primitive(item, schema, indent) for item in schema_type)
    return ts_primitive(schema_type, schema, indent)


def ts_primitive(schema_type: Any, schema: dict[str, Any], indent: int) -> str:
    if schema_type == "null":
        return "null"
    if schema_type == "boolean":
        return "boolean"
    if schema_type == "integer":
        return "JsonInteger"
    if schema_type == "number":
        return "number"
    if schema_type == "string":
        return "string"
    if schema_type == "array":
        return f"Array<{ts_type(schema.get('items', True), indent)}>"
    if schema_type == "object" or "properties" in schema:
        properties = schema.get("properties", {})
        required = set(schema.get("required", []))
        lines = ["{"]
        for name, child in properties.items():
            rendered_name = name if re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", name) else json.dumps(name)
            optional = "" if name in required else "?"
            lines.append(f"{' ' * (indent + 2)}{rendered_name}{optional}: {ts_type(child, indent + 2)};")
        lines.append(f"{' ' * indent}}}")
        return "\n".join(lines)
    return "JsonValue"


def render_typescript(models: dict[str, dict[str, Any]], digest: str) -> str:
    lines = [
        "// SPDX-License-Identifier: AGPL-3.0-only",
        "// @generated by tools/generate_sdk_models.py; do not edit.",
        f'export const CONTRACT_SHA256 = "{digest}" as const;',
        "",
        "export type JsonInteger = number | bigint;",
        "",
        "export type JsonValue =",
        "  | null",
        "  | boolean",
        "  | JsonInteger",
        "  | string",
        "  | Array<JsonValue>",
        "  | { [key: string]: JsonValue };",
        "",
    ]
    for name in sorted(models):
        schema = models[name]
        lines.extend(ts_comment(description(schema)))
        if schema.get("type") == "object" and "oneOf" not in schema:
            lines.append(f"export interface {name} {{")
            required = set(schema.get("required", []))
            for field, child in schema.get("properties", {}).items():
                lines.extend(ts_comment(description(child), "  "))
                rendered = field if re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", field) else json.dumps(field)
                optional = "" if field in required else "?"
                lines.append(f"  {rendered}{optional}: {ts_type(child, 2)};")
            lines.append("}")
        else:
            lines.append(f"export type {name} = {ts_type(schema)};")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def pascal(value: str) -> str:
    return "".join(part[:1].upper() + part[1:] for part in re.split(r"[^A-Za-z0-9]+", value) if part)


def tagged_variants(
    name: str,
    schema: dict[str, Any],
    models: dict[str, dict[str, Any]],
) -> list[tuple[str, dict[str, Any]]]:
    variants = schema.get("oneOf")
    if not isinstance(variants, list) or not variants:
        return []
    output: list[tuple[str, dict[str, Any]]] = []
    for index, variant in enumerate(variants, start=1):
        if isinstance(variant, dict) and isinstance(variant.get("$ref"), str):
            referenced = variant["$ref"].rsplit("/", 1)[-1]
            variant = models.get(referenced)
        if not isinstance(variant, dict) or variant.get("type") != "object":
            return []
        tag = None
        for child in variant.get("properties", {}).values():
            if isinstance(child, dict) and isinstance(child.get("const"), str):
                tag = child["const"]
                break
        suffix = pascal(tag) if tag is not None else f"Variant{index}"
        output.append((f"{name}{suffix}", variant))
    return output


def py_type(schema: Any) -> str:
    if schema is True or schema == {}:
        return "JsonValue"
    if schema is False:
        return "Never"
    if not isinstance(schema, dict):
        raise ValueError("invalid schema node")
    reference = schema.get("$ref")
    if isinstance(reference, str):
        return reference.rsplit("/", 1)[-1]
    if "const" in schema:
        return f"Literal[{py_literal(schema['const'])}]"
    if isinstance(schema.get("enum"), list):
        values = ", ".join(py_literal(value) for value in schema["enum"])
        return f"Literal[{values}]"
    if isinstance(schema.get("anyOf"), list):
        return " | ".join(py_type(item) for item in schema["anyOf"])
    if isinstance(schema.get("oneOf"), list):
        return " | ".join(py_type(item) for item in schema["oneOf"])
    schema_type = schema.get("type")
    if isinstance(schema_type, list):
        return " | ".join(py_primitive(item, schema) for item in schema_type)
    return py_primitive(schema_type, schema)


def py_literal(value: Any) -> str:
    if value is True:
        return "True"
    if value is False:
        return "False"
    if value is None:
        return "None"
    return json.dumps(value, ensure_ascii=False)


def py_primitive(schema_type: Any, schema: dict[str, Any]) -> str:
    if schema_type == "null":
        return "None"
    if schema_type == "boolean":
        return "bool"
    if schema_type == "integer":
        return "int"
    if schema_type == "number":
        return "float"
    if schema_type == "string":
        return "str"
    if schema_type == "array":
        return f"list[{py_type(schema.get('items', True))}]"
    if schema_type == "object" or "properties" in schema:
        return "dict[str, JsonValue]"
    return "JsonValue"


def render_python_class(name: str, schema: dict[str, Any]) -> list[str]:
    doc = description(schema)
    properties = schema.get("properties", {})
    if not properties:
        lines = [f"class {name}(TypedDict):"]
        if doc is not None:
            lines.append(f'    """{doc.replace(chr(34) * 3, chr(34) * 2)}"""')
        lines.append("    pass")
        return lines
    required = set(schema.get("required", []))
    fields = []
    for field, child in properties.items():
        annotation = py_type(child)
        if field not in required:
            annotation = f"NotRequired[{annotation}]"
        fields.append((field, annotation))
    if any(not field.isidentifier() or keyword.iskeyword(field) for field, _ in fields):
        lines = []
        if doc is not None:
            lines.append(f"# {doc.replace(chr(10), ' ')}")
        lines.extend([f"{name} = TypedDict(", f'    "{name}",', "    {"])
        lines.extend(
            f"        {json.dumps(field, ensure_ascii=False)}: {json.dumps(annotation)},"
            for field, annotation in fields
        )
        lines.extend(["    },", ")"])
        return lines
    lines = [f"class {name}(TypedDict):"]
    if doc is not None:
        lines.append(f'    """{doc.replace(chr(34) * 3, chr(34) * 2)}"""')
    for field, annotation in fields:
        lines.append(f"    {field}: {annotation}")
    return lines


def render_python(models: dict[str, dict[str, Any]], digest: str) -> str:
    lines = [
        "# SPDX-License-Identifier: AGPL-3.0-only",
        "# @generated by tools/generate_sdk_models.py; do not edit.",
        "from __future__ import annotations",
        "",
        "from typing import Literal, Never, NotRequired, TypeAlias, TypedDict",
        "",
        f'CONTRACT_SHA256 = "{digest}"',
        "",
        "JsonValue: TypeAlias = (",
        "    None | bool | int | str | list[\"JsonValue\"] | dict[str, \"JsonValue\"]",
        ")",
        "",
    ]
    aliases: list[str] = []
    exported = ["CONTRACT_SHA256", "JsonValue"]
    for name in sorted(models):
        schema = models[name]
        variants = tagged_variants(name, schema, models)
        if schema.get("type") == "object" and not variants:
            lines.extend(render_python_class(name, schema))
            lines.append("")
        elif variants:
            variant_names = []
            for variant_name, variant in variants:
                if variant_name not in models:
                    lines.extend(render_python_class(variant_name, variant))
                    lines.append("")
                variant_names.append(variant_name)
                exported.append(variant_name)
            aliases.append(f"{name}: TypeAlias = {' | '.join(variant_names)}")
        else:
            aliases.append(f"{name}: TypeAlias = {py_type(schema)}")
        exported.append(name)
    lines.extend(aliases)
    lines.append("")
    lines.append("__all__ = [")
    lines.extend(f'    "{name}",' for name in sorted(set(exported)))
    lines.append("]")
    return "\n".join(lines).rstrip() + "\n"


def write_or_check(path: Path, content: str, check: bool) -> bool:
    if check:
        return path.is_file() and path.read_text(encoding="utf-8") == content
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as output:
        output.write(content)
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    arguments = parser.parse_args()
    outputs: dict[Path, str] = {}
    for version, (typescript, python) in OUTPUTS.items():
        models, digest = load_models(version)
        outputs[typescript] = render_typescript(models, digest)
        outputs[python] = render_python(models, digest)
    stale = [path for path, content in outputs.items() if not write_or_check(path, content, arguments.check)]
    if stale:
        for path in stale:
            print(f"stale generated SDK model: {path.relative_to(ROOT)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
