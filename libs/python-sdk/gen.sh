#!/bin/sh
# Regenerate the Python protocol stubs from proto/. CI fails if the committed stubs differ.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
uv run --frozen python -m grpc_tools.protoc -I ../../proto \
  --python_out=src --pyi_out=src --grpc_python_out=src \
  ../../proto/escrow/v1/escrow.proto
touch src/escrow/v1/__init__.py
