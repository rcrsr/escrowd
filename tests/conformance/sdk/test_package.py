"""The SDK's module layout."""

import ast
import re
import tomllib
from pathlib import Path

import escrow


def test_only_lowlevel_and_model_import_the_generated_protocol_code():
    src = Path(escrow.__file__).parent
    importing = set()
    for path in src.glob("*.py"):
        for node in ast.walk(ast.parse(path.read_text())):
            names = [node.module] if isinstance(node, ast.ImportFrom) else []
            if isinstance(node, ast.Import):
                names = [a.name for a in node.names]
            if any(n and (n == "escrow.v1" or n.startswith("escrow.v1.")) for n in names):
                importing.add(path.stem)
    assert importing == {"lowlevel", "_model"}


PYPROJECT = Path(__file__).resolve().parents[3] / "sdk" / "python" / "pyproject.toml"
GENERATED = Path(escrow.__file__).parent / "v1"


def _version(text: str) -> tuple[int, ...]:
    return tuple(int(n) for n in text.split("."))


def _spec(name: str) -> str:
    deps = tomllib.loads(PYPROJECT.read_text())["project"]["dependencies"]
    return next(d for d in deps if d.startswith(name))


def _floor(name: str) -> tuple[int, ...]:
    found = re.search(r">=([\d.]+)", _spec(name))
    assert found
    return _version(found.group(1))


def test_dependency_floors_cover_the_generated_code():
    pb2 = (GENERATED / "escrow_pb2.py").read_text()
    grpc_gen = (GENERATED / "escrow_pb2_grpc.py").read_text()
    protobuf_gen = re.search(r"# Protobuf Python Version: ([\d.]+)", pb2)
    grpc_gen_version = re.search(r"GRPC_GENERATED_VERSION = '([\d.]+)'", grpc_gen)
    assert protobuf_gen and grpc_gen_version
    assert _floor("protobuf") >= _version(protobuf_gen.group(1))
    assert _floor("grpcio") >= _version(grpc_gen_version.group(1))[:2]


def test_protobuf_upper_bound_is_the_next_major():
    assert f"<{_floor('protobuf')[0] + 1}" in _spec("protobuf")


def test_package_ships_py_typed():
    assert (Path(escrow.__file__).parent / "py.typed").is_file()
