- `request.stream()`: the body (one chunk, the binary has it whole) then b""; `body()` raises "Stream consumed"
  afterwards, as Starlette does.
