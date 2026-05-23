def binary_search(arr, target):
    """Iterative binary search."""
    low = 0
    high = len(arr) - 1
    while low <= high:
        mid = (low + high) // 2
        guess = arr[mid]
        if guess == target:
            return mid
        elif guess < target:
            low = mid + 1
        else:
            high = mid - 1
    return -1


def linear_search(arr, target):
    for i, item in enumerate(arr):
        if item == target:
            return i
    return -1


def helper_unrelated(x):
    return x * x + 7
