#!/usr/bin/env python3
"""Cuts the screen recordings of a real agent run into footage/f0001.jpg..., 30 fps.

Sources (2560x1640 Retina recordings of a throwaway Atrium window, kept out of git):
  footage/raw/agent.mov  the whole run, from typing the request to the answer
  footage/raw/full.mov   the finished chat reopened in the full chat view
Each segment plays at its own speed; launch.html shows the speed and frames the
camera with the output times printed here.
"""
import json
import pathlib
import subprocess

here = pathlib.Path(__file__).resolve().parent
raw, out = here / "footage" / "raw", here / "footage"
FPS = 30
XFADE = .4

# (source, start, end, speed)
SEGMENTS = [
    ("agent", 3.6, 8.5, 2),      # typing the request into the start page, Ask mode
    ("agent", 8.5, 25.9, 5),     # chat view: steps shimmer, the agent's tab appears in the PiP
    ("agent", 25.9, 33.4, 1.6),  # in the agent's tab: Round trip -> One way, the date
    ("agent", 33.4, 49.3, 8),    # reading flight details
    ("agent", 49.3, 53.2, 1.25), # clicks Cheapest
    ("agent", 53.2, 58.6, 2),    # the answer arrives in the dock
    ("full", 0.3, 3.8, 1),       # the same chat in the full view, with the table
]

# Paint-overs. The recording pointer is covered with the strip of pixels just
# above it; the empty part of the translucent sidebar, where blurred windows from
# behind showed through, is filled flat; in the docked assistant the same
# show-through is flattened by turning those pixels into the plain background.
SIDEBAR = "drawbox=x=0:y=810:w=486:h=730:color=0x1b1b1d:t=fill"
DOCKED = {2, 3, 4, 5}


def clean(i, src):
    if src == "full":
        return f"{SIDEBAR},drawbox=x=960:y=1166:w=40:h=56:color=0x1c1c1e:t=fill"
    g = f"{SIDEBAR},split[m{i}][p{i}];[p{i}]crop=60:62:490:1148[q{i}];[m{i}][q{i}]overlay=490:1212"
    if i in DOCKED:
        # Show-through sits at luminance 21-35 or is dark and bluish; the bubble
        # behind the request is about 41 and text starts near 77.
        ghost = "gt(lt((r(X,Y)+g(X,Y)+b(X,Y))/3,37)+gt(b(X,Y)-r(X,Y),7)*lt(g(X,Y),72),0)"
        flat = f"format=gbrp,geq=r='if({ghost},25,r(X,Y))':g='if({ghost},25,g(X,Y))':b='if({ghost},26,b(X,Y))'"
        g += f",split[n{i}][d{i}];[d{i}]crop=770:1500:1790:70,{flat}[e{i}];[n{i}][e{i}]overlay=1790:70"
    return g


parts, labels, times, t = [], [], [], 0.0
for i, (src, a, b, speed) in enumerate(SEGMENTS):
    parts.append(f"[{0 if src == 'agent' else 1}:v]trim={a}:{b},setpts=(PTS-STARTPTS)/{speed},{clean(i, src)},fps={FPS},settb=1/{FPS}[s{i}]")
    dur = (b - a) / speed
    times.append({"start": round(t, 3), "speed": speed})
    t += dur
    labels.append(f"[s{i}]")
# The last segment is a different recording: cross-fade into it instead of cutting.
first = "".join(labels[:-1])
pre = t - (SEGMENTS[-1][2] - SEGMENTS[-1][1]) / SEGMENTS[-1][3]
graph = ";".join(parts) + f";{first}concat=n={len(labels) - 1}:v=1,fps={FPS},settb=1/{FPS}[pre];[pre]{labels[-1]}xfade=transition=fade:duration={XFADE}:offset={pre - XFADE:.3f}[v]"
times[-1]["start"] = round(pre - XFADE, 3)

for f in out.glob("f*.jpg"):
    f.unlink()
subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", raw / "agent.mov", "-i", raw / "full.mov",
                "-filter_complex", graph, "-map", "[v]", "-q:v", "2", out / "f%04d.jpg"], check=True)
count = len(list(out.glob("f*.jpg")))
(out / "footage.json").write_text(json.dumps({"fps": FPS, "frames": count, "segments": times}, indent=1))
print(json.dumps({"frames": count, "seconds": round(count / FPS, 2), "segments": times}))
