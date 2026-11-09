# Synthetic NxM config from a base config.json given in $BASE_CONFIG (required; never defaults to the live config).
# Usage: BASE_CONFIG=... gen_config.py N_INPUTS N_BUSES HOME_DIR
import json,sys,os
import sys as _s
if 'BASE_CONFIG' not in os.environ: _s.exit('BASE_CONFIG is required')
src=json.load(open(os.environ['BASE_CONFIG']))
nin,nbus,home=int(sys.argv[1]),int(sys.argv[2]),sys.argv[3]
base=src['inputs'][1]
names=['system','spotify','discord','browser','game','mic','piano','stems']+[f'x{k}' for k in range(20)]
ins=[]
for n in names[:nin]:
    s=[i for i in src['inputs'] if i['id']==n]
    i=json.loads(json.dumps(s[0] if s else base)); i['id']=n; i['name']=n.title()
    if n=='mic': i['kind']={'type':'source','target':'easyeffects_source'}
    else: i['kind']={'type':'virtual'}
    ins.append(i)
buses=[b for b in src['buses']]
hw=[b for b in buses if b['kind']['type']=='hardware']; vb=[b for b in buses if b['kind']['type']=='virtual']
allb=hw+vb
extra=[]
for k in range(10):
    extra.append({'id':f'b{k+3}','name':f'B{k+3}','color':'#fff','kind':{'type':'virtual','node':f'vmix{k}','description':f'VMix {k}'},'gain_db':0.0,'mute':False})
# order: a1 a2 a3 b1 b2 then extra; nbus picks first nbus (7x4 -> drop b2? use a1 a2 b1 b2)
order=[hw[0],hw[1],vb[0],vb[1],hw[2]]+extra
if nbus<=4: sel=order[:4]
else: sel=order[:4]+[hw[2]]+extra[:nbus-5]
for i in ins:
    i['routes']={b['id']:{'on':True,'level_db':0.0} for b in sel}
c=dict(src);c['inputs']=ins;c['buses']=sel
d=os.path.join(home,'.config/wavelink-mixer');os.makedirs(d,exist_ok=True)
json.dump(c,open(os.path.join(d,'config.json'),'w'))
print([b['id'] for b in sel])
