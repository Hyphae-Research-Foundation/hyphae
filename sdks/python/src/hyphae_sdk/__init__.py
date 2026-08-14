# SPDX-License-Identifier: AGPL-3.0-only

from importlib.metadata import PackageNotFoundError, version

from .client import ApiResponse, HyphaeApiError, HyphaeClient, HyphaeClientError
from .generated import *  # noqa: F403
from .generated import __all__ as _generated_all

try:
    __version__ = version("hyphae-sdk")
except PackageNotFoundError:
    __version__ = "0+uninstalled"

__all__ = [
    "__version__",
    "ApiResponse",
    "HyphaeApiError",
    "HyphaeClient",
    "HyphaeClientError",
    *_generated_all,
]
