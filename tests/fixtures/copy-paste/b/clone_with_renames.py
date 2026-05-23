def bsearch(items, needle):
    """Iterative binary search, identifiers renamed (Type-2 clone of original.py::binary_search)."""
    lo = 0
    hi = len(items) - 1
    while lo <= hi:
        m = (lo + hi) // 2
        cur = items[m]
        if cur == needle:
            return m
        elif cur < needle:
            lo = m + 1
        else:
            hi = m - 1
    return -1


def something_else(arr, target):
    # Unrelated code so the file isn't a 100% copy.
    total = 0
    for v in arr:
        total += v
    return total / len(arr) if arr else 0
