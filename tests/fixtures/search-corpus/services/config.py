"""Configuration loading and management."""

import json
import os
from typing import Any, Dict, Optional


def load_config(
    config_path: str,
    env_prefix: str = "APP_",
    defaults: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Load configuration from a JSON file with environment variable overrides."""
    config = dict(defaults) if defaults else {}

    if os.path.exists(config_path):
        with open(config_path) as f:
            file_config = json.load(f)
        config.update(file_config)

    for key, value in os.environ.items():
        if key.startswith(env_prefix):
            config_key = key[len(env_prefix):].lower()
            # Try to parse as JSON for non-string types
            try:
                config[config_key] = json.loads(value)
            except (json.JSONDecodeError, ValueError):
                config[config_key] = value

    return config


def get_nested(config: Dict[str, Any], key_path: str, default: Any = None) -> Any:
    """Get a nested config value using dot notation: 'database.host'."""
    keys = key_path.split(".")
    current = config
    for key in keys:
        if not isinstance(current, dict) or key not in current:
            return default
        current = current[key]
    return current


def merge_configs(*configs: Dict[str, Any]) -> Dict[str, Any]:
    """Deep-merge multiple config dicts. Later values override earlier ones."""
    result = {}
    for config in configs:
        for key, value in config.items():
            if (
                key in result
                and isinstance(result[key], dict)
                and isinstance(value, dict)
            ):
                result[key] = merge_configs(result[key], value)
            else:
                result[key] = value
    return result
