#!/usr/bin/env python3
"""Run skywave-tui in a pseudo-terminal as a second device, without tmux.

    tui-pty.py DIR -- skywave-tui --data-dir D   # serve (run in background)
    tui-pty.py DIR keys 'text' '<Enter>' ...      # send keys
    tui-pty.py DIR screen                         # print the rendered screen

DIR holds keys.fifo (input) and screen.txt (rendered every 0.3 s). Named
keys: <Enter> <Esc> <Tab> <Up> <Down> <Left> <Right> <BS> <C-x> (any letter).
Needs `pyte` (pip install pyte, e.g. in a venv).
"""

import os
import pty
import select
import sys
import time

KEYS = {"<Enter>": "\r", "<Esc>": "\x1b", "<Tab>": "\t", "<BS>": "\x7f",
        "<Up>": "\x1b[A", "<Down>": "\x1b[B", "<Right>": "\x1b[C", "<Left>": "\x1b[D"}
COLS, ROWS = 120, 40


def encode(word):
    if word in KEYS:
        return KEYS[word]
    if word.startswith("<C-") and word.endswith(">") and len(word) == 5:
        return chr(ord(word[3].lower()) - 96)
    return word


def serve(d, argv):
    import pyte
    os.makedirs(d, exist_ok=True)
    fifo = os.path.join(d, "keys.fifo")
    if not os.path.exists(fifo):
        os.mkfifo(fifo)
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"], os.environ["LINES"] = str(COLS), str(ROWS)
        os.execvp(argv[0], argv)
    import fcntl
    import struct
    import termios
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    kfd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
    last = 0.0
    while True:
        r, _, _ = select.select([fd, kfd], [], [], 0.3)
        if fd in r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            stream.feed(data)
        if kfd in r:
            data = os.read(kfd, 65536)
            if data:
                os.write(fd, data)
            else:
                os.close(kfd)
                kfd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
        if time.time() - last > 0.3:
            with open(os.path.join(d, "screen.tmp"), "w") as f:
                f.write("\n".join(line.rstrip() for line in screen.display))
            os.replace(os.path.join(d, "screen.tmp"), os.path.join(d, "screen.txt"))
            last = time.time()
    print("tui exited")


def main():
    d, cmd, *rest = sys.argv[1:]
    if cmd == "--":
        serve(d, rest)
    elif cmd == "keys":
        with open(os.path.join(d, "keys.fifo"), "w") as f:
            for w in rest:
                f.write(encode(w))
                f.flush()
                time.sleep(0.05)
    elif cmd == "screen":
        time.sleep(0.4)
        print(open(os.path.join(d, "screen.txt")).read())
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
