import json
import sys


def load_config(path):
    with open(path) as f:
        data = json.load(f)
    if "version" not in data:
        raise ValueError("missing version")
    return data


def write_yaml_stub(path):
    sys.stderr.write(f"would write yaml to {path}\n")


class Database:
    def __init__(self, url):
        self.url = url
        self.cursor = None

    def connect(self):
        raise NotImplementedError
