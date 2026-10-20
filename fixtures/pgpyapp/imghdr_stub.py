"""The `imghdr` stub pgpy needs on Python 3.13+ (its constants import it, its parsing never calls it)."""
import sys
import types

if "imghdr" not in sys.modules:
    stub = types.ModuleType("imghdr")
    stub.what = lambda *a, **kw: None
    sys.modules["imghdr"] = stub
