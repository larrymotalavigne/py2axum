"""Scenario of fixtures/docapp: openpyxl workbooks compared byte for byte (tests/docmask.py masks the zip's
DOS times and docProps/core.xml's instants only), the report exports, then programs of workbook operations
drawn from a fixed seed (styles shared or not, merges over styled cells, widths, sheets and their titles,
values of every type, errors). No database."""
import html.parser
import random
import re

STEPS: list = [("GET", "/health", None)]
STEPS += [("GET", f"/x/report?n={n}" + q, None) for n in (0, 1, 4, 12) for q in ("", "&title=", "&fields=rent,address")]
STEPS += [("GET", f"/x/stats?n={n}", None) for n in (0, 4, 9)]
STEPS += [("POST", f"/t/attach/{y}?size={n}", None) for y in (2025, 2026) for n in (0, 300, 70000)]
STEPS += [("POST", "/t/scratch?text=" + t, None) for t in ("", "abc", "%C3%A9t%C3%A9")]

rnd = random.Random(20261018)
TEXT = ["x", " a ", "   ", "", "Bât. A & <B> \"q\" 'p'", "été", "=SUM(A1:A3)", "=", "#N/A", "#REF!", "a\nb\tc\r",
        "😀 ok", "x" * 40, "\x01bad", "trail ", "10", "'=x"]
NUMS = [0, 1, -1, 7, 2**53 + 1, 12345678901234, 0.1, 0.30000000000000004, -0.0, 1e-7, 1e21, 123.456, 1.5e300, True, False]
COLORS = ["FFFFFF", "3498db", "2C3E50", "00FF0000", "ff00ff00", "red", "12345"]
SIDES = [None, "thin", "medium", "dashed", "double"]


def value():
    r = rnd.random()
    return rnd.choice(TEXT) if r < 0.5 else rnd.choice(NUMS) if r < 0.9 else None


def coord(rows=6, cols=5):
    return f"{'ABCDE'[rnd.randrange(cols)]}{rnd.randrange(1, rows + 1)}"


def side():
    s = {"style": rnd.choice(SIDES)}
    if rnd.random() < 0.3:
        s["color"] = rnd.choice(COLORS[:5])
    return s


def style():
    k = rnd.choice(["font", "fill", "align", "border"])
    if k == "font":
        spec = {"kind": "font", "bold": rnd.choice([None, True, False]), "italic": rnd.choice([None, True, False]),
                "size": rnd.choice([None, 11, 12.5, 16]), "color": rnd.choice([None] + COLORS), "name": rnd.choice([None, "Arial", "A&B"]),
                "strike": rnd.choice([None, True])}
        return "font", spec
    if k == "fill":
        return "fill", {"kind": "fill", "fill_type": rnd.choice([None, "solid", "gray125", "none"]),
                        "start_color": rnd.choice([None, "00000000"] + COLORS[:5]), "end_color": rnd.choice([None] + COLORS[:5])}
    if k == "align":
        return "alignment", {"kind": "align", "horizontal": rnd.choice([None, "center", "left", "general"]),
                             "vertical": rnd.choice([None, "center", "top"]), "wrap_text": rnd.choice([None, True, False]),
                             "shrink_to_fit": rnd.choice([None, True])}
    spec = {"kind": "border"}
    for n in ("left", "right", "top", "bottom", "diagonal"):
        if rnd.random() < 0.6:
            spec[n] = side()
    return "border", spec


def program():
    ops, merged = [], False
    for _ in range(rnd.randrange(1, 14)):
        r = rnd.random()
        if r < 0.25:
            ops.append(["cell", rnd.randrange(1, 7), rnd.randrange(1, 6), value()])
        elif r < 0.35:
            ops.append(["set", coord(), value()])
        elif r < 0.38:
            ops.append(["dec", coord(), rnd.choice(["12.50", "-0.001", "1E+3", "3"])])
        elif r < 0.6:
            attr, spec = style()
            ops.append(["style", coord(), attr, spec])
        elif r < 0.66 and not merged:
            merged = True
            a, b = sorted([rnd.randrange(1, 5), rnd.randrange(1, 5)]), sorted([rnd.randrange(1, 6), rnd.randrange(1, 6)])
            if rnd.random() < 0.5:
                ops.append(["merge", f"{'ABCDE'[a[0] - 1]}{b[0]}:{'ABCDE'[a[1] - 1]}{b[1]}"])
            else:
                ops.append(["mergekw", b[0], a[0], b[1], a[1]])
        elif r < 0.72:
            ops.append(["width", rnd.choice("ABCDE") if rnd.random() < 0.9 else "c", rnd.choice([0, 8, 14, 20.5, 50])])
        elif r < 0.75:
            ops.append(["dim", rnd.choice("ABC")])
        elif r < 0.79:
            ops.append(["sheet"] + ([rnd.choice(["Sheet", "sheet", "Sheet1", "Data", "Stats", "a/b", "Ré & <x>"])] if rnd.random() < 0.7 else []))
            merged = False
        elif r < 0.82:
            ops.append(["title", rnd.choice(["Data", "Sheet", "SHEET", "x[1]", "", "Sheet2", "Résumé \"q\""])])
        elif r < 0.86:
            ops.append(["append", [value() for _ in range(rnd.randrange(0, 4))]])
        elif r < 0.89:
            ops.append(["col", rnd.choice("ABC")])
        elif r < 0.91:
            ops.append(["rows", rnd.randrange(1, 4), rnd.randrange(1, 5)])
        elif r < 0.93:
            ops.append(["allrows"])
        elif r < 0.96:
            ops.append(["read", coord()])
        elif r < 0.98:
            ops.append(["extent"])
        else:
            ops.append(["letter", rnd.choice([1, 26, 27, 702, 703, 18278, 0, 18279])])
    ops.append(["names"])
    return ops


STEPS += [("POST", "/x/ops", program()) for _ in range(400)]
# security review 2 (docs/advanced/security.md): a numeric sheet-title suffix past 64 bits (it wrapped to 0: a
# duplicate title), and one distinct style per row (the styles were found by a linear search: quadratic)
STEPS += [("POST", "/x/ops", [["sheet", "Sheet0"], ["sheet", "Sheet99999999999999999999"], ["sheet", "Sheet"],
                              ["sheet", "sheet"], ["sheet", "Sheet"], ["names"]]),
          ("POST", "/x/ops", [*[["style", f"A{i}", "font", {"kind": "font", "color": f"FF{i:06X}", "size": 11}]
                                for i in range(1, 3001)], ["extent"], ["names"]])]

# reportlab (fixtures/docapp/pdfs.py): PDFs compared byte for byte but /CreationDate, /ModDate and /ID (tests/docmask.py).
# Paragraphs need CPython's HTML5 html.parser (3.12.12, 3.13.8 on; Debian trixie's 3.13.5 has a partial backport): on
# another, the binary refuses them (a py2axum TypeError, docs: Generated documents), so these steps run only where the
# reference's parser has it
HTML5_PARSER = hasattr(html.parser.HTMLParser, "_set_support_cdata")
Q = ["", "%C3%A9t%C3%A9%20%26%20co", "A%20%26%20B%20%3C%20C", "Ren%C3%A9e%20%3Cb%3EO%27Hara%3C%2Fb%3E", "x" * 300,
     "%E2%80%94%20%E2%82%AC%20%C2%AB%20%C2%BB", "a%26%23", "%26lt%3Bi%26gt%3B", "%3Cb%3Eunclosed"]
PDF_STEPS: list = [("GET", "/p/values", None)]
PDF_STEPS += [("GET", f"/p/errors/{c}", None) for c in ("style", "add", "markup", "tag", "layout", "font", "callback", "polygon",
                                                      "caught", "none")]
PDF_STEPS += [("GET", f"/p/notice?name={q}&months={m}&furnished={f}&reason={r}&details={q}", None)
          for q in Q for m, f, r in ((3, "false", ""), (1, "false", "job_loss"), (1, "true", "x"))]
PDF_STEPS += [("GET", f"/p/letter/{k}?landlord={q}&tenant={q}&address={q}&rent={r}", None)
          for k in ("echeance", "assurance") for q in Q[:6] for r in (950, 1234.5)]
PDF_STEPS += [("GET", f"/p/branded?n={n}&later={l}&pagesize={p}&meta={m}&logo={lg}", None)
          for n, l, p, m, lg in ((0, "true", "A4", "", 1.35), (3, "true", "A4", "x", 1.5), (9, "true", "letter", Q[2], 2),
                                 (14, "false", "landscape", Q[3], 1), (5, "true", "A4", "Quittance%3Cbr%2F%3E2026", 3))]
PDF_STEPS += [("GET", f"/p/lease?landlord={q}&clauses={c}&furnished={f}&extra={e}", None)
          for q, c, f, e in ((Q[0], 0, "false", ""), (Q[1], 6, "true", ""), (Q[2], 25, "false", Q[3]), (Q[4], 12, "true", Q[1]))]
PDF_STEPS += [("GET", f"/p/export?n={n}&owner={q}", None) for n, q in ((0, ""), (4, Q[1]), (40, Q[2]), (120, Q[5]))]
PDF_STEPS += [("GET", f"/p/receipt?tenant={q}&rows={n}&meta={m}", None)
          for q, n, m in ((Q[0], 1, ""), (Q[1], 3, Q[3]), (Q[2], 30, "x"), (Q[4], 60, Q[1]), (Q[5], 0, Q[2]))]

MARKUP = ["Texte simple", "<b>gras</b> et <i>italique</i>", "ligne<br/>suivante<br/>", "<font size=8>petit</font> normal",
          '<font color="#ff0000">rouge</font> <font name="Courier">c</font>', "A &amp; B &lt; C", "A & B", "a < b > c",
          "été à Noël — 12 € « citation »", "&eacute;t&eacute; &#233; &#xE9; &nbsp;fin", "mot " * 120, "<!-- commentaire --> après", "a<!--->b-->c", "<i>" + "très long texte justifié " * 30 + "</i>",
          "<font size=14 color=blue>Titre</font><br/><font size=7>sous-titre</font>", "&unknown; z", "tab\tet\nretour"]
# markup reportlab refuses (unclosed tag; `&#` not followed by a number: unclosed before CPython 3.14.1), drawn rarely
BAD_MARKUP = ["<b>non fermé", "x &#; y", "<font size=x>a</font>"]
FONTS = ["Helvetica", "Helvetica-Bold", "Times-Roman", "Times-Italic", "Courier", "Courier-BoldOblique", "Symbol"]
CLRS = [None, "black", "grey", "red", "#4F46E5", "#e5e7eb", [0.2, 0.4, 0.6], "lightgrey"]
PARENTS = ["Normal", "BodyText", "Title", "Heading1", "Heading2", "Heading6", "Italic", "Code", "h3"]


def style_attrs():
    attrs = {}
    for k, vals in (("fontName", FONTS[:6]), ("fontSize", [7, 8.5, 10, 13, 18]), ("leading", [9, 12, 14.5, 22]),
                    ("alignment", [0, 1, 2, 4]), ("spaceBefore", [0, 3, 12.5]), ("spaceAfter", [0, 6, 20]),
                    ("leftIndent", [0, 10, 36]), ("rightIndent", [0, 15]), ("firstLineIndent", [0, 12, -6]),
                    ("textColor", CLRS[1:]), ("allowOrphans", [0, 1]), ("allowWidows", [0, 1])):
        if rnd.random() < 0.3:
            attrs[k] = rnd.choice(vals)
    return attrs


def shape():
    k = rnd.choice(["Rect", "Circle", "Polygon", "Group"])
    kw = {}
    if k != "Group":
        for a, vals in (("fillColor", CLRS), ("strokeColor", CLRS), ("strokeWidth", [0.5, 1, 2])):
            if rnd.random() < 0.5:
                kw[a] = rnd.choice(vals)
    if k == "Rect":
        return [k, [rnd.randrange(0, 40), rnd.randrange(0, 40), rnd.randrange(1, 60), rnd.randrange(1, 60)], kw]
    if k == "Circle":
        return [k, [rnd.randrange(0, 60), rnd.randrange(0, 60), rnd.randrange(1, 30)], kw]
    if k == "Polygon":
        return [k, [rnd.randrange(0, 80) for _ in range(rnd.choice([6, 8, 10]))], kw]
    g = {"scale": [rnd.choice([0.5, 1, 1.5]), rnd.choice([0.5, 1, 2])]} if rnd.random() < 0.5 else {}
    if rnd.random() < 0.3:
        g["translate"] = [rnd.randrange(-10, 10), rnd.randrange(-10, 10)]
    return [k, [shape() for _ in range(rnd.randrange(0, 3))], g]


def table():
    nr, nc = rnd.randrange(1, 25), rnd.randrange(1, 4)
    cells = [[rnd.choice(["", "abc", "12,50 €", "Désignation longue " * 2, None, ["P", rnd.choice(MARKUP[:10]), "Normal"]])
              for _ in range(nc)] for _ in range(nr)]
    widths = rnd.choice([None, [rnd.choice([None, 60, 120, "30%"]) for _ in range(nc)]])
    cmds = []
    for _ in range(rnd.randrange(0, 6)):
        op = rnd.choice(["GRID", "BOX", "BACKGROUND", "TEXTCOLOR", "ALIGN", "VALIGN", "FONTNAME", "FONTSIZE", "LEFTPADDING",
                         "ROWBACKGROUNDS", "LINEBELOW", "SPAN"])
        a, b = [rnd.randrange(0, nc), rnd.randrange(0, nr)], rnd.choice([[-1, -1], [nc - 1, 0], [0, -1]])
        arg = {"GRID": [0.5, "grey"], "BOX": [1, "#4F46E5"], "BACKGROUND": ["#EEF2FF"], "TEXTCOLOR": ["red"],
               "ALIGN": [rnd.choice(["LEFT", "RIGHT", "CENTER"])], "VALIGN": [rnd.choice(["TOP", "MIDDLE", "BOTTOM"])],
               "FONTNAME": [rnd.choice(FONTS[:4])], "FONTSIZE": [rnd.choice([7, 9, 12])], "LEFTPADDING": [rnd.choice([0, 4, 10])],
               "ROWBACKGROUNDS": [["white", "#f8fafc"]], "LINEBELOW": [1.2, "black"], "SPAN": []}[op]
        cmds.append([op, a, b] + arg)
    return ["T", cells, widths, rnd.choice([0, 0, 1]), cmds]


def pdf_program():
    styles = []
    for i in range(rnd.randrange(0, 4)):
        parent = rnd.choice(PARENTS + [s[0] for s in styles])
        styles.append([f"S{i}", parent, style_attrs()])
    names = PARENTS + [s[0] for s in styles]
    story = []
    for _ in range(rnd.randrange(1, 18)):
        r = rnd.random()
        if r < 0.5:
            story.append(["P", rnd.choice(BAD_MARKUP if rnd.random() < 0.02 else MARKUP), rnd.choice(names)])
        elif r < 0.62:
            story.append(["S", rnd.choice([0, 1, 10]), 900 if rnd.random() < 0.02 else rnd.choice([0, 6, 30, 200])])
        elif r < 0.67:
            story.append(["B"])
        elif r < 0.75:
            kw = {k: rnd.choice(v) for k, v in (("width", ["100%", "50%", 200, 600]), ("thickness", [0.5, 1, 3]),
                                                  ("color", CLRS[1:]), ("hAlign", ["LEFT", "CENTER", "RIGHT"]),
                                                  ("spaceAfter", [0, 6]), ("dash", [None, [2, 2], 3])) if rnd.random() < 0.6}
            story.append(["H", kw])
        elif r < 0.82:
            story.append(["D", rnd.choice([40, 80, 160]), rnd.choice([30, 60]), [shape() for _ in range(rnd.randrange(1, 4))],
                          rnd.choice(["LEFT", "CENTER", "RIGHT"])])
        elif r < 0.92:
            story.append(table())
        elif styles:
            story.append(["M", rnd.choice(styles)[0], rnd.choice(["fontSize", "leading", "spaceBefore"]), rnd.choice([8, 16, 30])])
    def footer():
        ops, depth = [], 0
        for _ in range(rnd.randrange(0, 8)):
            k = rnd.choice(["save", "restore", "font", "fill", "stroke", "gray", "width", "line", "rect", "text"])
            if k == "restore" and depth == 0 and rnd.random() < 0.9:
                k = "save"
            depth += {"save": 1, "restore": -1}.get(k, 0)
            ops.append({"save": ["save"], "restore": ["restore"],
                        "font": ["font", "Nope" if rnd.random() < 0.02 else rnd.choice(FONTS), rnd.choice([7, 9.5])],
                        "fill": ["fill", rnd.choice(CLRS[1:])], "stroke": ["stroke", rnd.choice(CLRS[1:])],
                        "gray": ["gray", rnd.choice([0, 0.5, 1])], "width": ["width", rnd.choice([0.25, 1, 2])],
                        "line": ["line", 40, 40, 300, 40], "rect": ["rect", 30, 30, 100, 20, rnd.choice([0, 1]), rnd.choice([0, 1])],
                        "text": ["text", 50, 30, rnd.choice(["Page $P", "$D / x", "été — €", "a & <b>"]), rnd.choice(["l", "r", "c"])]}[k])
        return ops
    prog = {"doc": {"size": rnd.choice(["A4", "letter"]), "margins": [rnd.choice([36, 72, 56.7]) for _ in range(4)],
                    "title": rnd.choice([None, "Doc", "Résumé & co"]), "author": rnd.choice([None, "Moi"])},
            "styles": styles, "story": story}
    if rnd.random() < 0.6:
        prog["first"] = footer()
        if rnd.random() < 0.6:
            prog["later"] = footer()
    return prog


PDF_STEPS += [("POST", "/p/ops", pdf_program()) for _ in range(300)]
if HTML5_PARSER:
    STEPS += PDF_STEPS

# tempfile.TemporaryDirectory (fixtures/docapp/tmp.py)
STEPS += [("POST", f"/t/tmpdir?name={n}&size={s}&nested={x}", None) for n, s, x in (("dossier.pdf", 100, "false"),
          ("d%C3%A9j%C3%A0.pdf", 70000, "true"), ("x", 0, "false"))]
STEPS += [("POST", f"/t/tmpdir/options?prefix={p}&suffix={x}&explicit={e}", None)
          for p, x, e in (("pack-", "-x", "true"), ("", "", "false"), ("tmp", ".d", "true"))]


def reset(db: str) -> None:
    pass


_ADDR = re.compile(r"(<\w+ at |<\w+@)0x[0-9A-Fa-f]+")


def normalize(body):
    """reportlab's error messages name a flowable by CPython's id() (`<Spacer at 0x10aecd940 frame=normal>`,
    `<Table@0x0000000108A3C2D0 ...>`): the address is masked, the rest compared."""
    if isinstance(body, str):
        return _ADDR.sub(r"\g<1>0x?", body)
    if isinstance(body, dict):
        return {k: normalize(v) for k, v in body.items()}
    if isinstance(body, list):
        return [normalize(v) for v in body]
    return body
