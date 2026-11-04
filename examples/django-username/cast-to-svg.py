#!/usr/bin/env python3
"""Turn screencast.sh's asciinema recording into the README's animated SVG.

    python3 cast-to-svg.py demo.cast demo.svg

Only what screencast.sh prints is supported: plain lines, typed commands, and
three colours (dim comments, the green prompt, reset). The SVG replays the
recording with SMIL (lines appear, typed lines are revealed left to right,
the window scrolls) and loops. No dependencies beyond Python.
"""
import json
import re
import sys
from xml.sax.saxutils import escape

COLS, ROWS = 126, 30
CW, LH, PAD, TOP = 8.43, 19, 18, 44  # char width, line height, padding, title bar
HOLD = 5.0  # seconds the last screen stays before the loop restarts
HOME = re.compile(r"/home/[^/\s]+")  # the recording machine's user name -> /home/me

SGR = re.compile(r"\x1b\[([0-9;]*)m")
STYLE = {"2": "dim", "1;32": "prompt", "0": None}


def lines_of(cast):
    """[(start, typed_until, [(style, text)])] from the recording's events,
    with pauses cut to the recording's idle_time_limit, as players do."""
    header, *rest = cast.splitlines()
    limit = json.loads(header).get("idle_time_limit") or 1e9
    events, last, cut = [], 0.0, 0.0
    for t, kind, data in map(json.loads, rest):
        cut += max(0.0, t - last - limit)
        events.append((t - cut, kind, data))
        last = t
    out, cur, style, start, chars = [], [], None, None, 0
    for t, kind, data in events:
        if kind != "o":
            continue
        for part in re.split(r"(\x1b\[[0-9;]*m|\r?\n)", data):
            if not part or part == "\r":
                continue
            if part.endswith("\n"):
                out.append((start if start is not None else t, t if chars > 1 else start, cur))
                cur, start, chars = [], None, 0
                continue
            m = SGR.fullmatch(part)
            if m:
                style = STYLE.get(m.group(1))
                continue
            part = HOME.sub("/home/me", part.replace("\r", ""))
            if start is None:
                start = t
            chars += 1  # events, not characters: one event per typed character
            cur.append((style, part))
    return out, events[-1][0]


def render(lines, end):
    total = end + HOLD
    width = PAD * 2 + COLS * CW
    height = TOP + PAD * 2 + ROWS * LH
    k = lambda t: f"{min(t / total, 1):.5f}"
    body = []
    for i, (t0, t1, spans) in enumerate(lines):
        y = TOP + PAD + (i + 1) * LH - 5
        tspans = "".join(
            f'<tspan class="{s}">{escape(x)}</tspan>' if s else escape(x) for s, x in spans
        )
        text = sum(len(x) for _, x in spans)
        anim = (
            f'<animate attributeName="opacity" values="0;0;1;1" keyTimes="0;{k(t0)};{k(t0)};1" '
            f'dur="{total:.2f}s" repeatCount="indefinite" calcMode="discrete"/>'
        )
        clip = ""
        if t1 and t1 > t0:  # typed: reveal left to right over the typing time
            w = PAD + text * CW + 2
            clip = f' clip-path="url(#c{i})"'
            body.append(
                f'<clipPath id="c{i}"><rect x="0" y="{y - LH}" height="{LH + 6}" width="0">'
                f'<animate attributeName="width" values="{PAD + 2 * CW};{PAD + 2 * CW};{w:.0f};{w:.0f}" '
                f'keyTimes="0;{k(t0)};{k(t1)};1" dur="{total:.2f}s" repeatCount="indefinite"/></rect></clipPath>'
            )
        body.append(f'<text x="{PAD}" y="{y}" opacity="0"{clip} xml:space="preserve">{tspans}{anim}</text>')

    # Scroll: once more than ROWS lines are shown, move up a line per new line.
    shifts, times = ["0 0"], ["0"]
    for i, (t0, _, _) in enumerate(lines):
        if i >= ROWS:
            shifts.append(f"0 {-(i - ROWS + 1) * LH}")
            times.append(k(t0))
    shifts.append(shifts[-1])
    times.append("1")
    scroll = (
        f'<animateTransform attributeName="transform" type="translate" values="{";".join(shifts)}" '
        f'keyTimes="{";".join(times)}" dur="{total:.2f}s" repeatCount="indefinite" calcMode="discrete"/>'
    )
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0f}" height="{height}" viewBox="0 0 {width:.0f} {height}" role="img" aria-label="Three Claude Code agents fix a Django bug in parallel foil branches; the best fix is reviewed and committed">
<style>
text {{ font: 14px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; fill: #d4d4d8; }}
.dim {{ fill: #8b8b94; }} .prompt {{ fill: #4ade80; font-weight: 700; }}
</style>
<rect width="100%" height="100%" rx="10" fill="#18181b"/>
<circle cx="20" cy="20" r="6" fill="#ef4444"/><circle cx="40" cy="20" r="6" fill="#eab308"/><circle cx="60" cy="20" r="6" fill="#22c55e"/>
<text x="{width / 2:.0f}" y="25" text-anchor="middle" class="dim">foil: three agents, one Django bug (a recorded run; pauses shortened)</text>
<clipPath id="screen"><rect x="0" y="{TOP}" width="{width:.0f}" height="{height - TOP}"/></clipPath>
<g clip-path="url(#screen)"><g>{scroll}
{chr(10).join(body)}
</g></g>
</svg>
"""


if __name__ == "__main__":
    src, dst = sys.argv[1:3]
    lines, end = lines_of(open(src).read())
    open(dst, "w").write(render(lines, end))
