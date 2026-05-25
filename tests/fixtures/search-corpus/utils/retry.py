"""Retry and resilience utilities."""

import time
import random
from typing import Callable, TypeVar, Optional

T = TypeVar("T")


def retry_with_backoff(
    fn: Callable[[], T],
    max_retries: int = 3,
    base_delay: float = 1.0,
    max_delay: float = 30.0,
    jitter: bool = True,
    retryable_exceptions: tuple = (Exception,),
) -> T:
    """Retry a function with exponential backoff."""
    last_exception = None
    for attempt in range(max_retries + 1):
        try:
            return fn()
        except retryable_exceptions as exc:
            last_exception = exc
            if attempt == max_retries:
                break
            delay = min(base_delay * (2 ** attempt), max_delay)
            if jitter:
                delay = delay * (0.5 + random.random())
            time.sleep(delay)
    raise last_exception


def rate_limiter(max_calls: int, period: float = 1.0):
    """Simple rate limiter decorator using a sliding window."""
    timestamps = []

    def decorator(fn):
        def wrapper(*args, **kwargs):
            nonlocal timestamps
            now = time.time()
            timestamps = [t for t in timestamps if now - t < period]
            if len(timestamps) >= max_calls:
                sleep_time = period - (now - timestamps[0])
                if sleep_time > 0:
                    time.sleep(sleep_time)
            timestamps.append(time.time())
            return fn(*args, **kwargs)
        return wrapper
    return decorator


def with_timeout(fn: Callable[[], T], timeout_secs: float) -> Optional[T]:
    """Run a function with a timeout. Returns None if it times out."""
    import threading

    result = [None]
    exception = [None]

    def target():
        try:
            result[0] = fn()
        except Exception as e:
            exception[0] = e

    thread = threading.Thread(target=target)
    thread.daemon = True
    thread.start()
    thread.join(timeout=timeout_secs)

    if thread.is_alive():
        return None
    if exception[0]:
        raise exception[0]
    return result[0]
