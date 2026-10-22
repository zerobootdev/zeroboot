import subprocess, time, sys

# Test Python pipe protocol without numpy
proc = subprocess.Popen(
    ["/usr/bin/python3", "-u", "-c", """
import sys, io, traceback
_g = {'__builtins__': __builtins__, '__name__': '__main__'}
sys.stdout.write('PYREADY\\n'); sys.stdout.flush()
while True:
    line = sys.stdin.readline()
    if not line: break
    code = line.rstrip('\\n')
    cap = io.StringIO()
    old_out, old_err = sys.stdout, sys.stderr
    sys.stdout = sys.stderr = cap
    try: exec(compile(code, '<sandbox>', 'exec'), _g)
    except SystemExit: pass
    except: traceback.print_exc()
    finally: sys.stdout = old_out; sys.stderr = old_err
    out = cap.getvalue()
    old_out.write(out + 'PYDONE\\n'); old_out.flush()
"""],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE
)

# Wait PYREADY
start = time.time()
buf = b""
while True:
    c = proc.stdout.read(1)
    if not c: print('ERROR'); break
    buf += c
    if buf.endswith(b'PYREADY\n'):
        print(f'PYREADY in {time.time()-start:.3f}s', flush=True)
        break

# Test commands
for code in ['print(1+1)', 'x=42; print(x*2)', 'import os; print(os.getcwd())']:
    proc.stdin.write((code + '\n').encode())
    proc.stdin.flush()
    out = b''
    while True:
        c = proc.stdout.read(1)
        if not c: break
        out += c
        if out.endswith(b'PYDONE\n'):
            print(f'  {code!r} -> {out[:-7].decode().strip()!r}')
            break

proc.terminate()
print('All tests passed')
