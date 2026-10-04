"""escrow: Python SDK for escrowd. Phase 1.1 provides only the daemon client."""

from escrow._client import Client, connect

__all__ = ["Client", "connect"]
