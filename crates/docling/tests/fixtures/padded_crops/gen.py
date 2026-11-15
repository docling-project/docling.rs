"""#1133's reporter case: a 200 dpi page scan (DejaVu text as a JPEG) plus one
native Helvetica page number. The heading touches its paragraph, so the
paragraph's padded detector crop sees most of the heading too.

Run: python3 gen.py  (Pillow + DejaVu fonts; the font dir may differ)."""
import io
from PIL import Image, ImageDraw, ImageFont

DPI = 200
S = DPI / 72
F = "/usr/share/fonts/truetype/dejavu/"
pt = lambda v: int(v * S)
BLOCKS = [
    ("Opening Hours", ["The reading room is open from nine in the morning until six in the evening",
                       "on weekdays, and from ten until four on Saturdays and public holidays."]),
    ("Borrowing Rules", ["Members may borrow up to eight items at a time for a period of three weeks.",
                         "Items can be renewed twice unless another member has placed a hold on them."]),
    ("Study Rooms", ["Group study rooms can be booked at the front desk for up to two hours a day."]),
]

img = Image.new("L", (pt(612), pt(792)), 255)
d = ImageDraw.Draw(img)
fb = ImageFont.truetype(F + "DejaVuSans-Bold.ttf", pt(13))
fr = ImageFont.truetype(F + "DejaVuSans.ttf", pt(9.5))
y = 90
for h, lines in BLOCKS:
    d.text((pt(72), pt(y)), h, font=fb, fill=0)
    y += 14 + 6
    for l in lines:
        d.text((pt(72), pt(y)), l, font=fr, fill=0)
        y += 13
    y += 30
buf = io.BytesIO()
img.save(buf, "JPEG", quality=75)
jpg = buf.getvalue()

content = b"q 612 0 0 792 0 0 cm /Im0 Do Q\nBT /F1 8 Tf 300 30 Td (3) Tj ET\n"
objs = [
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> "
    b"/XObject << /Im0 6 0 R >> >> /Contents 4 0 R >>",
    b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream",
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    b"<< /Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceGray "
    b"/BitsPerComponent 8 /Filter /DCTDecode /Length %d >>\nstream\n" % (img.width, img.height, len(jpg))
    + jpg + b"\nendstream",
]
out = bytearray(b"%PDF-1.4\n")
offs = []
for i, o in enumerate(objs):
    offs.append(len(out))
    out += b"%d 0 obj\n" % (i + 1) + o + b"\nendobj\n"
x = len(out)
out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
for o in offs:
    out += b"%010d 00000 n \n" % o
out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, x)
open("scan_with_page_number.pdf", "wb").write(out)
