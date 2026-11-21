"""Regenerate the #1219 picture-OCR PDF fixture (Pillow + DejaVu Sans Bold).

    python3 crates/docling/tests/fixtures/picture_text/generate.py

picture_text.pdf  one page with a Helvetica text layer, a photo-like raster
                  (8.8 % of the page) whose rating label reads
                  `MODEL DEMO-123` / `LOT 12-34567-8`, and a small seal
                  (0.7 %, under the 5 % OCR threshold) reading `LAB` / `1234`.
                  Every word and number is a placeholder.
"""
import io
import os
import zlib
from PIL import Image, ImageDraw, ImageFont
F='/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf'
def photo():
    w,h=750,510
    im=Image.new('RGB',(w,h))
    d=ImageDraw.Draw(im)
    for y in range(h):
        d.line([(0,y),(w,y)],fill=(60+y//6,90+y//8,120+y//10))
    d.ellipse([40,60,330,460],fill=(150,110,80))
    d.rectangle([380,140,710,360],fill=(245,245,240),outline=(30,30,30),width=4)
    f=ImageFont.truetype(F,30)
    d.text((405,180),'MODEL DEMO-123',font=f,fill=(10,10,10))
    d.text((405,270),'LOT 12-34567-8',font=f,fill=(10,10,10))
    return im
def seal():
    w=h=240
    im=Image.new('RGB',(w,h),(255,255,255))
    d=ImageDraw.Draw(im)
    d.ellipse([6,6,234,234],outline=(20,40,140),width=10)
    d.ellipse([30,30,210,210],outline=(20,40,140),width=4)
    f=ImageFont.truetype(F,46)
    d.text((120,92),'LAB',font=f,fill=(20,40,140),anchor='mm')
    d.text((120,150),'1234',font=f,fill=(20,40,140),anchor='mm')
    return im
objs=[]
def add(b): objs.append(b); return len(objs)
def img_obj(im):
    data=zlib.compress(im.tobytes())
    return add(b'<< /Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode /Length %d >>\nstream\n' % (im.width,im.height,len(data))+data+b'\nendstream')
font=add(b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>')
fontb=add(b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>')
i1=img_obj(photo()); i2=img_obj(seal())
lines=[]
def t(x,y,s,sz=11,f='F1'): lines.append('BT /%s %d Tf %d %d Td (%s) Tj ET'%(f,sz,x,y,s))
t(72,730,'Laboratory Test Report',20,'F2')
t(72,700,'Report number: TR-0000-0001. Issued for demonstration purposes only.')
t(72,684,'The sample described below was received in good condition and tested on arrival.')
t(72,668,'All values in this generated document are placeholders, not real measurements.')
t(72,640,'1. Sample identification',13,'F2')
t(72,620,'The rating label of the tested unit is shown in the photograph below.')
cont='\n'.join(lines)+'\nq 250 0 0 170 72 430 cm /Im1 Do Q\n'
lines=[]
t(72,400,'2. Results',13,'F2')
t(72,380,'The unit passed every test of the applicable standard within the stated uncertainty.')
t(72,364,'Measured values are listed in the annex, which is not part of this demonstration page.')
t(72,348,'The laboratory is accredited for the tests marked in the scope of accreditation.')
t(72,200,'Signed: Head of laboratory',11)
cont+='\n'.join(lines)+'\nq 60 0 0 60 470 180 cm /Im2 Do Q\n'
c=cont.encode()
cs=add(b'<< /Length %d >>\nstream\n'%len(c)+c+b'\nendstream')
pages_id=len(objs)+2
page=add(b'<< /Type /Page /Parent %d 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 %d 0 R /F2 %d 0 R >> /XObject << /Im1 %d 0 R /Im2 %d 0 R >> >> /Contents %d 0 R >>'%(pages_id,font,fontb,i1,i2,cs))
add(b'<< /Type /Pages /Kids [%d 0 R] /Count 1 >>'%page)
cat=add(b'<< /Type /Catalog /Pages %d 0 R >>'%pages_id)
out=io.BytesIO(); out.write(b'%PDF-1.4\n')
offs=[]
for i,o in enumerate(objs):
    offs.append(out.tell()); out.write(b'%d 0 obj\n'%(i+1)+o+b'\nendobj\n')
x=out.tell()
out.write(b'xref\n0 %d\n0000000000 65535 f \n'%(len(objs)+1))
for o in offs: out.write(b'%010d 00000 n \n'%o)
out.write(b'trailer\n<< /Size %d /Root %d 0 R >>\nstartxref\n%d\n%%%%EOF\n'%(len(objs)+1,cat,x))
open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'picture_text.pdf'), 'wb').write(out.getvalue())
