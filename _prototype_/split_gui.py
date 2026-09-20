#!/usr/bin/env python3
"""Waveform editor for adjusting split points of a radio recording, then
exporting the tracks with ffmpeg (lossless -c copy).

Starts from the neural auto-detection in split_radio_nn.py; you then fix the
cut points by hand: drag markers, type exact times, nudge, snap to the
quietest moment, and listen to each cut before exporting.

Requires ffmpeg, ffprobe and ffplay on PATH (all in the ffmpeg package) and
the packages in requirements.txt.

Usage:
    python split_gui.py [recording.mp3]

Mouse (main waveform):
    click              move the playhead
    drag a marker      move that split point
    double-click       add a split point
    right-click marker delete it
    wheel              zoom around the cursor      shift+wheel  pan
Overview bar (bottom of the waveform): click / drag to jump around.

Keys:
    space              play from playhead / stop
    left / right       nudge selected split 0.1s  (shift: 1s, ctrl: 0.01s)
    up / down          previous / next split       delete  remove split
    a                  add split at playhead       p       listen to the cut
"""
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, messagebox, ttk

import numpy as np
import soundfile as sf

import split_radio_nn as nn

SR = 16000
ENV_RATE = 500
BIN = SR // ENV_RATE
MIN_GAP = 0.05
MIN_SPAN = 0.5
STATE_FILE = Path(__file__).with_name("split_gui_state.json")

BG = "#202124"
BG_ALT = "#26282d"
WAVE = "#6fa8dc"
MARK = "#ff5252"
MARK_SEL = "#ffb300"
HEAD = "#4caf50"
TEXT = "#9aa0a6"
RULER_H = 22


def fmt_time(t):
    t = max(0.0, t)
    m = int(t // 60)
    return f"{m}:{t - m * 60:06.3f}"


def fmt_dur(t):
    t = int(round(max(0.0, t)))
    return f"{t // 60}:{t % 60:02d}"


def parse_time(text):
    secs = 0.0
    for part in text.strip().split(":"):
        secs = secs * 60 + float(part)
    return secs


def compute_envelope(wav_path):
    info = sf.info(str(wav_path))
    n_bins = (info.frames + BIN - 1) // BIN
    lo = np.empty(n_bins, np.float32)
    hi = np.empty(n_bins, np.float32)
    pos = 0
    for chunk in sf.blocks(str(wav_path), blocksize=BIN * 16384, dtype="float32"):
        if chunk.ndim > 1:
            chunk = chunk.mean(axis=1)
        pad = (-len(chunk)) % BIN
        if pad:
            chunk = np.pad(chunk, (0, pad), mode="edge")
        rows = chunk.reshape(-1, BIN)
        lo[pos:pos + len(rows)] = rows.min(axis=1)
        hi[pos:pos + len(rows)] = rows.max(axis=1)
        pos += len(rows)
    return lo[:pos], hi[:pos]


def quiet_point(wav_path, t, window):
    start = max(0, int((t - window) * SR))
    seg, _ = sf.read(str(wav_path), start=start, stop=int((t + window) * SR), dtype="float32")
    frame, hop = 1024, 256
    if len(seg) < frame * 2:
        return t
    cum = np.concatenate([[0.0], np.cumsum(seg.astype(np.float64) ** 2)])
    starts = np.arange(0, len(seg) - frame, hop)
    energy = cum[starts + frame] - cum[starts]
    return (start + starts[int(np.argmin(energy))] + frame // 2) / SR


def clamp(v, lo, hi):
    return max(lo, min(hi, v))


class App:
    def __init__(self, root):
        self.root = root
        root.title("Radio splitter")
        root.geometry("1280x780")

        self.q = queue.Queue()
        self.busy = False
        self.input = None
        self.tmpdir = None
        self.wav = None
        self.lo = self.hi = None
        self.gain_ref = 1.0
        self.duration = 0.0
        self.marks = []
        self.sel = None
        self.playhead = 0.0
        self.view_start = 0.0
        self.view_span = 60.0
        self.drag_idx = None
        self.play_proc = None
        self.play_t0 = 0.0
        self.play_from = 0.0
        self.has_ffplay = shutil.which("ffplay") is not None
        self._syncing = False
        try:
            self.state = json.loads(STATE_FILE.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self.state = {}

        self.build_ui()
        root.protocol("WM_DELETE_WINDOW", self.on_close)
        root.after(100, self.poll)

    # ---------- UI ----------
    def btn(self, parent, text, cmd, **kw):
        return ttk.Button(parent, text=text, command=cmd, takefocus=False, **kw)

    def build_ui(self):
        root = self.root
        top = ttk.Frame(root, padding=(8, 8, 8, 4))
        top.pack(fill="x")
        self.btn(top, "Open…", self.open_file).pack(side="left")
        ttk.Separator(top, orient="vertical").pack(side="left", fill="y", padx=10)
        self.btn(top, "Auto-detect", lambda: self.auto_detect(ask=True)).pack(side="left")
        ttk.Label(top, text="sensitivity").pack(side="left", padx=(10, 2))
        self.sens_var = tk.StringVar(value="1.5")
        ttk.Entry(top, width=5, textvariable=self.sens_var).pack(side="left")
        ttk.Label(top, text="min track (s)").pack(side="left", padx=(10, 2))
        self.minlen_var = tk.StringVar(value="120")
        ttk.Entry(top, width=5, textvariable=self.minlen_var).pack(side="left")

        self.btn(top, "Export tracks", self.export).pack(side="right")
        self.btn(top, "Browse…", self.browse_out).pack(side="right", padx=(4, 8))
        self.out_var = tk.StringVar(value=str(Path("tracks").resolve()))
        ttk.Entry(top, width=42, textvariable=self.out_var).pack(side="right")
        ttk.Label(top, text="Output folder").pack(side="right", padx=(0, 4))

        self.main = tk.Canvas(root, height=320, bg=BG, highlightthickness=0)
        self.main.pack(fill="both", expand=True, padx=8, pady=(4, 2))
        self.ov = tk.Canvas(root, height=58, bg=BG, highlightthickness=0)
        self.ov.pack(fill="x", padx=8, pady=(0, 4))

        bottom = ttk.Frame(root, padding=(8, 0, 8, 4))
        bottom.pack(fill="x")

        left = ttk.Frame(bottom)
        left.pack(side="left", fill="both", expand=True)
        self.tree = ttk.Treeview(left, columns=("n", "time", "before", "after"),
                                 show="headings", height=8, selectmode="browse")
        for col, text, w in (("n", "#", 40), ("time", "Split time", 110),
                             ("before", "Track before", 100), ("after", "Track after", 100)):
            self.tree.heading(col, text=text)
            self.tree.column(col, width=w, anchor="center")
        sb = ttk.Scrollbar(left, orient="vertical", command=self.tree.yview)
        self.tree.configure(yscrollcommand=sb.set)
        self.tree.pack(side="left", fill="both", expand=True)
        sb.pack(side="left", fill="y")
        self.tree.bind("<<TreeviewSelect>>", self.on_tree_select)
        self.tree.bind("<Double-1>", lambda e: self.zoom_to_marker())

        right = ttk.Frame(bottom, padding=(14, 0, 0, 0))
        right.pack(side="left", fill="y")

        row = ttk.Frame(right)
        row.pack(fill="x", pady=2)
        ttk.Label(row, text="Selected split").pack(side="left")
        self.time_var = tk.StringVar()
        e = ttk.Entry(row, width=11, textvariable=self.time_var)
        e.pack(side="left", padx=6)
        e.bind("<Return>", lambda ev: self.set_from_entry())
        self.btn(row, "Set", self.set_from_entry).pack(side="left")
        ttk.Label(row, text="(m:ss.mmm or seconds)", foreground="#777").pack(side="left", padx=6)

        row = ttk.Frame(right)
        row.pack(fill="x", pady=2)
        ttk.Label(row, text="Nudge").pack(side="left")
        for label, d in (("−1s", -1), ("−0.1", -0.1), ("−0.01", -0.01),
                         ("+0.01", 0.01), ("+0.1", 0.1), ("+1s", 1)):
            self.btn(row, label, lambda d=d: self.nudge(d), width=6).pack(side="left", padx=1)

        row = ttk.Frame(right)
        row.pack(fill="x", pady=2)
        self.btn(row, "Snap to quiet", self.snap_selected).pack(side="left")
        self.btn(row, "Add at playhead", self.add_at_playhead).pack(side="left", padx=4)
        self.btn(row, "Delete", self.delete_selected).pack(side="left")

        row = ttk.Frame(right)
        row.pack(fill="x", pady=2)
        self.btn(row, "▶ Play from playhead", self.play_from_playhead).pack(side="left")
        self.btn(row, "▶ Listen to cut (±3s)", self.listen_to_cut).pack(side="left", padx=4)
        self.btn(row, "■ Stop", self.stop).pack(side="left")

        row = ttk.Frame(right)
        row.pack(fill="x", pady=2)
        ttk.Label(row, text="Zoom").pack(side="left")
        self.btn(row, "−", lambda: self.zoom(1.6), width=3).pack(side="left", padx=1)
        self.btn(row, "+", lambda: self.zoom(1 / 1.6), width=3).pack(side="left", padx=1)
        self.btn(row, "Whole file", self.zoom_all).pack(side="left", padx=(6, 1))
        self.btn(row, "Around split (10s)", self.zoom_to_marker).pack(side="left", padx=1)

        bar = ttk.Frame(root, padding=(8, 2, 8, 8))
        bar.pack(fill="x")
        self.status = tk.StringVar(value="Open a recording to begin.")
        ttk.Label(bar, textvariable=self.status).pack(side="left")
        self.progress = ttk.Progressbar(bar, mode="indeterminate", length=120)
        self.progress.pack(side="right")

        m = self.main
        m.bind("<Configure>", lambda e: self.redraw())
        m.bind("<ButtonPress-1>", self.on_press)
        m.bind("<B1-Motion>", self.on_drag)
        m.bind("<ButtonRelease-1>", self.on_release)
        m.bind("<Double-Button-1>", self.on_double)
        m.bind("<Button-3>", self.on_right)
        m.bind("<MouseWheel>", self.on_wheel)
        self.ov.bind("<Configure>", lambda e: self.draw_overview())
        self.ov.bind("<ButtonPress-1>", self.on_ov)
        self.ov.bind("<B1-Motion>", self.on_ov)

        root.bind_all("<KeyPress>", self.on_key)

    # ---------- background work ----------
    def poll(self):
        try:
            while True:
                kind, payload = self.q.get_nowait()
                if kind == "status":
                    self.status.set(payload)
                elif kind == "done":
                    callback, result = payload
                    self.set_busy(False)
                    callback(result)
                elif kind == "error":
                    self.set_busy(False)
                    self.show_error(payload)
        except queue.Empty:
            pass
        self.root.after(100, self.poll)

    def set_busy(self, busy, msg=None):
        self.busy = busy
        if busy:
            self.progress.start(12)
            if msg:
                self.status.set(msg)
        else:
            self.progress.stop()

    def run_bg(self, fn, args, callback, msg):
        if self.busy:
            return
        self.set_busy(True, msg)

        def target():
            try:
                result = fn(*args)
            except Exception as exc:
                traceback.print_exc()
                self.q.put(("error", exc))
                return
            self.q.put(("done", (callback, result)))

        threading.Thread(target=target, daemon=True).start()

    def show_error(self, exc):
        text = str(exc)
        if isinstance(exc, subprocess.CalledProcessError) and exc.stderr:
            text += "\n\n" + exc.stderr[-600:]
        self.status.set("Error: " + str(exc)[:120])
        messagebox.showerror("Error", text)

    # ---------- loading / detection ----------
    def open_file(self, path=None):
        if self.busy:
            return
        if not path:
            path = filedialog.askopenfilename(
                title="Open recording",
                initialdir=str(Path.home() / "Music"),
                filetypes=[("Audio", "*.mp3 *.wav *.m4a *.aac *.flac *.ogg *.mp4"), ("All files", "*.*")])
        if not path:
            return
        for tool in ("ffmpeg", "ffprobe"):
            if shutil.which(tool) is None:
                messagebox.showerror("ffmpeg missing", f"{tool} not found on PATH.")
                return
        self.stop()
        self.pending_input = Path(path).resolve()
        self.run_bg(self._load_worker, (self.pending_input,), self._loaded, "Decoding audio…")

    @staticmethod
    def _load_worker(path):
        duration = nn.probe_duration(path)
        tmp = tempfile.mkdtemp(prefix="splitgui_")
        wav = Path(tmp) / "audio.wav"
        nn.decode_to_wav(path, wav)
        lo, hi = compute_envelope(wav)
        return tmp, wav, lo, hi, duration

    def _loaded(self, result):
        tmp, wav, lo, hi, duration = result
        self.cleanup_tmp()
        self.tmpdir, self.wav, self.lo, self.hi, self.duration = tmp, wav, lo, hi, duration
        self.input = self.pending_input
        peak = np.maximum(np.abs(lo), np.abs(hi))
        self.gain_ref = max(float(np.percentile(peak, 99.9)), 1e-3)
        self.marks, self.sel, self.playhead = [], None, 0.0
        self.view_span = min(60.0, duration)
        self.view_start = 0.0
        self.root.title(f"Radio splitter — {self.input.name}")

        saved = self.state.get(str(self.input))
        if saved:
            self.marks = sorted(clamp(m, MIN_GAP, duration - MIN_GAP) for m in saved["marks"])
            if saved.get("out_dir"):
                self.out_var.set(saved["out_dir"])
            self.status.set(f"Loaded {len(self.marks)} saved split points. "
                            "Press Auto-detect to start over.")
            self.marks_changed(select=0 if self.marks else None, center=True)
        else:
            self.marks_changed()
            self.auto_detect(ask=False)

    def auto_detect(self, ask=True):
        if not self.input or self.busy:
            return
        try:
            sens = float(self.sens_var.get())
            minlen = float(self.minlen_var.get())
        except ValueError:
            messagebox.showerror("Auto-detect", "Sensitivity and min track length must be numbers.")
            return
        if ask and self.marks and not messagebox.askyesno(
                "Auto-detect", "Replace the current split points with auto-detected ones?"):
            return
        cache = Path(self.out_var.get()) / ".vggish_cache.npz"
        self.run_bg(self._detect_worker, (sens, minlen, cache), self._detected,
                    "Detecting track changes (first run computes the neural embeddings)…")

    def _detect_worker(self, sens, minlen, cache):
        embeddings, hop = nn.load_or_compute_embeddings(self.input, self.wav, cache)
        score = nn.consensus_novelty(embeddings, hop, [8.0, 15.0, 25.0])
        peaks = nn.pick_peaks(score, hop, minlen, sens)
        return sorted(quiet_point(self.wav, p * hop, 3.0) for p in peaks)

    def _detected(self, times):
        self.marks = times
        self.status.set(f"Auto-detected {len(times)} split points -> {len(times) + 1} tracks.")
        self.marks_changed(select=0 if times else None, center=True)

    # ---------- marker editing ----------
    def marks_changed(self, select="keep", center=False):
        if select != "keep":
            self.sel = select
        if self.sel is not None and self.sel >= len(self.marks):
            self.sel = len(self.marks) - 1 if self.marks else None
        self.refresh_tree()
        self.update_entry()
        if center and self.sel is not None:
            self.center_on(self.marks[self.sel], 30.0)
        elif center:
            self.clamp_view()
        self.redraw()
        self.save_state()

    def save_state(self):
        if not self.input:
            return
        self.state[str(self.input)] = {"marks": [round(m, 3) for m in self.marks],
                                       "out_dir": self.out_var.get()}
        try:
            STATE_FILE.write_text(json.dumps(self.state, indent=1), encoding="utf-8")
        except OSError:
            pass

    def refresh_tree(self):
        self._syncing = True
        self.tree.delete(*self.tree.get_children())
        bounds = [0.0] + self.marks + [self.duration]
        for i, m in enumerate(self.marks):
            self.tree.insert("", "end", iid=str(i), values=(
                i + 1, fmt_time(m), fmt_dur(bounds[i + 1] - bounds[i]),
                fmt_dur(bounds[i + 2] - bounds[i + 1])))
        if self.sel is not None:
            self.tree.selection_set(str(self.sel))
            self.tree.see(str(self.sel))
        self._syncing = False
        self.status.set(f"{len(self.marks)} split points -> {len(self.marks) + 1} tracks")

    def update_entry(self):
        self.time_var.set(fmt_time(self.marks[self.sel]) if self.sel is not None else "")

    def on_tree_select(self, _event):
        if self._syncing:
            return
        sel = self.tree.selection()
        if not sel:
            return
        self.sel = int(sel[0])
        self.update_entry()
        m = self.marks[self.sel]
        if not (self.view_start <= m <= self.view_start + self.view_span):
            self.center_on(m)
        self.redraw()

    def neighbor_limits(self, i):
        lo = self.marks[i - 1] + MIN_GAP if i > 0 else MIN_GAP
        hi = self.marks[i + 1] - MIN_GAP if i < len(self.marks) - 1 else self.duration - MIN_GAP
        return lo, hi

    def move_marker(self, i, t):
        lo, hi = self.neighbor_limits(i)
        self.marks[i] = clamp(t, lo, hi)

    def nudge(self, delta):
        if self.sel is None:
            self.status.set("Select a split point first.")
            return
        self.move_marker(self.sel, self.marks[self.sel] + delta)
        self.marks_changed()

    def set_from_entry(self):
        if self.sel is None:
            return
        try:
            t = parse_time(self.time_var.get())
        except ValueError:
            self.status.set("Could not parse time. Use m:ss.mmm or seconds.")
            return
        self.move_marker(self.sel, t)
        self.marks_changed()

    def snap_selected(self):
        if self.sel is None or not self.wav:
            return
        self.move_marker(self.sel, quiet_point(self.wav, self.marks[self.sel], 1.5))
        self.marks_changed()

    def add_marker(self, t):
        if not self.input:
            return
        t = clamp(t, MIN_GAP, self.duration - MIN_GAP)
        for i, m in enumerate(self.marks):
            if abs(m - t) < 0.2:
                self.marks_changed(select=i)
                return
        self.marks.append(t)
        self.marks.sort()
        self.marks_changed(select=self.marks.index(t))

    def add_at_playhead(self):
        self.add_marker(self.playhead)

    def delete_selected(self):
        if self.sel is None:
            return
        del self.marks[self.sel]
        self.marks_changed(select=min(self.sel, len(self.marks) - 1) if self.marks else None)

    def select_relative(self, step):
        if not self.marks:
            return
        i = 0 if self.sel is None else clamp(self.sel + step, 0, len(self.marks) - 1)
        self.marks_changed(select=i)
        self.center_on(self.marks[i])
        self.redraw()

    # ---------- view ----------
    def clamp_view(self):
        self.view_span = clamp(self.view_span, min(MIN_SPAN, self.duration), max(self.duration, MIN_SPAN))
        self.view_start = clamp(self.view_start, 0.0, max(0.0, self.duration - self.view_span))

    def center_on(self, t, span=None):
        if span:
            self.view_span = span
        self.view_start = t - self.view_span / 2
        self.clamp_view()

    def zoom(self, factor, anchor=None):
        if not self.input:
            return
        if anchor is None:
            anchor = self.view_start + self.view_span / 2
        frac = (anchor - self.view_start) / self.view_span
        self.view_span *= factor
        self.clamp_view()
        self.view_start = anchor - frac * self.view_span
        self.clamp_view()
        self.redraw()

    def zoom_all(self):
        self.view_start, self.view_span = 0.0, self.duration
        self.clamp_view()
        self.redraw()

    def zoom_to_marker(self):
        if self.sel is None:
            return
        self.center_on(self.marks[self.sel], 10.0)
        self.redraw()

    def x_of(self, t):
        return (t - self.view_start) * self.main.winfo_width() / self.view_span

    def t_of(self, x):
        return self.view_start + x * self.view_span / max(1, self.main.winfo_width())

    # ---------- drawing ----------
    def redraw(self):
        self.draw_main()
        self.draw_overview()

    def envelope_columns(self, t0, t1, width):
        edges = np.linspace(t0 * ENV_RATE, t1 * ENV_RATE, width + 1)
        idx = np.clip(edges.astype(np.int64), 0, len(self.lo) - 1)
        base = int(idx[0])
        end = min(len(self.lo), int(idx[-1]) + 1)
        starts = idx[:-1] - base
        lo = np.minimum.reduceat(self.lo[base:end], starts)
        hi = np.maximum.reduceat(self.hi[base:end], starts)
        return lo, hi

    def draw_main(self):
        c = self.main
        c.delete("all")
        W, H = c.winfo_width(), c.winfo_height()
        if self.lo is None or W < 20 or H < 60:
            c.create_text(W / 2, H / 2, text="Open a recording to begin", fill=TEXT)
            return
        t0, t1 = self.view_start, self.view_start + self.view_span
        area_h = H - RULER_H
        mid = RULER_H + area_h / 2
        half = area_h / 2 - 2

        bounds = [0.0] + self.marks + [self.duration]
        for k in range(len(bounds) - 1):
            a, b = bounds[k], bounds[k + 1]
            if b < t0 or a > t1:
                continue
            x0, x1 = max(0, self.x_of(a)), min(W, self.x_of(b))
            c.create_rectangle(x0, RULER_H, x1, H, fill=BG_ALT if k % 2 else BG, outline="")
            if x1 - x0 > 90:
                c.create_text(x0 + 6, H - 4, anchor="sw", fill=TEXT,
                              text=f"Track {k + 1}  ({fmt_dur(b - a)})")

        lo, hi = self.envelope_columns(t0, t1, W)
        g = half / self.gain_ref
        xs = np.arange(W, dtype=np.float64)
        top = mid - np.minimum(hi * g, half)
        bot = mid - np.maximum(lo * g, -half)
        coords = np.concatenate([np.column_stack([xs, top]).ravel(),
                                 np.column_stack([xs[::-1], bot[::-1]]).ravel()]).tolist()
        c.create_polygon(coords, fill=WAVE, outline="")

        self.draw_ruler(c, W, t0, t1)

        for i, m in enumerate(self.marks):
            if not (t0 <= m <= t1):
                continue
            x = self.x_of(m)
            selected = i == self.sel
            col = MARK_SEL if selected else MARK
            c.create_line(x, RULER_H, x, H, fill=col, width=3 if selected else 2)
            c.create_rectangle(x - 11, RULER_H, x + 11, RULER_H + 16, fill=col, outline="")
            c.create_text(x, RULER_H + 8, text=str(i + 1), fill="black")

        self.head_item = c.create_line(0, RULER_H, 0, H, fill=HEAD, width=2)
        self.update_playhead()

    def draw_ruler(self, c, W, t0, t1):
        px_per_s = W / self.view_span
        steps = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800]
        step = next((s for s in steps if s * px_per_s >= 90), steps[-1])
        t = np.ceil(t0 / step) * step
        while t <= t1:
            x = self.x_of(t)
            c.create_line(x, RULER_H - 6, x, RULER_H, fill=TEXT)
            label = fmt_time(t) if step < 1 else fmt_dur(t)
            c.create_text(x + 3, 2, anchor="nw", text=label, fill=TEXT)
            t += step

    def draw_overview(self):
        c = self.ov
        c.delete("all")
        W, H = c.winfo_width(), c.winfo_height()
        if self.lo is None or W < 20:
            return
        lo, hi = self.envelope_columns(0.0, self.duration, W)
        mid, half = H / 2, H / 2 - 3
        g = half / self.gain_ref
        xs = np.arange(W, dtype=np.float64)
        coords = np.concatenate([
            np.column_stack([xs, mid - np.minimum(hi * g, half)]).ravel(),
            np.column_stack([xs[::-1], (mid - np.maximum(lo * g, -half))[::-1]]).ravel()]).tolist()
        c.create_polygon(coords, fill="#4a6f96", outline="")
        for i, m in enumerate(self.marks):
            x = m / self.duration * W
            c.create_line(x, 0, x, H, fill=MARK_SEL if i == self.sel else MARK, width=2)
        x0 = self.view_start / self.duration * W
        x1 = (self.view_start + self.view_span) / self.duration * W
        c.create_rectangle(x0, 1, max(x1, x0 + 3), H - 1, outline="white", width=2)
        self.ov_head = c.create_line(0, 0, 0, H, fill=HEAD, width=2)
        self.update_playhead()

    def update_playhead(self):
        if self.lo is None:
            return
        W = self.main.winfo_width()
        x = self.x_of(self.playhead)
        try:
            self.main.coords(self.head_item, x, RULER_H, x, self.main.winfo_height())
            ox = self.playhead / self.duration * self.ov.winfo_width()
            self.ov.coords(self.ov_head, ox, 0, ox, self.ov.winfo_height())
        except (AttributeError, tk.TclError):
            pass

    # ---------- mouse / keys ----------
    def hit_marker(self, x, tol=7):
        best, best_d = None, tol + 1
        for i, m in enumerate(self.marks):
            d = abs(self.x_of(m) - x)
            if d < best_d:
                best, best_d = i, d
        return best

    def on_press(self, e):
        if not self.input:
            return
        i = self.hit_marker(e.x)
        if i is not None:
            self.drag_idx = i
            self.marks_changed(select=i)
        else:
            self.playhead = clamp(self.t_of(e.x), 0.0, self.duration)
            self.update_playhead()

    def on_drag(self, e):
        if self.drag_idx is None:
            return
        self.move_marker(self.drag_idx, self.t_of(e.x))
        self.update_entry()
        self.redraw()

    def on_release(self, _e):
        if self.drag_idx is not None:
            self.drag_idx = None
            self.marks_changed()

    def on_double(self, e):
        if self.input and self.hit_marker(e.x) is None:
            self.add_marker(self.t_of(e.x))

    def on_right(self, e):
        i = self.hit_marker(e.x, tol=10)
        if i is not None:
            self.sel = i
            self.delete_selected()

    def on_wheel(self, e):
        if not self.input:
            return
        if e.state & 0x1:
            self.view_start += -e.delta / 120 * self.view_span * 0.15
            self.clamp_view()
            self.redraw()
        else:
            self.zoom(0.8 if e.delta > 0 else 1.25, anchor=self.t_of(e.x))

    def on_ov(self, e):
        if not self.input:
            return
        self.center_on(e.x / max(1, self.ov.winfo_width()) * self.duration)
        self.redraw()

    def on_key(self, e):
        w = self.root.focus_get()
        if isinstance(w, (tk.Entry, ttk.Entry)):
            return
        key = e.keysym
        step = 1.0 if e.state & 0x1 else 0.01 if e.state & 0x4 else 0.1
        if key == "space":
            self.stop() if self.play_proc else self.play_from_playhead()
        elif key == "Left":
            self.nudge(-step)
        elif key == "Right":
            self.nudge(step)
        elif key in ("Up", "Down") and not isinstance(w, ttk.Treeview):
            self.select_relative(-1 if key == "Up" else 1)
        elif key == "Delete":
            self.delete_selected()
        elif key == "a":
            self.add_at_playhead()
        elif key == "p":
            self.listen_to_cut()

    # ---------- playback (ffplay) ----------
    def play(self, start, duration=None):
        self.stop()
        if not self.input:
            return
        if not self.has_ffplay:
            self.status.set("ffplay not found on PATH; playback unavailable.")
            return
        cmd = ["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet", "-ss", f"{start:.3f}"]
        if duration:
            cmd += ["-t", f"{duration:.3f}"]
        cmd.append(str(self.input))
        flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
        self.play_proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                          stderr=subprocess.DEVNULL, creationflags=flags)
        self.play_t0, self.play_from = time.monotonic(), start
        self.tick_play()

    def tick_play(self):
        if not self.play_proc:
            return
        if self.play_proc.poll() is not None:
            self.play_proc = None
            return
        self.playhead = clamp(self.play_from + max(0.0, time.monotonic() - self.play_t0 - 0.25),
                              0.0, self.duration)
        if not (self.view_start <= self.playhead <= self.view_start + self.view_span):
            self.center_on(self.playhead)
            self.redraw()
        self.update_playhead()
        self.root.after(50, self.tick_play)

    def stop(self):
        if self.play_proc:
            try:
                self.play_proc.terminate()
            except OSError:
                pass
            self.play_proc = None

    def play_from_playhead(self):
        self.play(self.playhead)

    def listen_to_cut(self):
        if self.sel is None:
            self.status.set("Select a split point first.")
            return
        start = max(0.0, self.marks[self.sel] - 3.0)
        self.play(start, 6.0)

    # ---------- export ----------
    def browse_out(self):
        d = filedialog.askdirectory(title="Output folder", initialdir=self.out_var.get() or None)
        if d:
            self.out_var.set(str(Path(d)))
            self.save_state()

    def export(self):
        if not self.input or self.busy:
            return
        out = Path(self.out_var.get())
        existing = list(out.glob("track_*.mp3")) if out.exists() else []
        if existing and not messagebox.askyesno(
                "Export", f"{out} already contains {len(existing)} files named track_*.mp3.\n"
                          "Files with the same names are overwritten and any extra old ones stay.\n\nContinue?"):
            return
        self.save_state()
        self.run_bg(self._export_worker, (list(self.marks), out), self._exported, "Exporting…")

    def _export_worker(self, marks, out):
        nn.extract_segments(
            self.input, marks, self.duration, out, "track_",
            on_progress=lambda i, n, p: self.q.put(("status", f"Writing {p.name} ({i}/{n})")))
        return len(marks) + 1, out

    def _exported(self, result):
        n, out = result
        self.status.set(f"Exported {n} tracks to {out}")
        messagebox.showinfo("Export finished", f"{n} tracks written to\n{out}")

    # ---------- shutdown ----------
    def cleanup_tmp(self):
        if self.tmpdir:
            shutil.rmtree(self.tmpdir, ignore_errors=True)
            self.tmpdir = None

    def on_close(self):
        self.stop()
        self.cleanup_tmp()
        self.root.destroy()


def main():
    if os.name == "nt":
        try:
            import ctypes
            ctypes.windll.shcore.SetProcessDpiAwareness(1)
        except Exception:
            pass
    root = tk.Tk()
    app = App(root)
    if len(sys.argv) > 1:
        root.after(200, lambda: app.open_file(sys.argv[1]))
    root.mainloop()


if __name__ == "__main__":
    main()
