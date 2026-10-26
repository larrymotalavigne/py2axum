- `urlparse(url, scheme)` and `allow_fragments=False` were accepted but ignored; `.geturl()` of a URL without a
  netloc (`file:///x`, `http:x`) differed from CPython's; a bytes URL was read as its `repr`. All follow CPython now
  (bytes refused).
