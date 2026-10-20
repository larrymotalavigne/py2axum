"""Scenario of fixtures/pydxapp: the Pydantic and FastAPI constructions of the documentation corpora (Iterable
fields...). No database. Compare the logs too (--logs): the 500s of an Iterable field's items print a traceback."""

import gzip

VALUES = [["a", "b"], [1, 2], "xy", "", {"k": 1, "j": 2}, 123, 1.5, True, None, [], [None], [["a"]], {"a": "b"},
          [1, "2", "x"], ["3", 4.0], [4.5], [True]]

STEPS: list = [("GET", "/health", None)]
for path, key in (("names", "f"), ("ints", "y"), ("maybe", "f"), ("strict", "y"), ("upper", "f"), ("plain", "y")):
    STEPS += [("POST", f"/iterx/{path}", {key: v}) for v in VALUES]
    STEPS.append(("POST", f"/iterx/{path}", {}))
STEPS += [("POST", "/iterx/maybe", {"tag": "x"}), ("POST", "/iterx/maybe", {"f": None, "tag": "y"})]
STEPS += [("POST", "/iterx/nested", {"rows": r}) for r in ([{"y": [1, 2]}, {"y": "34"}], [{"y": ["z"]}], [{}], 5, [[1]])]
for how in ("next", "next_default", "stop", "loop", "list", "dump_json_mode", "dump_json", "adapter", "strict",
            "partial"):
    STEPS += [("POST", "/iterx/use", {"how": how, "y": y}) for y in ([1, "2", 3], [1, "x", 3], "12", [], 7, ["9"])]
STEPS.append(("GET", "/iterx/schema", None))
STEPS += [("POST", "/coreschx/custom", b) for b in ({"my_field": 3}, {"my_field": "x"}, {"my_field": "4"}, {})]
STEPS += [("POST", "/coreschx/doubled", {"y": v}) for v in ("ab", "", 3, None, ["a"])]
STEPS += [("POST", f"/coreschx/{p}", {"value": v}) for p in ("compressed", "compressed/dump", "restricted", "small")
          for v in ("a b a", "CBA", "XYZ", "too long!!!!!", "ok", "", 5, None)]
STEPS += [("POST", "/coreschx/small", {"value": "ok", "other": o}) for o in (0, 5, 6, -1, "3", "x")]
STEPS += [("POST", "/coreschx/before", {"n": v}) for v in (" 7 ", "8", 9, " x ", None)]
STEPS += [("POST", "/coreschx/chained", {"n": v}) for v in (2, "3", "x", 1.5)]
STEPS += [("POST", "/coreschx/direct", {"v": v}) for v in ("AB", "ABZ", 3)]
JSON_DOCS = ['{"a": 1}', '[1,2]', "test", "not json", "", "{", '[1, "2"]', "1e400", "-1e400", "NaN", "Infinity", "-Infinity",
             "inf", '"\\ud800"', '"\\ud83d\\ude00"', "  [1]  ", "[1] x", '{"a":1,"a":2}', "0123", "[1,]", "tru", "nul", '"\\x"', "1.0",
             "-0", "\t[]\n", "[" * 300 + "]" * 300, '{"a": [1, {"b": null}]}', '{"a" 1}', '{1: 2}', "[1 2]", '"abc',
             "\u00e9", '"\u00e9"', "1.", ".5", "+1", "-", "1e", "[-0.0, 1E2, 2.5e-3]", '{"a":}', "true false"]
STEPS += [("POST", f"/jsonx/{p}", {"x": d}) for p in ("raw", "ints") for d in JSON_DOCS]
STEPS += [("POST", "/jsonx/raw", {"x": v}) for v in (1, None, 1.5, True, [], {"a": 1})]
STEPS += [("POST", "/jsonx/typed", b) for b in ({}, {"x": '{"k": [1]}', "y": "7"}, {"x": "[1]"}, {"y": '"7"'}, {"y": "x"},
                                                {"x": None}, {"y": 5})]
STEPS += [("POST", "/jsonx/direct", {"v": d, "bytes": bt}) for d in ('[1, "a"]', "nope", "", '{"a":') for bt in (False, True)]
ITEMS = [{"name": "a", "price": 1}, {"name": "a"}, [1], {"name": 1, "price": "x", "extra": 1}, "str", None, 3,
         {"name": "a", "price": 2, "tags": ["t", 1], "tax": "0.5"}, {"name": "a", "price": 2, "tags": "x"}, {}]
STEPS += [("POST", f"/dcx/{p}/", b) for p in ("items", "echo") for b in ITEMS]
STEPS += [("GET", f"/dcx/next?bad={b}", None) for b in (0, 1, 2)]
STEPS += [("POST", "/dcx/authors/x/items/", b) for b in ([{"name": "a"}, {"name": "b", "description": "d"}], [{"name": 2}],
                                                          [{"x": 1}], [5], {"name": "a"}, [])]
STEPS.append(("GET", "/dcx/authors/", None))
_GZ = {"content-encoding": "gzip", "content-type": "application/json"}
STEPS += [("POST", "/routex/gz/sum", gzip.compress(b"[1, 2, 3]"), _GZ), ("POST", "/routex/gz/sum", [4, 5]),
          ("POST", "/routex/gz/sum", gzip.compress(b"[1, \"x\"]"), _GZ), ("POST", "/routex/gz/sum", b"not gzip", _GZ),
          ("POST", "/routex/gz/echo/a?q=2", gzip.compress(b"hello"), {"content-encoding": "gzip"}),
          ("POST", "/routex/gz/echo/b", b"plain")]
STEPS += [("POST", "/routex/log/sum", b) for b in ([1, 2, 3], [1, "x"], "nope", {"a": 1}, None, b"[1, 2", b"")]
STEPS += [("GET", f"/routex/log/item/{i}", None) for i in ("5", "0", "x", "5?q=a")]
STEPS += [("GET", "/routex/hdr/timed", None), ("GET", "/routex/hdr/text", None), ("POST", "/routex/hdr/check", {"n": 3}),
          ("POST", "/routex/hdr/check", {"n": "x"}), ("GET", "/routex/hdr/missing", None), ("GET", "/routex/plain/", None),
          ("GET", "/routex/app/before", None), ("GET", "/routex/app/after", None)]

DTS = ["2032-04-23T10:20:30.400+02:30", "1999-04-23T10:20:30+00:00", "2032-04-23T10:20:30", 5, "x", "2000-01-01T00:00:00Z",
       "2000-01-01T01:00:00+01:00", "2000-01-01T00:00:00", "2000-01-01T06:00:00+00:00", "2000-01-01T09:00:00+00:00",
       "2000-01-01T06:00:00", "2000-01-01T05:06:08-03:30", "2030-01-01T00:00:00Z", "2029-12-31T23:59:59.999999+00:00",
       "2029-12-31T23:00:00-05:00", "2020-02-29T23:59:59.999999", "2020-03-01", "2000-01-01", 946684800, 9.5e8, None,
       [], "2005-01-01T00:00:00Z", "2005-01-01T00:00:00", "2010-06-15T12:00:00Z", "2010-06-15T11:59:59Z"]
STEPS += [("POST", f"/dtx/{p}", {k: v}) for p, k in (("event", "dt"), ("window", "at"), ("naive", "dt"), ("strict", "dt"))
          for v in DTS]
STEPS += [("POST", "/dtx/window", {"at": "2001-01-01T00:00:00", "east": v}) for v in DTS[6:12]]
STEPS += [("POST", "/dtx/naive", {"dt": "2001-01-01T00:00:00", "plain": v}) for v in DTS[13:18]]
STEPS += [("POST", "/dtx/day", {"d": v}) for v in ("1999-12-31", "2000-01-01", "2000-01-02", "2000-12-31", "2001-01-01",
                                                   "2000-01-01T00:00:00", "2000-06-01T00:00:00Z", "2000-06-01T01:00:00", 0,
                                                   978220800, "x", None)]
STEPS += [("GET", f"/dtx/since?after={a}" + (f"&before={b}" if b else ""), None)
          for a, b in (("2001-01-01T00:00:00", None), ("2001-01-01T00:00:01", "2029-12-31"), ("2002-01-01", "2030-01-01"),
                       ("x", "y"), ("2000-12-31T23:00:00-02:00", None), ("2001-01-01T00:00:00%2B00:00", None))]

STEPS += [("POST", "/postx/doubled", {"x": v}) for v in (3, 0, 1, -1, "x", 5, 2)]
STEPS += [("POST", "/postx/outer", b) for b in ({"inner": {"x": 4}, "items": [{"x": 1}, {"x": 6, "z": "w"}]},
                                                 {"inner": {"x": 0}, "items": [{"x": 0}, {"x": -2}]}, {"inner": {"x": 7}},
                                                 {"inner": {"x": 3}, "items": [{"x": 8}, {"x": 9}]})]
STEPS += [("POST", "/postx/direct", b) for b in ({"x": 4}, {"x": 0}, {"x": 1, "z": "q"}, {"x": "a"}, {})]
STEPS += [("POST", "/postx/only", b) for b in ({"n": 1}, {"n": "x"})]

STEPS += [("POST", "/cfgx/parent", b) for b in ({"user_1": {"name": "ABC"}, "user_2": {"name": "ABC"}},
                                                {"user_1": {"name": 1}, "user_2": {}}, {"user_1": "x", "user_2": {"name": "Ok"}})]
STEPS += [("POST", "/cfgx/rich", b) for b in (
    {"items": [{"label": "ab", "tags": ["x", "yy"], "color": "blue"}, {"label": "abcdefgh"}]},
    {"one": {"label": "lo", "owner": {"name": "who"}, "color": "red"}, "strip": {"name": "  Mixed  "}, "pyd": {"name": "keepCase"}},
    {"one": {"label": "lo", "tags": ["toolongtag"], "color": "green"}}, {"strip": {"name": " longer than six "}},
    {"pyd": {"name": "much longer than six"}}, {"one": {"label": "x", "owner": {"name": "abcdefgh"}}})]
STEPS += [("POST", "/cfgx/plain", b) for b in ({"name": "AbC"}, {"name": 5}, {})]

_AUTH = [None, "Bearer abc", "bearer  x", "Basic abc", "Bearer", "Token abc", "Bearer a b"]
STEPS += [("GET", f"/secx/{p}", None, {"authorization": a} if a else {}) for p in ("me", "me2", "tea") for a in _AUTH]
STEPS += [("GET", "/secx/basic", None, {"authorization": a} if a else {})
          for a in (None, "Basic dTpw", "Basic !!", "Basic dXNlcg==", "Bearer x", "Basic w6k6cA==")]
for path in ("users/me", "users/items", "status"):
    STEPS += [("GET", f"/secx/{path}", None, {"authorization": a} if a else {})
              for a in (None, "Bearer ann:me", "Bearer bob:me,items", "Bearer root:me,items,base,admin", "Bearer x:items", "Bearer y:")]
STEPS += [("GET", f"/secx/items/{q}", None) for q in ("", "?id=isbn-1", "?id=wtf-yes", "?id=imdb-2&tag=ab", "?tag=", "?id=")]
STEPS += [("GET", f"/secx/items/{n}", None, h) for n, h in (("3", {}), ("0", {}), ("-2", {}), ("x", {}), ("4?q=3&q=1&q=2", {}),
                                                            ("5", {"x-token": "isbn-ok"}), ("5", {"x-token": "nope"}),
                                                            ("0?q=z", {"x-token": "nope"}))]
STEPS += [("POST", "/secx/video", {"data": "Fake video bytes\nline 2\n", "n": 3}), ("GET", "/secx/video", None),
          ("POST", "/secx/video", {"data": "no newline"}), ("GET", "/secx/video", None), ("GET", "/secx/letters", None),
          ("POST", "/secx/video", {"data": "a\r\nb\rc\n\n"}), ("GET", "/secx/video", None)]
STEPS += [("POST", "/secx/named", b) for b in ({"name": "ABC"}, {"name": "x", "title": "imdb-1", "short": "XYZW"},
                                              {"name": 3}, {"name": "x", "title": "bad"}, {})]

STEPS += [("GET", f"/respx/{p}", None) for p in ("orjson", "api", "api/0", "api/3", "shout", "shout?q=abc d", "xml", "empty",
                                                 "image/stream", "image/stream-no-async", "image/stream-no-async-yield-from",
                                                 "image/stream-no-annotation")]

STEPS += [("POST", f"/cmx/{p}", b) for p in ("user", "user/dump") for b in (
    {"first_name": "John", "last_name": "Doe"}, {"first_name": "J%hn", "last_name": "D e"}, {"first_name": 1}, {})]
STEPS += [("POST", f"/cmx/{p}", b) for p in ("line", "direct") for b in (
    {"price": 3, "sku": "ab"}, {"price": 30, "qty": 4, "sku": "abc"}, {"price": -1, "sku": "a"}, {"price": 2, "sku": "toolong"},
    {"price": "x", "qty": "y", "sku": "ok", "note": 5}, {})]

STEPS += [("POST", "/annx/strict", b) for b in (
    {"s": "x"}, {"s": 1}, {"s": "x", "i": "1"}, {"s": "x", "i": 1.0}, {"s": "x", "i": True}, {"s": "x", "f": 1}, {"s": "x", "f": "1.5"},
    {"s": "x", "b": 1}, {"s": "x", "b": "true"}, {"s": "x", "b": False}, {"s": "x", "b": None}, {"s": "x", "y": "ab"},
    {"s": "x", "o": 5}, {"s": "x", "ls": [1, "2"]}, {"s": "x", "sm": "a"}, {"s": "x", "sm": "abcde"}, {"s": "x", "sm": 12}, {})]
STEPS += [("POST", "/annx/paths", b) for b in (
    {"p": "a/b"}, {"p": ""}, {"p": "/abs//x/../y/"}, {"p": 5}, {"p": None}, {"p": ["a"]}, {"p": "a", "o": "b/c.txt"}, {"p": "a", "o": 3},
    {"p": "a", "u": "x/y"}, {"p": "a", "u": 7}, {"p": "a", "u": 1.5}, {"p": "a", "u": []}, {"p": "a", "s": "t"}, {"p": "a", "ls": ["x", "y/z"]},
    {"p": "a", "ls": ["x", 1]}, {"p": "./a/./b/"}, {"p": "a.tar.gz"}, {})]
PG = "postgresql://u:p@localhost:5432/db"
STEPS += [("POST", "/annx/dsns", b) for b in (
    {"pg": PG}, {"pg": "postgresql+asyncpg://u@h/db?x=1#f"}, {"pg": "postgres://h"}, {"pg": "postgresql://"}, {"pg": "postgresql:///db"},
    {"pg": "mysql://h"}, {"pg": "postgresql://h:abc/db"}, {"pg": ""}, {"pg": 5}, {"pg": None}, {"pg": "postgresql://H/Db"},
    {"pg": "postgresql://u%40x:p%2Fw@h:99999/db"}, {"pg": "postgresql://h/db", "mongo": "mongodb://h"}, {"pg": PG, "mongo": "mongodb+srv://c.example"},
    {"pg": PG, "mongo": "http://h"}, {"pg": PG, "amqp": "amqp://g:g@h/v"}, {"pg": PG, "amqp": "amqps://h"}, {"pg": PG, "amqp": "x://h"},
    {"pg": PG, "kafka": "kafka://"}, {"pg": PG, "kafka": "kafka://k:1"}, {"pg": PG, "mysql": "mysql+pymysql://u@h/d"}, {"pg": PG, "mysql": "mysql://"},
    {"pg": PG, "mysql": "postgresql://h"}, {})]
STEPS += [("POST", "/annx/signs", b) for b in (
    {}, {"p": 0}, {"p": -1}, {"p": "5"}, {"n": 0}, {"n": -3}, {"nn": -1}, {"nn": 0}, {"np": 1}, {"np": 0}, {"pf": 0}, {"pf": 0.1},
    {"nf": -0.5}, {"nf": "x"}, {"tight": 2}, {"tight": 10}, {"tight": 9}, {"tight": 3})]
STEPS += [("POST", "/annx/con", b) for b in (
    {}, {"s": "a"}, {"s": "abcdef"}, {"up": "  hi  "}, {"up": "   "}, {"pat": "ABC"}, {"pat": "abc"}, {"strict_s": 5}, {"strict_s": ""},
    {"maybe": "abcd"}, {"maybe": "abcde"}, {"maybe": None}, {"i": 0}, {"i": 11}, {"i": 10}, {"mult": 4}, {"mult": "3"}, {"mult": 6},
    {"f": 0}, {"f": 1}, {"f": 0.25}, {"d": "1.234"}, {"d": "123456"}, {"d": "12.3"}, {"d": 1.5}, {"lst": []}, {"lst": [1, 2, 3, 4]},
    {"lst": ["x"]}, {"lst": "ab"}, {"tags": ["ab", "c"]}, {"st": [1, 2, 3]}, {"st": [1, 1]}, {"st": ["a"]})]
STEPS += [("POST", "/annx/con/users", b) for b in ([1], [], [1, 2, 3], ["a"], 5, None)]
STEPS += [("GET", f"/annx/generic{p}", None) for p in ("", "/list", "/list?n=0", "/list?n=x", "/bad", "/page", "/module")]
STEPS += [("POST", "/annx/generic/in", b) for b in (
    {"data": {"id": 1, "name": "a"}}, {"code": "x", "data": {"id": "y"}}, {"data": None}, {"data": 5}, {}, [])]


# aliasx: alias generators of several statements, `Other.model_config["alias_generator"]`, `**Base.model_config`,
# hide_input_in_errors
_TASKS = ({"taskName": "t", "cronExpression": "* * *", "accountId": 3}, {"task_name": "t", "cron_expression": "c", "account_id": 1,
          "configJson": {"a": 1}, "enabled": False, "aB": 2}, {"taskName": "", "cronExpression": 1, "accountId": 0},
          {"taskName": "t", "cronExpression": "c", "accountId": 2, "unknown": 1}, {"taskName": "t", "cron_expression": "c",
          "accountId": "x", "configJson": "s", "a__b": 5}, {}, [])
STEPS += [("POST", f"/aliasx/{p}", b) for p in ("task", "task/raw") for b in _TASKS]
STEPS += [("POST", "/aliasx/dashed", b) for b in ({"X-ray-id": 1}, {"x_ray_id": 1}, {"X-ray-id": "2", "long-field-name": "z",
                                                  "plain": True}, {"X-ray-id": "q", "long-field-name": 3})]
STEPS += [("POST", f"/aliasx/{p}", b) for p in ("item", "item/dump", "strict") for b in (
    {"itemId": 1, "displayName": "n"}, {"item_id": 2, "display_name": "m"}, {"itemId": "x"}, {"pageSize": 3},
    {"page_size": 4, "other": 1}, {})]
STEPS += [("POST", "/aliasx/secret", b) for b in ({"pass_code": 1}, {"pass_code": "hunter2", "nested_list": [1, "x"]}, {})]
STEPS += [("POST", f"/aliasx/hidden/{w}", b) for w in ("secret", "outer", "shown", "child") for b in (
    {"pass_code": "secret-" * 9, "nested_list": ["a", 2]}, {"inner": {"value": "nope"}}, {"secret": {"pass_code": "x"}},
    {"pass_code": 5}, {"inner": {"value": 1}}, {"secret": {"pass_code": 9}}, {"extra_field": 3})]
STEPS += [("GET", f"/aliasx/settings/{w}", None) for w in ("ignore", "keep", "required")]
STEPS += [("POST", "/aliasx/finite", b) for b in (
    {"x": 1.5, "xs": [1, 2.5], "z": {"a": 3}}, {"x": "inf"}, {"x": float("nan"), "xs": [1, "-inf"]}, {"x": 1e309},
    {"x": "Infinity", "y": "nan", "z": {"k": float("-inf")}}, {"x": "-inf", "xs": ["nan", 2], "z": {"k": "inf"}}, {"x": 1, "y": -1}, {"x": 1, "inner": {"value": 2}})]


def reset(db: str) -> None:
    pass
