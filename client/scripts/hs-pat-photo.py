#!/usr/bin/env python3
"""Rig test picture: a detailed desktop with a detailed window moving in circles, full screen on the
rig's captured output, at FPS. Run through `hs-rig.sh run --`."""
import sys, time, math, subprocess
import numpy as np
import gi
gi.require_version('Gst','1.0')
from gi.repository import Gst
Gst.init(None)
fps=int(sys.argv[1]) if len(sys.argv)>1 else 240
W,H=1920,1080
rng=np.random.default_rng(1)
yy,xx=np.mgrid[0:H,0:W].astype(np.float32)
def field(ph):
    c=[128+90*np.sin(xx/(60+ph*13)+yy/(90+ph*7)+ph)+25*np.sin(xx/9.0+ph)*np.cos(yy/7.0) for _ in range(1)][0]
    return c
def photo(h,w,seed,amp):
    r=np.random.default_rng(seed)
    y,x=np.mgrid[0:h,0:w].astype(np.float32)
    ch=[]
    for k in range(3):
        base=128+90*np.sin(x/(60+k*13)+y/(90+k*7)+k)+25*np.sin(x/9.0+k)*np.cos(y/7.0)
        ch.append(np.clip(base+r.normal(0,amp,(h,w)),0,255).astype(np.uint8))
    return np.stack(ch+[np.full((h,w),255,np.uint8)],axis=2).copy()
bg=photo(H,W,1,9.0)
WW,WH=480,360
win=photo(WH,WW,2,12.0)
win[WH//2-24:WH//2+24,WW//2-24:WW//2+24]=(255,0,0,255)
R=250; CX,CY=960,540; PERIOD=2.0
buf=bg.copy()
last=[None]
def frame(t):
    a=2*math.pi*t/PERIOD
    x,y=int(CX+R*math.cos(a)),int(CY+R*math.sin(a))
    x0,y0=x-WW//2,y-WH//2
    if last[0]:
        lx,ly=last[0]; buf[ly:ly+WH,lx:lx+WW]=bg[ly:ly+WH,lx:lx+WW]
    buf[y0:y0+WH,x0:x0+WW]=win
    last[0]=(x0,y0)
    return buf.tobytes()
import os; open(sys.argv[2],"w").write(str(os.getpid()))
subprocess.run(['swaymsg','focus output HEADLESS-1'],capture_output=True)
subprocess.run(['swaymsg','workspace number 1'],capture_output=True)
p=Gst.parse_launch(f"appsrc name=src is-live=true format=time do-timestamp=true caps=video/x-raw,format=BGRx,width={W},height={H},framerate={fps}/1 ! waylandsink fullscreen=true sync=false")
src=p.get_by_name('src')
p.set_state(Gst.State.PLAYING)
bus=p.get_bus()
per=1.0/fps; t0=time.monotonic(); i=0
while True:
    i+=1
    t=t0+i*per
    d=t-time.monotonic()
    if d>0: time.sleep(d)
    src.emit('push-buffer',Gst.Buffer.new_wrapped(frame(t-t0)))
    m=bus.pop_filtered(Gst.MessageType.ERROR)
    if m: print(m.parse_error(),flush=True); break
