"""Generated documents: openpyxl workbooks (styles, merged title, column widths, several sheets) and reportlab PDFs
(paragraphs, styles, rules, vector logo, page callbacks) returned as attachments, the way real applications'
exports build them, and small interpreters of workbook and document operations the scenario fuzzes. No database."""
from fastapi import FastAPI

from .pdfs import router as pdf_router
from .sheets import router
from .tmp import router as tmp_router

app = FastAPI()
app.include_router(router)
app.include_router(tmp_router)
app.include_router(pdf_router)


@app.get("/health")
async def health():
    return {"ok": True}
