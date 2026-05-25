"""String and data formatting utilities."""

import re
from datetime import datetime, timezone
from typing import Optional


def slugify(text: str) -> str:
    """Convert a string to a URL-friendly slug."""
    text = text.lower().strip()
    text = re.sub(r"[^\w\s-]", "", text)
    text = re.sub(r"[\s_]+", "-", text)
    text = re.sub(r"-+", "-", text)
    text = text.strip("-")
    return text


def format_file_size(size_bytes: int) -> str:
    """Format a byte count as a human-readable file size."""
    if size_bytes < 0:
        return "0 B"
    units = ["B", "KB", "MB", "GB", "TB"]
    unit_index = 0
    size = float(size_bytes)
    while size >= 1024.0 and unit_index < len(units) - 1:
        size /= 1024.0
        unit_index += 1
    if unit_index == 0:
        return f"{int(size)} B"
    return f"{size:.1f} {units[unit_index]}"


def truncate_text(text: str, max_length: int, suffix: str = "...") -> str:
    """Truncate text to max_length, appending suffix if truncated."""
    if not text or len(text) <= max_length:
        return text
    if max_length <= len(suffix):
        return suffix[:max_length]
    return text[: max_length - len(suffix)] + suffix


def format_timestamp(dt: Optional[datetime] = None, fmt: str = "iso") -> str:
    """Format a datetime to a standard string representation."""
    if dt is None:
        dt = datetime.now(timezone.utc)
    if fmt == "iso":
        return dt.isoformat()
    elif fmt == "human":
        return dt.strftime("%B %d, %Y at %I:%M %p")
    elif fmt == "date":
        return dt.strftime("%Y-%m-%d")
    elif fmt == "time":
        return dt.strftime("%H:%M:%S")
    else:
        return dt.strftime(fmt)


def camel_to_snake(name: str) -> str:
    """Convert camelCase or PascalCase to snake_case."""
    result = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", name)
    result = re.sub(r"([a-z\d])([A-Z])", r"\1_\2", result)
    return result.lower()


def snake_to_camel(name: str, pascal: bool = False) -> str:
    """Convert snake_case to camelCase (or PascalCase)."""
    components = name.split("_")
    if pascal:
        return "".join(x.capitalize() for x in components)
    return components[0] + "".join(x.capitalize() for x in components[1:])
