"""Launch a private synthetic library and capture only the application's window."""
import ctypes, ctypes.wintypes as wt, os, pathlib, subprocess, time
from PIL import Image, ImageDraw, ImageGrab

root=pathlib.Path(__file__).resolve().parents[1]
out=root/'test-output'
photos=out/'smoke-photos'
photos.mkdir(parents=True,exist_ok=True)
for i in range(48):
    image=Image.new('RGB',(1200,800),(24+i*3,55+i,85+i*2))
    draw=ImageDraw.Draw(image)
    draw.ellipse((710,100,900,290),fill=(239,193,115))
    draw.polygon([(0,650),(350,250),(700,650)],fill=(76,133,151))
    draw.polygon([(350,800),(850,350),(1200,800)],fill=(41,90,111))
    draw.text((40,40),f'MyGoImage / test frame {i+1:02}',fill='white',font_size=36)
    image.save(photos/f'{i:03}.png')
env=os.environ.copy();env['LOCALAPPDATA']=str(out/'smoke-appdata')
env['PATH']=os.pathsep.join([os.path.join(os.environ.get('SystemRoot','C:\\Windows'),'System32'),os.environ.get('SystemRoot','C:\\Windows')])
exe=pathlib.Path(os.environ.get('MYGO_SMOKE_EXE',root/'target/debug/mygoimage.exe'))
log=(out/'smoke-stderr.log').open('w')
p=subprocess.Popen([str(exe),str(photos)],cwd=exe.parent,env=env,stdout=log,stderr=log)
u=ctypes.WinDLL('user32',use_last_error=True)
u.FindWindowW.argtypes=[wt.LPCWSTR,wt.LPCWSTR];u.FindWindowW.restype=wt.HWND
u.GetWindowRect.argtypes=[wt.HWND,ctypes.POINTER(wt.RECT)]
u.PostMessageW.argtypes=[wt.HWND,wt.UINT,wt.WPARAM,wt.LPARAM]
u.SetForegroundWindow.argtypes=[wt.HWND]
u.SetWindowPos.argtypes=[wt.HWND,wt.HWND,ctypes.c_int,ctypes.c_int,ctypes.c_int,ctypes.c_int,wt.UINT]
deadline=time.monotonic()+30
hwnd=None
while time.monotonic()<deadline:
    if p.poll() is not None:raise RuntimeError(f'Application exited {p.returncode}; see {log.name}')
    hwnd=u.FindWindowW(None,'MyGoImage')
    if hwnd:break
    time.sleep(.1)
if not hwnd:raise RuntimeError('Window did not appear')
u.SetForegroundWindow(hwnd)
time.sleep(3)
def capture(name):
    u.SetWindowPos(hwnd,wt.HWND(-1),0,0,0,0,0x13)
    time.sleep(.15)
    rect=wt.RECT();u.GetClientRect(hwnd,ctypes.byref(rect));point=wt.POINT(0,0);u.ClientToScreen(hwnd,ctypes.byref(point))
    ImageGrab.grab(bbox=(point.x,point.y,point.x+rect.right,point.y+rect.bottom)).save(out/name)
    u.SetWindowPos(hwnd,wt.HWND(-2),0,0,0,0,0x13)
capture('ui-library.png')
# Post keys directly to this window, never to other desktop applications.
for key in [0x27,0x27,0x23]:
    u.PostMessageW(hwnd,0x100,key,0);u.PostMessageW(hwnd,0x101,key,0);time.sleep(.25)
capture('ui-navigation.png')
def click(x,y):
    point=(y<<16)|x
    u.PostMessageW(hwnd,0x200,0,point);time.sleep(.05)
    u.PostMessageW(hwnd,0x201,1,point);time.sleep(.05)
    u.PostMessageW(hwnd,0x202,0,point)
click(445,92) # 100%
click(577,92) # Rotate display
time.sleep(.3)
u.PostMessageW(hwnd,0x200,0,(400<<16)|760)
time.sleep(.05)
u.PostMessageW(hwnd,0x201,1,(400<<16)|760)
time.sleep(.05)
u.PostMessageW(hwnd,0x200,1,(450<<16)|820)
time.sleep(.05)
u.PostMessageW(hwnd,0x202,0,(450<<16)|820)
time.sleep(.3)
capture('ui-transform.png')
print(f'pid={p.pid} hwnd={hwnd} screenshots={out}')
# Leave the app open for visual verification; caller closes this exact PID.
(out/'smoke.pid').write_text(str(p.pid))
