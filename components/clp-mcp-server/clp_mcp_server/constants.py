"""Constants for CLP MCP server."""

from enum import auto, IntEnum

SEARCH_MAX_NUM_RESULTS = 1000

POLLING_INTERVAL_SECONDS = 1


class QueryJobStatus(IntEnum):
    """Matching the `QueryJobStatus` class in `job_orchestration.query_scheduler.constants`."""

    PENDING = 0
    RUNNING = auto()
    SUCCEEDED = auto()
    FAILED = auto()
    CANCEL_REQUESTED = auto()
    CANCELLING = auto()
    CANCELLED = auto()
    KILLED = auto()
