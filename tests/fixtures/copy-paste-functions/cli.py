"""Command-line entry point. Different file, same arg parser."""

import sys


def main():
    args = read_cli_args(sys.argv[1:])
    if "help" in args:
        print("usage: tool [--flag value] ...")
        return
    run(args)


def read_cli_args(arguments):
    result = {}
    index = 0
    while index < len(arguments):
        item = arguments[index]
        if item.startswith("--"):
            name = item[2:]
            val = arguments[index + 1] if index + 1 < len(arguments) else True
            result[name] = val
            index += 2
        else:
            result.setdefault("positional", []).append(item)
            index += 1
    return result


def run(args):
    print("running with", args)
