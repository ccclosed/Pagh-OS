#!/usr/bin/env python3
"""
Pagh-OS MCP server (v2, 13 tools).

Zapusk:
    export PAGH_MCP_TOKEN=$(openssl rand -hex 24)
    python3 pagh_mcp.py
Slushaet tolko 127.0.0.1:8765; naruzhu vystavlyaetsya cloudflared-tunnelem.
Kazhdyi zapros proveryaet Authorization: Bearer <PAGH_MCP_TOKEN>.
"""
import os
import shutil
import subprocess
import sys
import time

from mcp.server.fastmcp import FastMCP, Image

TOKEN = os.environ.get("PAGH_MCP_TOKEN", "")
if len(TOKEN) < 16:
    sys.exit("PAGH_MCP_TOKEN ne zadan ili koroche 16 simvolov.\n"
             "Sdelai: export PAGH_MCP_TOKEN=$(openssl rand -hex 24)")

PROJECT = os.path.expanduser(
    os.environ.get("PAGH_PROJECT", "~/\u0420\u0430\u0431\u043e\u0447\u0438\u0439 \u0441\u0442\u043e\u043b/Pagh-OS-hardened"))

HOST, PORT = "127.0.0.1", 8765
MAX_OUT = 60_000

# Otklyuchaem DNS-rebinding zashchitu SDK: my za tunnelem, Host prikhodit
# vneshnii (*.lhr.life / *.trycloudflare.com); dostup i tak zakryt Bearer-tokenom.
try:
    from mcp.server.transport_security import TransportSecuritySettings
    _sec = TransportSecuritySettings(enable_dns_rebinding_protection=False)
    mcp = FastMCP("pagh-os", host=HOST, port=PORT, transport_security=_sec)
except Exception:
    mcp = FastMCP("pagh-os", host=HOST, port=PORT)


def _abs(path: str) -> str:
    path = os.path.expanduser(path or ".")
    return path if os.path.isabs(path) else os.path.join(PROJECT, path)


def _run(command: str, timeout_s: int, cwd: str = "") -> str:
    cwd = _abs(cwd) if cwd else PROJECT
    if not os.path.isdir(cwd):
        return f"exit=-1\ncwd ne sushchestvuet: {cwd}"
    try:
        p = subprocess.run(["bash", "-lc", command], cwd=cwd,
                           capture_output=True, text=True, errors="replace",
                           timeout=min(max(timeout_s, 1), 3600))
        out = p.stdout or ""
        if p.stderr:
            out += "\n--- stderr ---\n" + p.stderr
        if len(out) > MAX_OUT:
            out = "[...nachalo obrezano...]\n" + out[-MAX_OUT:]
        return f"exit={p.returncode}\n{out}"
    except subprocess.TimeoutExpired as e:
        out = e.stdout or b""
        if isinstance(out, bytes):
            out = out.decode(errors="replace")
        return f"exit=TIMEOUT({timeout_s}s)\n{out[-MAX_OUT:]}"


# ------------------------- obshchie -------------------------

@mcp.tool()
def sh(command: str, timeout_s: int = 120, cwd: str = "") -> str:
    """Run a shell command. cwd defaults to the Pagh-OS project root."""
    return _run(command, timeout_s, cwd)


@mcp.tool()
def sysinfo() -> str:
    """Toolchain and host info: rustc, cargo, qemu, disk, project path."""
    return _run("echo PROJECT=$PWD; rustc --version 2>&1; cargo --version 2>&1; "
                "qemu-system-x86_64 --version 2>&1 | head -1; df -h . | tail -1", 60)


# ------------------------- fayly -------------------------

@mcp.tool()
def ls(path: str = ".") -> str:
    """List a directory (relative to project root unless absolute)."""
    return _run(f"ls -la {_abs(path)!r}", 30)


@mcp.tool()
def read_file(path: str, start_line: int = 1, end_line: int = 200) -> str:
    """Read file lines [start_line..end_line] with line numbers."""
    try:
        with open(_abs(path), errors="replace") as f:
            lines = f.readlines()
    except OSError as e:
        return f"error: {e}"
    s = max(1, start_line) - 1
    e = min(len(lines), max(s + 1, end_line))
    body = "".join(f"{i+1}: {lines[i]}" for i in range(s, e))
    return f"total_lines={len(lines)}\n{body[:MAX_OUT]}"


@mcp.tool()
def write_file(path: str, content: str) -> str:
    """Create/overwrite a file with the given content."""
    p = _abs(path)
    try:
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(content)
        return f"written {len(content)} chars -> {p}"
    except OSError as e:
        return f"error: {e}"


@mcp.tool()
def patch_replace(path: str, old_str: str, new_str: str,
                  expected_count: int = 1) -> str:
    """Exact string replacement with occurrence-count assertion.
    If the file contains old_str a different number of times than
    expected_count, NOTHING is written and the real count is reported.
    For .rs files the brace balance delta is reported after writing."""
    p = _abs(path)
    try:
        s = open(p, errors="replace").read()
    except OSError as e:
        return f"error: {e}"
    n = s.count(old_str)
    if n != expected_count:
        return f"ABORT: old_str found {n} times, expected {expected_count}; file untouched"
    s2 = s.replace(old_str, new_str)
    open(p, "w").write(s2)
    extra = ""
    if p.endswith(".rs"):
        extra = f"; brace_delta={s2.count('{') - s2.count('}')}"
    return f"replaced {n} occurrence(s) in {p}{extra}"


@mcp.tool()
def grep(pattern: str, glob: str = "*.rs", max_lines: int = 120) -> str:
    """Search the project (grep -rn) limited to files matching glob."""
    return _run(f"grep -rn --include={glob!r} -e {pattern!r} . | head -n {int(max_lines)}", 60)


# ------------------------- git -------------------------

@mcp.tool()
def git(args: str, timeout_s: int = 60) -> str:
    """Run git in the project, e.g. 'status', 'log --oneline -5', 'diff', 'add -A', 'commit -m msg'."""
    return _run(f"git {args}", timeout_s)


# ------------------------- sborka i zapusk -------------------------

@mcp.tool()
def build(timeout_s: int = 2400) -> str:
    """Build Pagh-OS (./build.sh --release), return the tail of the output."""
    return _run("./build.sh --release 2>&1 | tail -c 60000", timeout_s)


@mcp.tool()
def run_os(seconds: int = 45) -> str:
    """Boot the OS (./run.sh --release) for N seconds, then kill QEMU.
    Returns captured serial/stdout. The QEMU window appears on the host
    display, so a human can watch/type while it runs."""
    out = _run(f"timeout --foreground {int(seconds)} ./run.sh --release 2>&1 | tail -c 60000",
               int(seconds) + 60)
    subprocess.run(["pkill", "-f", "qemu-system-x86_64"], capture_output=True)
    return out


@mcp.tool()
def kill_qemu() -> str:
    """Kill any leftover qemu-system-x86_64 processes."""
    p = subprocess.run(["pkill", "-f", "qemu-system-x86_64"], capture_output=True)
    return "killed" if p.returncode == 0 else "no qemu process found"


@mcp.tool()
def serial_tail(lines: int = 200, path: str = "serial.log") -> str:
    """Tail a log file (default serial.log in the project root)."""
    return _run(f"tail -n {int(lines)} {path!r}", 30)


# ------------------------- ekran -------------------------

@mcp.tool()
def screenshot() -> Image:
    """Screenshot of the host display (to see the QEMU window).
    Tries spectacle (KDE), gnome-screenshot, then ImageMagick import."""
    out = "/tmp/pagh_mcp_shot.png"
    try:
        os.remove(out)
    except OSError:
        pass
    cmds = [["spectacle", "-b", "-n", "-o", out],
            ["gnome-screenshot", "-f", out],
            ["import", "-window", "root", out]]
    for c in cmds:
        if shutil.which(c[0]):
            subprocess.run(c, capture_output=True, timeout=30)
            for _ in range(20):
                if os.path.exists(out) and os.path.getsize(out) > 0:
                    return Image(path=out)
                time.sleep(0.25)
    raise RuntimeError("no screenshot tool worked (need spectacle / gnome-screenshot / imagemagick)")


# ------------------------- auth + start -------------------------
from starlette.middleware.base import BaseHTTPMiddleware
from starlette.responses import PlainTextResponse


class Auth(BaseHTTPMiddleware):
    async def dispatch(self, request, call_next):
        if request.headers.get("authorization", "") != f"Bearer {TOKEN}":
            return PlainTextResponse("unauthorized", status_code=401)
        # Perepisyvaem Host na lokalnyi, chtoby transport SDK ne otbrasyval
        # zaprosy, prishedshie cherez tunnel (rabotaet na lyuboi versii SDK).
        local = f"{HOST}:{PORT}".encode()
        request.scope["headers"] = [
            (k, local if k == b"host" else v)
            for k, v in request.scope["headers"]
        ]
        return await call_next(request)


app = mcp.streamable_http_app()
app.add_middleware(Auth)

if __name__ == "__main__":
    import uvicorn
    print(f"Pagh-OS MCP v2.1 (13 tools): http://{HOST}:{PORT}/mcp")
    print(f"project: {PROJECT}")
    uvicorn.run(app, host=HOST, port=PORT, log_level="warning")
