- `it = iter(x)` kept in a variable: `next(it)` advances it, a `for` loop takes one item at a time (a `break` leaves
  the rest), `list(it)` the rest. A list changed after `iter()` is not seen.
