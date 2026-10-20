"""reportlab documents (dynrt/rl/, dynrt/rlbind.rs): what a rental-management back office generates — notice
letters, pre-written letters on a mutated sample style, a branded document (vector logo, footer drawn by the page
callbacks, several pages), a lease-like contract with rules and page breaks, a report export — and `/p/ops`, a
program of document operations the scenario draws at random (tests/scenarios/docapp.py). Tables: the branded header
(logo, nested identity table, metadata), label/value tables, long tables split over pages with a repeated header."""
import io

from fastapi import APIRouter, Body
from fastapi.responses import Response, StreamingResponse
from reportlab.graphics.shapes import Circle, Drawing, Group, Polygon, Rect
from reportlab.lib import colors
from reportlab.lib.enums import TA_CENTER, TA_JUSTIFY, TA_LEFT, TA_RIGHT
from reportlab.lib.pagesizes import A4, landscape, letter
from reportlab.lib.styles import ParagraphStyle, getSampleStyleSheet
from reportlab.lib.units import cm, inch, mm
from reportlab.platypus import HRFlowable, PageBreak, Paragraph, SimpleDocTemplate, Spacer, Table, TableStyle
from reportlab.platypus.doctemplate import LayoutError

router = APIRouter()
PDF = "application/pdf"

BRAND = "Gestimmo"
SITE = "gestimmo.example"
PRIMARY = colors.HexColor("#4F46E5")
ACCENT = colors.HexColor("#FCD34D")
INK = colors.HexColor("#111827")
MUTED = colors.HexColor("#6B7280")
LINE = colors.HexColor("#E5E7EB")


def pdf(buffer: io.BytesIO, name: str) -> Response:
    return Response(content=buffer.getvalue(), media_type=PDF, headers={"Content-Disposition": f'attachment; filename="{name}"'})


# ---------------------------------------------------------------- notice letter


@router.get("/p/notice")
async def notice(name: str = "Jean Dupont", address: str = "12 rue des Lilas, 75011 Paris", landlord: str = "SCI Les Tilleuls",
                 months: int = 3, furnished: bool = False, reason: str = "", details: str = "", date: str = "18/10/2026"):
    buffer = io.BytesIO()
    doc = SimpleDocTemplate(buffer, pagesize=A4, rightMargin=2 * cm, leftMargin=2 * cm, topMargin=2 * cm, bottomMargin=2 * cm)
    elements = []
    styles = getSampleStyleSheet()
    heading_style = ParagraphStyle("NoticeHeading", parent=styles["Heading2"], fontSize=12, textColor=colors.HexColor("#34495e"),
                                   spaceAfter=10, spaceBefore=15, fontName="Helvetica-Bold")
    normal_style = ParagraphStyle("NoticeNormal", parent=styles["Normal"], fontSize=10, leading=14, alignment=TA_JUSTIFY)
    elements.append(Paragraph(f"""
    {name}<br/>
    {address}<br/>
    Email: {name.lower().replace(" ", ".")}@example.org<br/>
    Téléphone: 06 12 34 56 78
    """, normal_style))
    elements.append(Spacer(1, 30))
    elements.append(Paragraph(f"""
    <b>À l'attention de :</b><br/>
    {landlord}<br/>
    {address}
    """, normal_style))
    elements.append(Spacer(1, 30))
    elements.append(Paragraph(f"<i>Fait le {date}</i>", ParagraphStyle("DateStyle", parent=normal_style, alignment=TA_RIGHT)))
    elements.append(Spacer(1, 20))
    elements.append(Paragraph("<b>Lettre recommandée avec accusé de réception</b>",
                              ParagraphStyle("Recommande", parent=normal_style, alignment=TA_CENTER, fontName="Helvetica-Bold")))
    elements.append(Spacer(1, 30))
    elements.append(Paragraph(f"<b>Objet : Congé de location - Préavis de {months} mois</b>", heading_style))
    elements.append(Spacer(1, 15))
    elements.append(Paragraph("Madame, Monsieur,", normal_style))
    elements.append(Spacer(1, 15))
    kind = "meublé" if furnished else "non meublé"
    elements.append(Paragraph(f"""
    Par la présente, je vous informe de ma décision de donner congé du logement que j'occupe à l'adresse suivante :<br/>
    <br/>
    <b>{address}</b><br/>
    <br/>
    Type de logement : {kind}<br/>
    <br/>
    Conformément à la loi n° 89-462 du 6 juillet 1989, je respecte un préavis de <b>{months} mois</b>.
    """, normal_style))
    elements.append(Spacer(1, 15))
    if reason and months == 1 and not furnished:
        labels = {"job_transfer": "mutation professionnelle", "job_loss": "perte d'emploi",
                  "health_reasons": "état de santé justifiant un changement de domicile"}
        text = f"""
        <b>Motif du préavis réduit (1 mois) :</b><br/>
        {labels.get(reason, reason).capitalize()}<br/>
        """
        if details:
            text += f"""<br/>
        <b>Précisions :</b> {details}<br/>
        """
        text += """<br/>
        Conformément à l'article 15-I de la loi n° 89-462, le préavis est réduit à un mois dans ce cas de figure.
        """
        elements.append(Paragraph(text, normal_style))
        elements.append(Spacer(1, 15))
    elements.append(Paragraph("""
    Je vous rappelle que le dépôt de garantie devra m'être restitué dans les délais légaux :<br/>
    - 1 mois maximum si aucune somme n'est retenue<br/>
    - 2 mois maximum si des retenues sont effectuées (avec justificatifs)
    """, normal_style))
    elements.append(Spacer(1, 30))
    elements.append(Paragraph(f"<b>{name}</b><br/><br/>Signature :", normal_style))
    elements.append(Spacer(1, 20))
    elements.append(Paragraph("""
    <i><font size=8>
    Note : le congé est notifié par lettre recommandée avec demande d'avis de réception, par acte de
    commissaire de justice ou par remise en main propre contre récépissé ou émargement (art. 15-I et 25-8
    de la loi n° 89-462). Le préavis court à compter de sa réception par le bailleur.
    </font></i>
    """, ParagraphStyle("Footer", parent=normal_style, fontSize=8, textColor=colors.grey)))
    doc.build(elements)
    buffer.seek(0)
    return StreamingResponse(buffer, media_type=PDF, headers={"Content-Disposition": 'attachment; filename="notice.pdf"'})


# ---------------------------------------------------------------- pre-written letters (a mutated sample style)


def build_letter(content_builder, title: str) -> io.BytesIO:
    buffer = io.BytesIO()
    doc = SimpleDocTemplate(buffer, pagesize=A4, rightMargin=2.5 * cm, leftMargin=2.5 * cm, topMargin=2.5 * cm,
                            bottomMargin=2.5 * cm, title=title)
    styles = getSampleStyleSheet()
    normal = styles["Normal"]
    normal.fontName = "Helvetica"
    normal.fontSize = 10
    normal.leading = 14
    heading = ParagraphStyle("Heading", parent=normal, fontSize=13, fontName="Helvetica-Bold", alignment=TA_CENTER, spaceAfter=6)
    right = ParagraphStyle("Right", parent=normal, alignment=TA_RIGHT)
    justify = ParagraphStyle("Justify", parent=normal, alignment=TA_JUSTIFY)
    small = ParagraphStyle("Small", parent=normal, fontSize=9)
    story = content_builder(normal, heading, right, justify, small)
    doc.build(story)
    buffer.seek(0)
    return buffer


@router.get("/p/letter/{kind}")
async def letter_route(kind: str, landlord: str = "Le propriétaire", tenant: str = "Le locataire", address: str = "",
                       rent: float = 950.0, charges: float = 80.5, period: str = "Octobre 2026", date: str = "18/10/2026"):
    def builder(normal, heading, right, justify, small):
        story = [Paragraph(landlord, normal), Spacer(1, 4 * mm), Paragraph(f"À {tenant}", normal), Paragraph(address, normal),
                 Spacer(1, 6 * mm), Paragraph(f"Fait le {date}", right), Spacer(1, 8 * mm),
                 HRFlowable(width="100%", thickness=1, color=colors.black), Spacer(1, 4 * mm)]
        if kind == "echeance":
            story.append(Paragraph("AVIS D'ÉCHÉANCE", heading))
        else:
            story.append(Paragraph("DEMANDE D'ATTESTATION D'ASSURANCE HABITATION", heading))
        story += [Spacer(1, 4 * mm), HRFlowable(width="100%", thickness=1, color=colors.black), Spacer(1, 8 * mm)]
        if kind == "echeance":
            story.append(Paragraph(f"Période : <b>{period}</b>", normal))
            story.append(Spacer(1, 6 * mm))
            story.append(Paragraph(f"Loyer : <b>{rent:.2f} €</b>", normal))
            story.append(Paragraph(f"Charges : <b>{charges:.2f} €</b>", normal))
            story.append(Paragraph(f"<b>Total à régler : {rent + charges:.2f} €</b>", normal))
            story.append(Spacer(1, 10 * mm))
            story.append(Paragraph("Merci de bien vouloir régler cette somme à la date indiquée ci-dessus.", justify))
        else:
            story.append(Paragraph(f"Madame, Monsieur {tenant},", normal))
            story.append(Spacer(1, 4 * mm))
            story.append(Paragraph("Conformément à l'article 7 g) de la loi du 6 juillet 1989, je vous remercie de me faire "
                                   "parvenir l'attestation d'assurance habitation couvrant les risques locatifs pour "
                                   f"le logement situé {address or 'à l’adresse du bail'}, dans un délai d'un mois.", justify))
            story.append(Spacer(1, 6 * mm))
            story.append(Paragraph("<i>Sans réponse de votre part, le bailleur peut souscrire une assurance pour le compte du "
                                   "locataire et en récupérer le montant.</i>", small))
        return story

    return pdf(build_letter(builder, "Avis d'échéance" if kind == "echeance" else "Demande d'attestation"), f"{kind}.pdf")


# ---------------------------------------------------------------- branded document: logo, footer callbacks, pages


def brand_logo(size: float = 1.5 * cm) -> Drawing:
    s = size / 200.0
    d = Drawing(size, size)
    g = Group()
    g.add(Circle(100, 100, 95, fillColor=PRIMARY, strokeColor=None))
    g.add(Polygon([65, 115, 100, 145, 135, 115], fillColor=colors.white, strokeColor=None))
    g.add(Rect(65, 60, 70, 55, fillColor=colors.white, strokeColor=None))
    g.add(Rect(88, 60, 24, 30, fillColor=PRIMARY, strokeColor=None))
    win = colors.HexColor("#E0E7FF")
    g.add(Rect(72, 98, 12, 12, fillColor=win, strokeColor=None))
    g.add(Rect(116, 98, 12, 12, fillColor=win, strokeColor=None))
    g.add(Circle(126, 134, 4.5, fillColor=ACCENT, strokeColor=None))
    g.add(Rect(126, 132.8, 13, 2.4, fillColor=ACCENT, strokeColor=None))
    g.scale(s, s)
    d.add(g)
    return d


def draw_page_footer(canvas, doc) -> None:
    canvas.saveState()
    width, _height = canvas._pagesize
    y = 1.25 * cm
    canvas.setStrokeColor(LINE)
    canvas.setLineWidth(0.5)
    canvas.line(2 * cm, y + 10, width - 2 * cm, y + 10)
    canvas.setFont("Helvetica", 7.5)
    canvas.setFillColor(MUTED)
    canvas.drawString(2 * cm, y, f"Généré avec {BRAND} — {SITE}")
    canvas.drawRightString(width - 2 * cm, y, f"Page {canvas.getPageNumber()}")
    canvas.restoreState()


def first_page(canvas, doc) -> None:
    draw_page_footer(canvas, doc)
    canvas.saveState()
    canvas.setFillColor(PRIMARY)
    canvas.rect(0, doc.pagesize[1] - 0.4 * cm, doc.pagesize[0], 0.4 * cm, stroke=0, fill=1)
    canvas.setFont("Helvetica-Bold", 9)
    canvas.drawCentredString(doc.pagesize[0] / 2, doc.pagesize[1] - 1.2 * cm, doc.title or "")
    canvas.restoreState()


PHRASES = [
    "Le locataire s'engage à user paisiblement des locaux loués suivant la destination prévue au contrat.",
    "Les charges récupérables sont exigibles en contrepartie des services rendus liés à l'usage des éléments de la chose louée, "
    "des dépenses d'entretien courant et des menues réparations sur les éléments d'usage commun.",
    "Tout retard de paiement entraînera l'application des intérêts au taux légal & des frais de relance (art. 1231-6 du Code civil).",
    "Le dépôt de garantie, d'un montant de <b>1 900,00 €</b>, sera restitué dans un délai maximal de <i>deux mois</i>.",
    "La révision annuelle du loyer s'effectue selon la variation de l'IRL publié par l'INSEE ; l'indice de référence est celui du "
    "2e trimestre (« IRL » &lt; 3,5 %).",
]


@router.get("/p/branded")
async def branded(n: int = 3, meta: str = "Quittance n° 2026-10<br/>Émise le 18/10/2026", title: str = "Quittance de loyer",
                  author: str = BRAND, later: bool = True, pagesize: str = "A4", logo: float = 1.35):
    buffer = io.BytesIO()
    size = A4 if pagesize == "A4" else letter if pagesize == "letter" else landscape(A4)
    doc = SimpleDocTemplate(buffer, pagesize=size, leftMargin=2 * cm, rightMargin=2 * cm, topMargin=2 * cm, bottomMargin=2.2 * cm,
                            title=title, author=author, subject="Document de gestion", creator=BRAND)
    styles = getSampleStyleSheet()
    word = ParagraphStyle("BrandWordmark", fontName="Helvetica-Bold", fontSize=15, leading=17, textColor=INK, alignment=TA_LEFT)
    tag = ParagraphStyle("BrandTagline", fontName="Helvetica", fontSize=8, leading=10, textColor=MUTED, alignment=TA_LEFT)
    meta_style = ParagraphStyle("BrandMeta", fontName="Helvetica", fontSize=8.5, leading=12, textColor=MUTED, alignment=TA_RIGHT)
    body = ParagraphStyle("Body", parent=styles["BodyText"], alignment=TA_JUSTIFY, leading=13)
    head = ParagraphStyle("Head", parent=styles["Heading2"], textColor=PRIMARY)
    logo_d = brand_logo(logo * cm)
    logo_d.hAlign = "CENTER"
    elements = [logo_d, Paragraph(BRAND, word), Paragraph("Gestion locative simplifiée", tag)]
    if meta:
        elements.append(Paragraph(meta, meta_style))
    elements.append(HRFlowable(width="100%", thickness=1.2, color=PRIMARY, spaceBefore=4, spaceAfter=10))
    for i in range(n):
        elements.append(Paragraph(f"{i + 1}. Article {i + 1}", head))
        for j in range(3):
            elements.append(Paragraph(PHRASES[(i + j) % len(PHRASES)] + f" (§ {i + 1}.{j + 1})", body))
        if i % 4 == 3:
            elements.append(PageBreak())
        else:
            elements.append(Spacer(1, 6))
    elements.append(HRFlowable(width=8 * cm, thickness=0.5, color=LINE, hAlign="RIGHT", dash=[2, 1]))
    elements.append(Paragraph("Fin du document", ParagraphStyle("End", parent=styles["Italic"], alignment=TA_CENTER)))
    if later:
        doc.build(elements, onFirstPage=first_page, onLaterPages=draw_page_footer)
    else:
        doc.build(elements, onFirstPage=draw_page_footer)
    return pdf(buffer, "branded.pdf")


# ---------------------------------------------------------------- lease-like contract


@router.get("/p/lease")
async def lease(landlord: str = "M. Martin", tenant: str = "Mme Durand", furnished: bool = False, clauses: int = 6,
                extra: str = "", date: str = "18/10/2026"):
    buffer = io.BytesIO()
    W = 17 * cm
    doc = SimpleDocTemplate(buffer, pagesize=A4, rightMargin=2 * cm, leftMargin=2 * cm, topMargin=2 * cm, bottomMargin=2 * cm)
    styles = getSampleStyleSheet()
    DARK = colors.HexColor("#1a2533")
    BLUE = colors.HexColor("#2563eb")
    BORDER = colors.HexColor("#cbd5e1")
    title_style = ParagraphStyle("LeaseTitle", parent=styles["Normal"], fontSize=15, fontName="Helvetica-Bold", textColor=DARK,
                                 alignment=TA_CENTER, spaceAfter=4)
    subtitle_style = ParagraphStyle("LeaseSubtitle", parent=styles["Normal"], fontSize=8, textColor=colors.HexColor("#64748b"),
                                    alignment=TA_CENTER, spaceAfter=2)
    sub_style = ParagraphStyle("SubHead", parent=styles["Normal"], fontSize=9, fontName="Helvetica-Bold", textColor=DARK,
                               spaceBefore=8, spaceAfter=4)
    normal = ParagraphStyle("LeaseNormal", parent=styles["Normal"], fontSize=9, leading=13, textColor=DARK, alignment=TA_JUSTIFY)
    small = ParagraphStyle("LeaseSmall", parent=styles["Normal"], fontSize=8, leading=11, textColor=colors.HexColor("#475569"))
    right_style = ParagraphStyle("LeaseRight", parent=styles["Normal"], fontSize=8, textColor=colors.HexColor("#64748b"),
                                 alignment=TA_RIGHT)
    label = "MEUBLÉ" if furnished else "NU"
    elements = [Spacer(1, 10), Paragraph(f"CONTRAT DE LOCATION DE LOGEMENT {label}", title_style),
                Paragraph("Loi n° 89-462 du 6 juillet 1989 — Décret n° 2015-587 du 29 mai 2015", subtitle_style),
                HRFlowable(width=W, thickness=1.5, color=BLUE, spaceAfter=6), Paragraph(f"Établi le {date}", right_style),
                Spacer(1, 12)]
    elements.append(Paragraph("I. DÉSIGNATION DES PARTIES", sub_style))
    elements.append(Paragraph(f"<b>Le bailleur :</b> {landlord}<br/><b>Le locataire :</b> {tenant}", normal))
    for k in range(clauses):
        elements.append(Paragraph(f"Article {k + 1} — <font color='#2563eb'>Clause</font> n° {k + 1}", sub_style))
        elements.append(Paragraph((PHRASES[k % len(PHRASES)] + " ") * (1 + k % 3), normal))
        if extra:
            elements.append(Paragraph(extra, small))
    elements.append(PageBreak())
    elements.append(Paragraph("SIGNATURES", sub_style))
    elements.append(HRFlowable(width=W, thickness=0.5, color=BORDER))
    elements.append(Paragraph(f"Fait en deux exemplaires, le {date}.<br/>Le bailleur &amp; le locataire", normal))
    doc.build(elements)
    return pdf(buffer, "lease.pdf")


# ---------------------------------------------------------------- report export


@router.get("/p/export")
async def export(n: int = 4, owner: str = "Jean Dupont"):
    buffer = io.BytesIO()
    doc = SimpleDocTemplate(buffer, pagesize=A4)
    elements = []
    styles = getSampleStyleSheet()
    title_style = ParagraphStyle("CustomTitle", parent=styles["Heading1"], fontSize=24, textColor=colors.HexColor("#2c3e50"),
                                 spaceAfter=30, alignment=TA_CENTER)
    info_style = styles["Normal"]
    elements.append(Paragraph("Rapport des Propriétés", title_style))
    elements.append(Paragraph(f"<b>Propriétaire:</b> {owner}", info_style))
    elements.append(Paragraph("<b>Date du rapport:</b> 18/10/2026 10:00", info_style))
    elements.append(Spacer(1, 0.3 * inch))
    if n:
        total = 0.0
        for i in range(n):
            rent = 450.0 + 137.25 * i
            total += rent
            elements.append(Paragraph(f"{i + 1}. {i * 3 + 2} rue de la Paix — loyer {rent:.2f} € — <i>{'loué' if i % 2 else 'vacant'}</i>",
                                      info_style))
        elements.append(Spacer(1, 0.3 * inch))
        elements.append(Paragraph("Statistiques", styles["Heading2"]))
        elements.append(Paragraph(f"Total des loyers : {total:.2f} € ; moyenne : {total / n:.2f} €", styles["Normal"]))
    else:
        elements.append(Paragraph("Aucune propriété trouvée.", styles["Normal"]))
    doc.build(elements)
    return pdf(buffer, "export.pdf")


# ---------------------------------------------------------------- tables


def brand_header(meta_html: str = "", width: float = 17 * cm) -> Table:
    word = ParagraphStyle("BrandWordmark", fontName="Helvetica-Bold", fontSize=15, leading=17, textColor=INK, alignment=TA_LEFT)
    tag = ParagraphStyle("BrandTagline", fontName="Helvetica", fontSize=8, leading=10, textColor=MUTED, alignment=TA_LEFT)
    meta_style = ParagraphStyle("BrandMeta", fontName="Helvetica", fontSize=8.5, leading=12, textColor=MUTED, alignment=TA_RIGHT)
    identity = Table([[Paragraph(BRAND, word)], [Paragraph("Gestion locative simplifiée", tag)]], colWidths=[6 * cm])
    identity.setStyle(TableStyle([
        ("LEFTPADDING", (0, 0), (-1, -1), 0),
        ("RIGHTPADDING", (0, 0), (-1, -1), 0),
        ("TOPPADDING", (0, 0), (-1, -1), 0),
        ("BOTTOMPADDING", (0, 0), (0, 0), 1),
        ("BOTTOMPADDING", (0, 1), (0, 1), 0),
    ]))
    meta_cell = Paragraph(meta_html, meta_style) if meta_html else ""
    header = Table([[brand_logo(1.35 * cm), identity, meta_cell]], colWidths=[1.7 * cm, 7 * cm, width - 8.7 * cm])
    header.setStyle(TableStyle([
        ("VALIGN", (0, 0), (-1, -1), "MIDDLE"),
        ("LEFTPADDING", (0, 0), (0, 0), 0),
        ("LEFTPADDING", (1, 0), (-1, -1), 8),
        ("RIGHTPADDING", (0, 0), (-1, -1), 0),
        ("TOPPADDING", (0, 0), (-1, -1), 0),
        ("BOTTOMPADDING", (0, 0), (-1, -1), 0),
        ("LINEBELOW", (0, 0), (-1, -1), 1.2, PRIMARY),
        ("BOTTOMPADDING", (0, 0), (-1, -1), 10),
    ]))
    return header


@router.get("/p/receipt")
async def receipt(tenant: str = "Mme Durand", address: str = "12 rue des Lilas, 75011 Paris", rent: float = 950.0,
                  charges: float = 80.5, rows: int = 3, meta: str = "Quittance n° 2026-10<br/>Émise le 18/10/2026"):
    buffer = io.BytesIO()
    doc = SimpleDocTemplate(buffer, pagesize=A4, leftMargin=2 * cm, rightMargin=2 * cm, topMargin=1.8 * cm, bottomMargin=2.2 * cm,
                            title="Quittance de loyer", author=BRAND)
    styles = getSampleStyleSheet()
    small = ParagraphStyle("Small", parent=styles["Normal"], fontSize=8, leading=11, textColor=MUTED)
    normal = ParagraphStyle("N", parent=styles["Normal"], fontSize=9, leading=13, textColor=INK, alignment=TA_JUSTIFY)
    W = 17 * cm
    elements = [brand_header(meta, W), Spacer(1, 14)]
    info = [[Paragraph("<b>Locataire</b>", small), Paragraph(tenant or "—", normal)],
            [Paragraph("<b>Adresse du bien</b>", small), Paragraph(address or "—", normal)],
            [Paragraph("<b>Période</b>", small), "Octobre 2026"]]
    t = Table(info, colWidths=[5 * cm, W - 5 * cm])
    t.setStyle(TableStyle([
        ("VALIGN", (0, 0), (-1, -1), "TOP"),
        ("BOTTOMPADDING", (0, 0), (-1, -1), 3),
        ("TOPPADDING", (0, 0), (-1, -1), 3),
        ("ROWBACKGROUNDS", (0, 0), (-1, -1), [colors.HexColor("#f8fafc"), colors.white]),
        ("BOX", (0, 0), (-1, -1), 0.5, LINE),
        ("INNERGRID", (0, 0), (-1, -1), 0.3, LINE),
        ("FONTSIZE", (1, 2), (1, 2), 9),
    ]))
    elements += [t, Spacer(1, 12)]
    data = [["Désignation", "Montant"]]
    for i in range(rows):
        data.append([f"Loyer — mois {i + 1}", f"{rent:.2f} €"])
        data.append([f"Provision sur charges — mois {i + 1}", f"{charges:.2f} €"])
    data.append(["Total", f"{rows * (rent + charges):.2f} €"])
    amounts = Table(data, colWidths=[W - 4 * cm, 4 * cm], repeatRows=1, hAlign="LEFT")
    amounts.setStyle(TableStyle([
        ("BACKGROUND", (0, 0), (-1, 0), PRIMARY),
        ("TEXTCOLOR", (0, 0), (-1, 0), colors.white),
        ("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold"),
        ("ALIGN", (1, 0), (1, -1), "RIGHT"),
        ("FONTNAME", (0, -1), (-1, -1), "Helvetica-Bold"),
        ("LINEABOVE", (0, -1), (-1, -1), 1, INK),
        ("ROWBACKGROUNDS", (0, 1), (-1, -2), [colors.white, colors.HexColor("#EEF2FF")]),
        ("GRID", (0, 0), (-1, -2), 0.25, LINE),
    ]))
    elements += [amounts, Spacer(1, 12),
                 Paragraph("Je soussigné, bailleur, déclare avoir reçu la somme ci-dessus & en donne quittance, sous réserve "
                           "de tous mes droits.", normal)]
    doc.build(elements, onFirstPage=draw_page_footer, onLaterPages=draw_page_footer)
    return pdf(buffer, "receipt.pdf")


# ---------------------------------------------------------------- values and errors


@router.get("/p/values")
async def values():
    styles = getSampleStyleSheet()
    n = styles["Normal"]
    h = ParagraphStyle("H", parent=styles["Heading5"], fontSize=11.5)
    out = {
        "units": [cm, mm, inch], "A4": list(A4), "letter": list(letter), "land": list(landscape(A4)),
        "enums": [TA_LEFT, TA_CENTER, TA_RIGHT, TA_JUSTIFY],
        "normal": [n.name, n.fontName, n.fontSize, n.leading, n.alignment, n.spaceBefore, n.textColor.red, n.allowWidows],
        "h": [h.name, h.parent.name, h.fontSize, h.leading, h.spaceBefore, h.fontName, h.firstLineIndent],
        "in": ["Normal" in styles, "h1" in styles, "Nope" in styles, "Title" in styles],
        "colors": [colors.white.red, PRIMARY.red, PRIMARY.green, PRIMARY.blue, colors.grey.blue, colors.HexColor("#2c3e50").blue],
        "title": styles["title"].name,
    }
    buf = io.BytesIO()
    doc = SimpleDocTemplate(buf, pagesize=A4, leftMargin=2 * cm, rightMargin=1.5 * cm, title="T")
    out["doc"] = [doc.width, doc.height, doc.leftMargin, doc.topMargin, doc.title, list(doc.pagesize)]
    story = [Paragraph("x", n)]
    doc.build(story)
    out["after"] = [len(story), doc.page, len(buf.getvalue()) > 500]
    return out


@router.get("/p/errors/{case}")
async def errors(case: str):
    styles = getSampleStyleSheet()
    buf = io.BytesIO()
    try:
        if case == "style":
            styles["Nope"]
        elif case == "add":
            styles.add(ParagraphStyle("Normal"))
        elif case == "markup":
            Paragraph("<b>unclosed", styles["Normal"])
        elif case == "tag":
            Paragraph("a <font size=x>b</font>", styles["Normal"])
        elif case == "layout":
            SimpleDocTemplate(buf, pagesize=A4).build([Spacer(10, 2000)])
        elif case == "font":
            SimpleDocTemplate(buf).build([Paragraph("x", styles["Normal"])], onFirstPage=lambda c, d: c.setFont("Nope", 9))
        elif case == "callback":
            def boom(canvas, doc):
                raise ValueError(f"page {doc.page}")
            SimpleDocTemplate(buf).build([Paragraph("x", styles["Normal"])], onFirstPage=boom)
        elif case == "polygon":
            Polygon([1, 2, 3])
        elif case == "caught":
            try:
                SimpleDocTemplate(buf).build([Spacer(1, 5000)])
            except LayoutError as e:
                return {"caught": str(e)}
        return {"ok": True}
    except Exception as e:  # noqa: BLE001 - the class and message are what is compared
        return {"error": type(e).__name__, "message": str(e)}


# ---------------------------------------------------------------- programs (tests/scenarios/docapp.py)


NAMED = {"black": colors.black, "white": colors.white, "grey": colors.grey, "lightgrey": colors.lightgrey, "red": colors.red,
         "darkblue": colors.darkblue, "transparent": colors.transparent}


def color_of(spec):
    if spec is None or isinstance(spec, list):
        return None if spec is None else colors.Color(spec[0], spec[1], spec[2])
    if spec.startswith("#"):
        return colors.HexColor(spec)
    return NAMED[spec]


def shape_of(spec):
    kind, args, kw = spec[0], spec[1], spec[2]
    if kind == "Rect":
        s = Rect(args[0], args[1], args[2], args[3])
    elif kind == "Circle":
        s = Circle(args[0], args[1], args[2])
    elif kind == "Polygon":
        s = Polygon(args)
    else:
        s = Group()
        for c in args:
            s.add(shape_of(c))
        if kw.get("scale"):
            s.scale(kw["scale"][0], kw["scale"][1])
        if kw.get("translate"):
            s.translate(kw["translate"][0], kw["translate"][1])
        return s
    for k, v in kw.items():
        setattr(s, k, color_of(v) if k.endswith("Color") else v)
    return s


def run_ops(canvas, doc, ops):
    for op in ops:
        name, args = op[0], op[1:]
        if name == "save":
            canvas.saveState()
        elif name == "restore":
            canvas.restoreState()
        elif name == "font":
            canvas.setFont(args[0], args[1])
        elif name == "fill":
            canvas.setFillColor(color_of(args[0]))
        elif name == "stroke":
            canvas.setStrokeColor(color_of(args[0]))
        elif name == "gray":
            canvas.setFillGray(args[0])
        elif name == "width":
            canvas.setLineWidth(args[0])
        elif name == "line":
            canvas.line(args[0], args[1], args[2], args[3])
        elif name == "rect":
            canvas.rect(args[0], args[1], args[2], args[3], stroke=args[4], fill=args[5])
        elif name == "text":
            w, h = canvas._pagesize
            t = args[2].replace("$P", str(canvas.getPageNumber())).replace("$D", str(doc.page))
            if args[3] == "r":
                canvas.drawRightString(w - args[0], args[1], t)
            elif args[3] == "c":
                canvas.drawCentredString(w / 2, args[1], t)
            else:
                canvas.drawString(args[0], args[1], t)


@router.post("/p/ops")
async def ops(program: dict = Body(...)):
    """A document: its options, styles (name, parent, attributes set one by one), story and footer operations; an
    exception is answered with its class and message."""
    buffer = io.BytesIO()
    try:
        d = program["doc"]
        doc = SimpleDocTemplate(buffer, pagesize=A4 if d["size"] == "A4" else letter, leftMargin=d["margins"][0],
                                rightMargin=d["margins"][1], topMargin=d["margins"][2], bottomMargin=d["margins"][3],
                                title=d.get("title"), author=d.get("author"))
        styles = getSampleStyleSheet()
        made = {}
        for name, parent, attrs in program["styles"]:
            st = ParagraphStyle(name, parent=made[parent] if parent in made else styles[parent])
            for k, v in attrs.items():
                setattr(st, k, color_of(v) if k.endswith("Color") else v)
            made[name] = st
        story = []
        for item in program["story"]:
            kind = item[0]
            if kind == "P":
                story.append(Paragraph(item[1], made[item[2]] if item[2] in made else styles[item[2]]))
            elif kind == "S":
                story.append(Spacer(item[1], item[2]))
            elif kind == "B":
                story.append(PageBreak())
            elif kind == "H":
                kw = item[1]
                story.append(HRFlowable(width=kw.get("width", "80%"), thickness=kw.get("thickness", 1),
                                        color=color_of(kw.get("color", "lightgrey")), spaceBefore=kw.get("spaceBefore", 1),
                                        spaceAfter=kw.get("spaceAfter", 1), hAlign=kw.get("hAlign", "CENTER"), dash=kw.get("dash")))
            elif kind == "D":
                dr = Drawing(item[1], item[2])
                for s in item[3]:
                    dr.add(shape_of(s))
                dr.hAlign = item[4]
                story.append(dr)
            elif kind == "T":
                cells = [[Paragraph(c[1], styles[c[2]]) if isinstance(c, list) else c for c in row] for row in item[1]]
                tb = Table(cells, colWidths=item[2], repeatRows=item[3])
                tb.setStyle(TableStyle([(c[0], tuple(c[1]), tuple(c[2]), *c[3:]) for c in item[4]]))
                story.append(tb)
            elif kind == "M":  # a style changed after the paragraphs that use it were made
                setattr(made[item[1]], item[2], item[3])
        first, later = program.get("first"), program.get("later")
        if first is not None and later is not None:
            doc.build(story, onFirstPage=lambda c, dd: run_ops(c, dd, first), onLaterPages=lambda c, dd: run_ops(c, dd, later))
        elif first is not None:
            doc.build(story, onFirstPage=lambda c, dd: run_ops(c, dd, first))
        else:
            doc.build(story)
    except Exception as e:  # noqa: BLE001 - the class and message are what is compared
        return {"error": type(e).__name__, "message": str(e)}
    return pdf(buffer, "ops.pdf")
