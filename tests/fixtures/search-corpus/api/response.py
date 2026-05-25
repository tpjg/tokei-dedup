"""API response formatting and error handling."""

from typing import Any, Dict, List, Optional
import json
import traceback


def make_api_response(
    data: Any = None,
    status: int = 200,
    message: str = "ok",
    errors: Optional[List[str]] = None,
) -> Dict[str, Any]:
    """Build a standardized API response envelope."""
    response = {
        "status": status,
        "message": message,
    }
    if data is not None:
        response["data"] = data
    if errors:
        response["errors"] = errors
    return response


def make_error_response(
    status: int,
    message: str,
    details: Optional[str] = None,
    exception: Optional[Exception] = None,
) -> Dict[str, Any]:
    """Build a standardized error response."""
    response = {
        "status": status,
        "message": message,
        "data": None,
    }
    errors = []
    if details:
        errors.append(details)
    if exception:
        errors.append(str(exception))
    if errors:
        response["errors"] = errors
    return response


def paginate_response(
    items: List[Any],
    page: int = 1,
    per_page: int = 20,
) -> Dict[str, Any]:
    """Paginate a list and return a response with pagination metadata."""
    total = len(items)
    total_pages = (total + per_page - 1) // per_page
    page = max(1, min(page, total_pages)) if total_pages > 0 else 1
    start = (page - 1) * per_page
    end = start + per_page
    page_items = items[start:end]

    return make_api_response(
        data={
            "items": page_items,
            "pagination": {
                "page": page,
                "per_page": per_page,
                "total": total,
                "total_pages": total_pages,
                "has_next": page < total_pages,
                "has_prev": page > 1,
            },
        }
    )


def serialize_response(response: Dict[str, Any]) -> str:
    """Serialize a response dict to a JSON string with consistent formatting."""
    return json.dumps(response, indent=None, sort_keys=True, default=str)
