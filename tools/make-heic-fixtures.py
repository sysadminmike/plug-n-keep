# Makes the HEIC test fixtures in core/src/heic/fixtures/ from a tiny HEIC made with Windows' own
# HEIF encoder (WIC, 64 x 48, left half magenta/red, a blue band on top): it adds an irot or imir
# property, an ICC profile (made here, not from a photo) and an EXIF item. No real photo is used.
# `dangling.heic` is built like some iPhone photos: a second image (item 3) is a depth image of the
# main one (`auxC` of type urn:mpeg:hevc:2015:auxid:2), `auxl 3 -> (1, 4)`, and item 4 is a `tmap`
# (HDR tone map) item, a type libheif 1.17 doesn't place among its images. libheif 1.17 fails such
# a file with "Non-existing item ID referenced: Non-existing depth image referenced" unless the
# reference is retired first.
import struct,sys
base=open(sys.argv[1],'rb').read()
out=sys.argv[2]
def box(t,body): return struct.pack('>I4s',len(body)+8,t)+body
def full(t,v,fl,body): return box(t,struct.pack('>I',(v<<24)|fl)+body)
# pull original pieces
def boxes(b,off,end):
    r={}
    while off+8<=end:
        sz,t=struct.unpack('>I4s',b[off:off+8]); r[t]=(off,sz); off+=sz
    return r
top=boxes(base,0,len(base))
ftyp=base[0:32]
mo,ms=top[b'meta']
meta_children=boxes(base,mo+12,mo+ms)
hdlr=base[meta_children[b'hdlr'][0]:][:meta_children[b'hdlr'][1]]
pitm=base[meta_children[b'pitm'][0]:][:meta_children[b'pitm'][1]]
io,isz=meta_children[b'iprp']
ipco_off,ipco_sz=boxes(base,io+8,io+isz)[b'ipco']
ipco_children=base[ipco_off+8:ipco_off+ipco_sz]
mdo,mds=top[b'mdat']; payload=base[mdo+8:mdo+mds]

def icc():
    def xyz(x,y,z): return b'XYZ \0\0\0\0'+struct.pack('>iii',int(x*65536),int(y*65536),int(z*65536))
    def pad(b): return b+b'\0'*((-len(b))%4)
    desc=b'desc\0\0\0\0'+struct.pack('>I',9)+b'PNK test\0'+b'\0'*(4+4+2+1+67)  # ascii part + empty unicode/scriptcode
    curv=b'curv\0\0\0\0'+struct.pack('>IH',1,0x0233)+b'\0\0'
    cprt=b'text\0\0\0\0'+b'none\0'
    tags=[(b'desc',desc),(b'cprt',cprt),(b'wtpt',xyz(.9642,1,.8249)),(b'rXYZ',xyz(.5151,.2412,-.0011)),(b'gXYZ',xyz(.292,.6922,.0419)),(b'bXYZ',xyz(.1571,.0666,.7841)),(b'rTRC',curv),(b'gTRC',curv),(b'bTRC',curv)]
    n=len(tags); off=128+4+12*n; table=b''; data=b''
    for t,d in tags:
        d=pad(d); table+=t+struct.pack('>II',off+len(data),len(d)); data+=d
    total=off+len(data)
    h=struct.pack('>I4sI4s4s4s',total,b'none',0x02100000,b'mntr',b'RGB ',b'XYZ ')+b'\0'*12+b'acsp'+b'\0'*4+b'\0'*4+b'\0'*4+b'\0'*4+b'\0'*8+struct.pack('>I',0)+struct.pack('>iii',int(.9642*65536),65536,int(.8249*65536))+b'\0'*48
    h=h[:128].ljust(128,b'\0')
    return h+struct.pack('>I',n)+table+data

def tiff(be,orient):
    e='>' if be else '<'
    t=(b'MM\0*' if be else b'II*\0')+struct.pack(e+'I',8)
    ents=[(0x010F,2,4,b'PNK\0'),(0x0112,3,1,struct.pack(e+'H',orient)+b'\0\0')]
    t+=struct.pack(e+'H',len(ents))
    for tag,ty,cnt,val in ents: t+=struct.pack(e+'HHI',tag,ty,cnt)+val
    return t+struct.pack(e+'I',0)

def build(props,exif,dangling=False):
    auxc=full(b'auxC',0,0,b'urn:mpeg:hevc:2015:auxid:2'+bytes([0]))
    ipco=box(b'ipco',ipco_children+b''.join(props)+(auxc if dangling else b''))
    assoc=bytes([1,0x82,3])+bytes([0x80|(4+i) for i in range(len(props))])
    ids=[1,3] if dangling else [1]
    def assoc_of(i): return assoc+bytes([0x80|(4+len(props))]) if i==3 else assoc
    ipma=full(b'ipma',0,0,struct.pack('>I',len(ids))+b''.join(struct.pack('>HB',i,len(assoc_of(i)))+assoc_of(i) for i in ids))
    iprp=box(b'iprp',ipco+ipma)
    def infe(i,t): return full(b'infe',2,0,struct.pack('>HH4s',i,0,t)+b'\0')
    items=[infe(1,b'hvc1')]
    if exif: items.append(infe(2,b'Exif'))
    if dangling: items+=[infe(3,b'hvc1'),infe(4,b'tmap')]
    iinf=full(b'iinf',0,0,struct.pack('>H',len(items))+b''.join(items))
    refs=(box(b'cdsc',struct.pack('>HHH',2,1,1)) if exif else b'')+(box(b'auxl',struct.pack('>HHHH',3,2,1,4)) if dangling else b'')
    iref=full(b'iref',0,0,refs) if refs else b''
    item=struct.pack('>I',0)+exif if exif else b''
    tm=bytes(8) if dangling else b''
    def iloc(o1):
        n=1+(1 if exif else 0)+(2 if dangling else 0)
        b=bytes([0x44,0])+struct.pack('>H',n)
        b+=struct.pack('>HHHII',1,0,1,o1,len(payload))
        if exif: b+=struct.pack('>HHHII',2,0,1,o1+len(payload),len(item))
        if dangling:
            b+=struct.pack('>HHHII',3,0,1,o1,len(payload))
            b+=struct.pack('>HHHII',4,0,1,o1+len(payload)+len(item),len(tm))
        return full(b'iloc',0,0,b)
    def meta(o1): return box(b'meta',struct.pack('>I',0)+hdlr+pitm+iinf+iloc(o1)+iref+iprp)
    o1=len(ftyp)+len(meta(0))+8
    return ftyp+meta(o1)+box(b'mdat',payload+item+tm)

def prop(t,b): return box(t,b)
fx={
 'plain':build([],None),
 'rotated':build([prop(b'irot',bytes([1]))],tiff(True,6)),
 'mirrored':build([prop(b'imir',bytes([0]))],None),
 'icc':build([box(b'colr',b'prof'+icc())],tiff(False,1)),
 'dangling':build([prop(b'irot',bytes([1]))],tiff(True,6),dangling=True),
}
for k,v in fx.items(): open(f'{out}/{k}.heic','wb').write(v); print(k,len(v))
