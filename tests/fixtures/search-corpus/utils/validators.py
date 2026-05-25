"""Validation utilities used across the application."""

import re
from typing import Optional


def verify_email_format(address: str) -> bool:
    """Check whether an email address is syntactically valid per RFC 5321."""
    if not address or not isinstance(address, str):
        return False
    parts = address.strip().split("@")
    if len(parts) != 2:
        return False
    local_part, domain = parts
    if not local_part or not domain:
        return False
    if len(local_part) > 64 or len(domain) > 255:
        return False
    if ".." in domain or domain.startswith(".") or domain.endswith("."):
        return False
    domain_parts = domain.split(".")
    if len(domain_parts) < 2:
        return False
    for part in domain_parts:
        if not part or len(part) > 63:
            return False
        if not re.match(r"^[a-zA-Z0-9]([a-zA-Z0-9-]*[a-zA-Z0-9])?$", part):
            return False
    return True


def validate_phone_number(phone: str, country_code: str = "US") -> bool:
    """Validate a phone number for a given country code."""
    cleaned = re.sub(r"[\s\-\(\)\.]", "", phone)
    if not cleaned:
        return False
    if cleaned.startswith("+"):
        cleaned = cleaned[1:]
    if not cleaned.isdigit():
        return False
    if country_code == "US":
        return len(cleaned) == 10 or (len(cleaned) == 11 and cleaned.startswith("1"))
    elif country_code == "UK":
        return len(cleaned) == 10 or len(cleaned) == 11
    else:
        return 7 <= len(cleaned) <= 15


def sanitize_url(url: str) -> Optional[str]:
    """Sanitize and normalize a URL. Returns None if invalid."""
    if not url or not isinstance(url, str):
        return None
    url = url.strip()
    if not url.startswith(("http://", "https://")):
        url = "https://" + url
    # Remove trailing slashes for consistency
    url = url.rstrip("/")
    # Basic structure check
    pattern = r"^https?://[a-zA-Z0-9]([a-zA-Z0-9\-]*[a-zA-Z0-9])?(\.[a-zA-Z0-9]([a-zA-Z0-9\-]*[a-zA-Z0-9])?)*(/.*)?$"
    if not re.match(pattern, url):
        return None
    return url


def validate_password_strength(password: str) -> dict:
    """Check password strength and return detailed feedback."""
    result = {
        "valid": True,
        "score": 0,
        "feedback": [],
    }
    if len(password) < 8:
        result["valid"] = False
        result["feedback"].append("Password must be at least 8 characters")
    if len(password) >= 12:
        result["score"] += 2
    elif len(password) >= 8:
        result["score"] += 1
    if re.search(r"[A-Z]", password):
        result["score"] += 1
    else:
        result["feedback"].append("Add uppercase letters")
    if re.search(r"[a-z]", password):
        result["score"] += 1
    else:
        result["feedback"].append("Add lowercase letters")
    if re.search(r"\d", password):
        result["score"] += 1
    else:
        result["feedback"].append("Add numbers")
    if re.search(r"[!@#$%^&*(),.?\":{}|<>]", password):
        result["score"] += 1
    else:
        result["feedback"].append("Add special characters")
    return result
