"""openpyxl exports (dynrt/xlsx.rs): what a report endpoint does (Workbook, styles, merged title, widths from the
longest value, a statistics sheet, BytesIO into a StreamingResponse), and `/x/ops`, a list of operations played
on a fresh workbook (tests/scenarios/docapp.py generates them)."""
import io
import json
from decimal import Decimal
from typing import Any

from fastapi import APIRouter, Body
from fastapi.responses import Response, StreamingResponse
from openpyxl import Workbook
from openpyxl.styles import Alignment, Border, Font, PatternFill, Side
from openpyxl.utils import get_column_letter

router = APIRouter()
XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"

ROWS = [
    {"address": "12 rue des Lilas", "type": "apartment", "surface": 54.5, "rooms": 3, "rent": 950, "status": "rented"},
    {"address": "=HYPERLINK(\"x\")", "type": "house", "surface": 120, "rooms": 5, "rent": Decimal("1830.50"), "status": "vacant"},
    {"address": "  Résidence  Été ", "type": "studio", "surface": 18.25, "rooms": 1, "rent": 0, "status": None},
    {"address": "Bât. A & <B>", "type": "parking", "surface": 0.1, "rooms": 0, "rent": -12.75, "status": "#N/A"},
]


def neutralize(value: Any) -> Any:
    if isinstance(value, str) and value.startswith(("=", "+", "-", "@", "\t", "\r")):
        return "'" + value
    return value


def neutralize_book(wb: Workbook) -> None:
    # iter_rows() without bounds yields nothing until append() was used: openpyxl's own rule
    for ws in wb.worksheets:
        for row in ws.iter_rows():
            for cell in row:
                if isinstance(cell.value, str):
                    cell.value = neutralize(cell.value)


def to_excel(data: list[dict], fieldnames: list[str], sheet_name: str = "Data", title: str | None = None) -> io.BytesIO:
    wb = Workbook()
    ws = wb.active
    ws.title = sheet_name
    header_font = Font(bold=True, color="FFFFFF", size=12)
    header_fill = PatternFill(start_color="3498db", end_color="3498db", fill_type="solid")
    header_alignment = Alignment(horizontal="center", vertical="center")
    title_font = Font(bold=True, size=16, color="2c3e50")
    border = Border(left=Side(style="thin"), right=Side(style="thin"), top=Side(style="thin"), bottom=Side(style="thin"))
    current_row = 1
    if title:
        ws.merge_cells(start_row=current_row, start_column=1, end_row=current_row, end_column=len(fieldnames))
        title_cell = ws.cell(row=current_row, column=1, value=title)
        title_cell.font = title_font
        title_cell.alignment = Alignment(horizontal="center", vertical="center")
        current_row += 2
    ws.cell(row=current_row, column=1, value="Generated: fixed")
    ws.cell(row=current_row, column=2, value=f"Total Records: {len(data)}")
    current_row += 2
    for col_num, fieldname in enumerate(fieldnames, start=1):
        cell = ws.cell(row=current_row, column=col_num, value=fieldname.replace("_", " ").title())
        cell.font = header_font
        cell.fill = header_fill
        cell.alignment = header_alignment
        cell.border = border
    current_row += 1
    for row_data in data:
        for col_num, fieldname in enumerate(fieldnames, start=1):
            cell = ws.cell(row=current_row, column=col_num, value=row_data.get(fieldname, ""))
            cell.border = border
            cell.alignment = Alignment(vertical="center")
        current_row += 1
    for col_num in range(1, len(fieldnames) + 1):
        column_letter = get_column_letter(col_num)
        max_length = 0
        for cell in ws[column_letter]:
            if cell.value:
                max_length = max(max_length, len(str(cell.value)))
        ws.column_dimensions[column_letter].width = min(max_length + 2, 50)
    buffer = io.BytesIO()
    neutralize_book(wb)
    wb.save(buffer)
    buffer.seek(0)
    return buffer


@router.get("/x/report")
async def report(n: int = 4, title: str | None = "Property Report", fields: str = "address,type,surface,rooms,rent,status"):
    names = fields.split(",")
    buf = to_excel((ROWS * 3)[:n], names, "Properties", title)
    return StreamingResponse(buf, media_type=XLSX, headers={"Content-Disposition": "attachment; filename=report.xlsx"})


@router.get("/x/stats")
async def stats(n: int = 4):
    props = (ROWS * 3)[:n]
    wb = Workbook()
    ws = wb.active
    ws.title = "Properties"
    header_font = Font(bold=True, color="FFFFFF", size=11)
    header_fill = PatternFill(start_color="27ae60", end_color="27ae60", fill_type="solid")
    title_font = Font(bold=True, size=14)
    ws.merge_cells("A1:F1")
    title_cell = ws["A1"]
    title_cell.value = "Property Report"
    title_cell.font = title_font
    title_cell.alignment = Alignment(horizontal="center")
    ws["A3"] = "Generated: fixed"
    ws["A4"] = f"Total Properties: {len(props)}"
    for col_num, header in enumerate(["Address", "Type", "Surface (m²)", "Rooms", "Rent (€)", "Status"], start=1):
        cell = ws.cell(row=6, column=col_num, value=header)
        cell.font = header_font
        cell.fill = header_fill
        cell.alignment = Alignment(horizontal="center")
    for row_num, p in enumerate(props, start=7):
        for col, key in enumerate(("address", "type", "surface", "rooms", "rent", "status"), start=1):
            ws.cell(row=row_num, column=col, value=p.get(key, ""))
    for col in range(1, 7):
        ws.column_dimensions[get_column_letter(col)].width = 20
    st = wb.create_sheet("Statistics")
    st["A1"] = "Property Statistics"
    st["A1"].font = title_font
    if props:
        total = sum(float(p.get("rent", 0)) for p in props)
        table = [["Metric", "Value"], ["Total Properties", len(props)], ["Total Monthly Rent", f"{total:.2f} €"],
                 ["Average Rent", f"{total / len(props):.2f} €"]]
        for row_num, (label, value) in enumerate(table, start=3):
            st.cell(row=row_num, column=1, value=label)
            st.cell(row=row_num, column=2, value=value)
            if row_num == 3:
                st.cell(row=row_num, column=1).font = header_font
                st.cell(row=row_num, column=2).font = header_font
        st.column_dimensions["A"].width = 25
        st.column_dimensions["B"].width = 20
    buffer = io.BytesIO()
    neutralize_book(wb)
    wb.save(buffer)
    return Response(content=buffer.getvalue(), media_type=XLSX, headers={"Content-Disposition": 'attachment; filename="stats.xlsx"'})


def side_of(spec: dict | None):
    return None if spec is None else Side(style=spec.get("style"), color=spec.get("color"))


def style_of(spec: dict):
    g = spec.get
    kind = g("kind")
    if kind == "font":
        return Font(name=g("name"), size=g("size"), bold=g("bold"), italic=g("italic"), strike=g("strike"), color=g("color"))
    if kind == "fill":
        return PatternFill(fill_type=g("fill_type"), start_color=g("start_color"), end_color=g("end_color"))
    if kind == "align":
        return Alignment(horizontal=g("horizontal"), vertical=g("vertical"), wrap_text=g("wrap_text"), shrink_to_fit=g("shrink_to_fit"))
    return Border(left=side_of(g("left")), right=side_of(g("right")), top=side_of(g("top")), bottom=side_of(g("bottom")),
                  diagonal=side_of(g("diagonal")))


def apply_style(cell, attr: str, style) -> None:
    if attr == "font":
        cell.font = style
    elif attr == "fill":
        cell.fill = style
    elif attr == "alignment":
        cell.alignment = style
    else:
        cell.border = style


@router.post("/x/ops")
async def ops(program: list = Body(...)):
    """Each operation is a list: its name, then its arguments; an exception stops the program and is answered."""
    wb = Workbook()
    ws = wb.active
    seen = []
    try:
        for op in program:
            name, args = op[0], op[1:]
            if name == "cell":
                c = ws.cell(row=args[0], column=args[1], value=args[2] if len(args) > 2 else None)
                seen.append(c.coordinate)
            elif name == "set":
                ws[args[0]] = args[1]
            elif name == "dec":
                ws[args[0]] = Decimal(args[1])
            elif name == "style":
                apply_style(ws[args[0]], args[1], style_of(args[2]))
            elif name == "merge":
                ws.merge_cells(args[0])
            elif name == "mergekw":
                ws.merge_cells(start_row=args[0], start_column=args[1], end_row=args[2], end_column=args[3])
            elif name == "width":
                ws.column_dimensions[args[0]].width = args[1]
            elif name == "dim":
                seen.append(ws.column_dimensions[args[0]].width)
            elif name == "sheet":
                ws = wb.create_sheet(args[0]) if args else wb.create_sheet()
                seen.append(ws.title)
            elif name == "title":
                ws.title = args[0]
                seen.append(ws.title)
            elif name == "append":
                ws.append(args[0])
            elif name == "col":
                seen.append([c.value for c in ws[args[0]]])
            elif name == "rows":
                seen.append([[c.coordinate for c in r] for r in ws.iter_rows(min_row=args[0], max_row=args[1])])
            elif name == "allrows":
                seen.append(len(list(ws.iter_rows())))
            elif name == "read":
                c = ws[args[0]]
                seen.append([c.value, c.data_type, c.row, c.column, c.column_letter])
            elif name == "extent":
                seen.append([ws.min_row, ws.max_row, ws.min_column, ws.max_column])
            elif name == "letter":
                seen.append(get_column_letter(args[0]))
            elif name == "names":
                seen.append(wb.sheetnames)
    except Exception as e:  # noqa: BLE001 - the message is what is compared
        return {"error": type(e).__name__, "message": str(e), "seen": seen}
    buf = io.BytesIO()
    wb.save(buf)
    return Response(content=buf.getvalue(), media_type=XLSX, headers={"X-Seen": json.dumps(seen)})
