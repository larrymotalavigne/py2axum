- `pickle.DEFAULT_PROTOCOL` (4, 5 from Python 3.14) and `pickle.HIGHEST_PROTOCOL`: aiocache's `PickleSerializer`
  translates. fixtures/libsrcapp checks aiocache end to end (memory and Redis caches, serializers, `@cached`).
