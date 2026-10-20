# Generated documents

Routes that build a spreadsheet or a PDF and return it (an export, a report, a letter) run in the binary when
they stay within the surface below. The file is the one the Python application writes, byte for byte, except
for the instants it records: a workbook's zip entries carry the time they were written, and `docProps/core.xml`
its creation and modification times; a PDF its `/CreationDate`, `/ModDate` and the `/ID` derived from them.

## Spreadsheets (openpyxl)

openpyxl 3.1 with et_xmlfile 2.0, **without lxml**: when lxml is installed, openpyxl writes its XML through
it and the bytes differ: when the project locks lxml, a route that builds a `Workbook()` is refused (file:line),
or left to Python with `--python-side auto`; the other routes are translated.

Supported:

- `Workbook()`; `wb.active`, `wb.worksheets`, `wb.sheetnames`, `wb[title]`, `wb.create_sheet(title)`,
  `wb.save(buffer)` into an `io.BytesIO`.
- `ws.title` (read and set: invalid characters, duplicates numbered like openpyxl), `ws.cell(row, column, value)`,
  `ws["A1"]`, `ws["A"]`, `ws["3"]`, `ws["A1:C3"]`, `ws["A1"] = value`, `ws.merge_cells("A1:F1")` or by bounds,
  `ws.column_dimensions["A"].width`, `ws.iter_rows(...)`, `ws.iter_cols(...)`, `ws.append(row)`,
  `ws.min_row`/`max_row`/`min_column`/`max_column`.
- Cell values: `str` (formulas, error codes, 32,767 characters at most, `IllegalCharacterError`), `int`,
  `float`, `Decimal`, `bool`, `None`; `cell.value`, `row`, `column`, `coordinate`, `column_letter`, `data_type`.
- Styles assigned to a cell: `Font(name, size, bold, italic, strike, color)`,
  `PatternFill(fill_type, start_color, end_color)`, `Border(left, right, top, bottom, diagonal)` of
  `Side(style, color)`, `Alignment(horizontal, vertical, wrap_text, shrink_to_fit)`; shared between cells like
  openpyxl's indexed lists.
- `openpyxl.utils.get_column_letter`.

Refused when the project is translated (file:line): other constructor arguments, positional arguments.
Raising a `py2axum:` TypeError at run time (a 500 where Python would answer):

- date and time values (they need number formats);
- a second merged range in one worksheet (openpyxl keeps them in a set, whose order is CPython's);
- a sheet title over 31 characters (openpyxl only warns);
- reading a cell's style back (`cell.font.bold`), saving to a file name, `create_sheet(index=)`.

Differences:

- `iter_rows()` and `iter_cols()` build every row when they are called, not one at a time: leaving the loop
  early still creates the cells the remaining rows would have (empty, they add `<row>` elements to the sheet).

## PDF (reportlab)

reportlab 4.4.5 exactly (4.4.6 changed the header of every PDF), on its pure-Python path: when `rl_accel` (its C
accelerators) or `rl_renderPM` is in the lock, the calls that build documents are refused (file:line; with
`--python-side auto` their routes stay in Python).

Supported:

- `SimpleDocTemplate(buffer, pagesize, leftMargin, rightMargin, topMargin, bottomMargin, title, author, subject,
  creator, producer, keywords, invariant, pageCompression, allowSplitting, rotation, initialFontName,
  initialFontSize, initialLeading)` writing into an `io.BytesIO`; `doc.build(story, onFirstPage=, onLaterPages=)`
  (the list is emptied, like reportlab's); `doc.width`, `height`, `pagesize`, the margins, `title`..., `doc.page`.
- The page callbacks receive the document and a canvas: `saveState`, `restoreState`, `setFont`, `setFontSize`,
  `setFillColor`/`setStrokeColor` (a colour, a tuple, a name or `#rrggbb`), `setFillColorRGB`,
  `setStrokeColorRGB`, `setFillGray`, `setStrokeGray`, `setLineWidth`, `line`, `rect`, `circle`, `roundRect`,
  `drawString`, `drawRightString`, `drawCentredString`, `translate`, `scale`, `rotate`, `stringWidth`,
  `getPageNumber()`, `_pagesize`. An exception they raise is annotated like reportlab's
  (`\nhandle_pageBegin args=() ...`, the original as its context; an AttributeError unchanged).
- `Paragraph(text, style)` with the markup `<b>`, `<strong>`, `<i>`, `<em>`, `<font name size color face>`,
  `<span>`, `<br/>`, `<para>`, `<nobr>`, entities and character references; unknown tags are ignored as in
  reportlab. A paragraph keeps its style object: a style changed after the paragraph was made is the one laid out.
- `ParagraphStyle(name, parent=, **attributes)` (attributes read and set: `fontName`, `fontSize`, `leading`,
  indents, `alignment`, `spaceBefore`/`spaceAfter`, `textColor`, `backColor`, borders, `allowWidows`/`allowOrphans`,
  `splitLongWords`, `keepWithNext`...), `getSampleStyleSheet()` (`styles["Normal"]`, aliases such as `"h1"`, `in`,
  `get`, `add`, the `KeyError` of an unknown name).
- `Spacer`, `PageBreak()`, `HRFlowable(width, thickness, lineCap, color, spaceBefore, spaceAfter, hAlign, vAlign,
  dash)`.
- `Table(data, colWidths, rowHeights, style, repeatRows, hAlign, vAlign, spaceBefore, spaceAfter)` with cells of text,
  numbers, `None`, flowables (paragraphs, drawings, nested tables) or lists of them; `TableStyle([...])`,
  `table.setStyle(...)` (commands accumulate), `TableStyle.add(...)`; the commands are tuples
  `(op, (col, row), (col, row), *args)`, split across pages by rows with the repeated header rows.
- `reportlab.graphics.shapes`: `Drawing(width, height)` as a flowable (`hAlign`, `vAlign`, `renderScale`), `Group`
  (`add`, `scale`, `translate`, `rotate`, `skew`, `shift`), `Rect`, `Circle`, `Ellipse`, `Line`, `Polygon`,
  `PolyLine` with their fill and stroke attributes.
- `reportlab.lib`: `colors.<name>`, `colors.HexColor(...)`, `colors.Color(r, g, b, alpha)` (`.red`, `.green`,
  `.blue`, `.alpha`); `units.inch`/`cm`/`mm`; `pagesizes.A3`/`A4`/`A5`/`letter`/`legal`, `landscape()`,
  `portrait()`; `enums.TA_LEFT`/`TA_CENTER`/`TA_RIGHT`/`TA_JUSTIFY`; `LayoutError`.
- The 14 standard fonts.

Refused when the project is translated (file:line): other classes (`KeepTogether`, `Image`, the `pdfgen` canvas
used directly, `CMYKColor`...) and other constructor arguments (`Paragraph(bulletText=)`, `PageBreak(nextTemplate)`,
`TableStyle(parent=)`...). Raising a `py2axum:` TypeError at run time (a 500 where Python would answer): markup
or style features outside the port (`<u>`, `<strike>`, `<a>`, `<img>`, `<super>`, bullets, `wordWrap`,
hyphenation...), a document written to a file name, a canvas kept and used after its callback, a list style of the
sample sheet (`UnorderedList`).

The paragraph markup goes through CPython's `html.parser`, which changed in patch releases: the binary follows
the project's Python (`--python`, `.python-version`, the transpiler's own patch, or `PY2AXUM_PYTHON_VERSION` for the
binary), and the transpiler probes its own `html.parser` for the backports of distribution builds:

| Behaviour | 3.12 | 3.13 | 3.14 | 3.15 |
|---|---|---|---|---|
| HTML5 tag and comment parsing (older parsers: `Paragraph` refused at run time) | 3.12.12 | 3.13.8 | all | all |
| `xmp`, `iframe`, `noembed`, `noframes` read as raw text, `plaintext` to the end | 3.12.13 | 3.13.10 | 3.14.1 | all |
| `&#` without a number is text (before: the paragraph ends there, `unclosed tags` unless a `;` follows) | — | 3.13.10 | 3.14.1 | all |
| `<!-->` and `<!--->` close the comment before a later `-->` | — | 3.13.15 | 3.14.7 | all |

Differences:

- The `LayoutError` of a flowable too large for the page names it by CPython's `id()`
  (`<Spacer at 0x10aecd940 frame=normal>`): the binary writes another address. A table there is named
  `<Table@0x00000000 N rows x M cols>...`, without reportlab's tallest row and first cell.

## Temporary files (tempfile)

`tempfile.TemporaryDirectory(suffix, prefix, dir, ignore_cleanup_errors, delete=)`: a directory made like `mkdtemp`
(mode 0o700, `prefix` + 8 random characters + `suffix`); `with ... as path` gives its name, and the block's end
removes it with its content (`delete=True`), as do `cleanup()` (again: nothing) and the object going away;
`.name`, `repr()`. Its removal retries an entry that is not writable as CPython does.

`tempfile.NamedTemporaryFile(mode, suffix, prefix, dir, delete)` in `w`/`w+` modes (binary or text) and
`tempfile.gettempdir()`: a new file created exclusively with mode 0o600, named `prefix` + 8 random characters +
`suffix` in the temporary directory CPython picks (`$TMPDIR`, `$TEMP`, `$TMP`, `/tmp`, ...), removed when it is
closed if `delete` is true. Other arguments are refused when the project is translated.

Difference: what is written reaches the file at once, as with `open()` in the binary. CPython keeps small writes
in its buffer until `flush()` or `close()`: reading the file by its name before either sees less in Python.
