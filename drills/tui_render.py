#!/usr/bin/env python3
"""Render the wukong dashboard the way a person sees it, for the drills.

The TUI had no automated coverage at all, because there was no way to
look at it: `script -q /dev/null wukong` hands ratatui a 0x0 viewport
when there is no controlling terminal, so it draws nothing and every
assertion passes vacuously. This opens a pty, sets its window size
explicitly, and replays the terminal's own escape sequences into a
character grid — so what a drill greps is the FRAME, not the stream of
redraws that produced it.

    python3 drills/tui_render.py <path-to-wukong> [keys...]

Each key is sent after a pause and a frame is printed for each. Keys are
single characters, or TAB / ESC / ENTER.
"""
import fcntl, os, pty, re, select, signal, struct, sys, termios, time

COLS, ROWS = 120, 40


def render(stream: bytes, cols=COLS, rows=ROWS) -> str:
    """A minimal terminal emulator: enough of the escape vocabulary that
    ratatui's absolute-positioned redraws land in the right cells."""
    grid = [[" "] * cols for _ in range(rows)]
    cy = cx = 0
    i = 0
    text = stream.decode("utf-8", "replace")
    while i < len(text):
        ch = text[i]
        if ch == "\x1b":
            m = re.match(r"\x1b\[([0-9;?]*)([a-zA-Z])", text[i:])
            if not m:
                m2 = re.match(r"\x1b[()][B0]|\x1b[=>]|\x1b\][^\x07]*\x07", text[i:])
                i += (m2.end() if m2 else 1)
                continue
            args, cmd = m.group(1), m.group(2)
            nums = [int(n) for n in args.split(";") if n.isdigit()]
            if cmd == "H":
                cy = (nums[0] - 1) if nums else 0
                cx = (nums[1] - 1) if len(nums) > 1 else 0
            elif cmd == "J":
                grid = [[" "] * cols for _ in range(rows)]
                cy = cx = 0
            elif cmd == "K":
                for x in range(cx, cols):
                    grid[cy][x] = " "
            elif cmd == "A":
                cy = max(0, cy - (nums[0] if nums else 1))
            elif cmd == "B":
                cy = min(rows - 1, cy + (nums[0] if nums else 1))
            elif cmd == "C":
                cx = min(cols - 1, cx + (nums[0] if nums else 1))
            elif cmd == "D":
                cx = max(0, cx - (nums[0] if nums else 1))
            i += m.end()
            continue
        if ch == "\n":
            cy = min(rows - 1, cy + 1)
        elif ch == "\r":
            cx = 0
        elif ch == "\t":
            cx = min(cols - 1, (cx // 8 + 1) * 8)
        elif ch >= " ":
            if 0 <= cy < rows and 0 <= cx < cols:
                grid[cy][cx] = ch
            cx += 1
            if cx >= cols:
                cx, cy = 0, min(rows - 1, cy + 1)
        i += 1
    return "\n".join("".join(row).rstrip() for row in grid).rstrip("\n")


def drive(argv, script):
    """script: list of (wait_seconds, keys_to_send). Returns (frames, raw)."""
    pid, fd = pty.fork()
    if pid == 0:
        env = dict(os.environ, TERM="xterm-256color", LINES=str(ROWS), COLUMNS=str(COLS))
        os.execvpe(argv[0], argv, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    raw = b""
    frames = []

    def pump(seconds):
        nonlocal raw
        deadline = time.time() + seconds
        while time.time() < deadline:
            r, _, _ = select.select([fd], [], [], 0.2)
            if fd in r:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return False
                if not chunk:
                    return False
                raw += chunk
        return True

    for wait, keys in script:
        if not pump(wait):
            break
        frames.append(render(raw))
        if keys:
            os.write(fd, keys.encode())
    pump(1.0)
    frames.append(render(raw))
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    os.waitpid(pid, os.WNOHANG)
    os.close(fd)
    return frames, raw


if __name__ == "__main__":
    wukong = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.local/bin/wukong")
    steps = [(2.5, None)]
    for key in sys.argv[2:]:
        steps.append((1.2, {"TAB": "\t", "ESC": "\x1b", "ENTER": "\r"}.get(key, key)))
    frames, raw = drive([wukong], steps)
    for n, frame in enumerate(frames):
        print(f"\n{'=' * 30} frame {n} {'=' * 30}")
        print(frame)
    open("/tmp/tui_raw.bin", "wb").write(raw)
    print(f"\n[{len(raw)} bytes of terminal output captured]")
