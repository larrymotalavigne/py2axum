- `type(x)` of a defaultdict or Counter (a copy of one included) is `collections.defaultdict` /
  `collections.Counter`, not `dict`; `iter(it) is it` is true for a stored iterator.
