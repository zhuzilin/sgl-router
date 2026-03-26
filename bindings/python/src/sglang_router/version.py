_base_version = "0.3.2"

try:
    from sglang_router_rs import get_git_commit
    _commit = get_git_commit()
    __version__ = f"{_base_version}-{_commit}" if _commit and _commit != "unknown" else _base_version
except ImportError:
    __version__ = _base_version
