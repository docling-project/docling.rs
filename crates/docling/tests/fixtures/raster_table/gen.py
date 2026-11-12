"""The #1114 fixture: body paragraphs, a text caption, and under it a table
that is pixels only (a PNG drawn onto the page), so the table region holds no
text-layer cell. Converted without OCR, the table must stay an (empty) item
and keep its caption.

    pip install reportlab pillow
    python gen.py raster_table_caption.pdf
"""
import io, sys
from PIL import Image, ImageDraw, ImageFont
from reportlab.lib.pagesizes import A4
from reportlab.lib.utils import ImageReader
from reportlab.pdfgen import canvas

rows = [
    ["Factors", "Self-insurance", "Partial", "Insurance"],
    ["Probability of loss", "High", "Medium", "Very low"],
    ["Severity of loss", "Low", "Medium", "High"],
    ["Consequences", "Minor", "Serious", "Catastrophic"],
]
img = Image.new("RGB", (1000, 260), "white")
d = ImageDraw.Draw(img)
font = ImageFont.load_default(size=26)
for r, row in enumerate(rows):
    for c, text in enumerate(row):
        x, y = c * 250, r * 65
        d.rectangle([x, y, x + 249, y + 64], outline="black", width=2)
        d.text((x + 12, y + 18), text, fill="black", font=font)
buf = io.BytesIO()
img.save(buf, "PNG")
buf.seek(0)

c = canvas.Canvas(sys.argv[1], pagesize=A4, invariant=1)
c.setFont("Times-Roman", 12)
body = [
    "There are several rules for choosing a course of action in risky situations.",
    "Do not risk more than you can afford to lose. Do not risk a lot for a little.",
    "Think about the consequences before you act, and keep a reserve fund.",
]
y = 770
for _ in range(6):
    for line in body:
        c.drawString(57, y, line)
        y -= 16
    y -= 26
c.drawCentredString(A4[0] / 2, 202, "Table 1: Decision matrix for risk insurance")
c.drawImage(ImageReader(buf), 60, 57, width=500, height=130)
c.save()
