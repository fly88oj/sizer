#!/usr/bin/env python3
"""Controlled GTK4 client used as the target window by the GNOME harness.

Prints one JSON object per line on stdout:
  {"event": "ready", ...}      once the surface is mapped
  {"event": "size", ...}       on every surface layout change (independent oracle)
"""

import argparse
import json
import re
import sys

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk, GLib  # noqa: E402


def emit(payload):
    print(json.dumps(payload), flush=True)


def app_id_for(title):
    # GApplication ids must be lowercase alphanumerics separated by dots.
    slug = re.sub(r"[^a-z0-9]", "", title.lower())
    return f"dev.sizerlinux.spike.{slug}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--title", default="SIZER-SPIKE-CLIENT")
    parser.add_argument("--width", type=int, default=800)
    parser.add_argument("--height", type=int, default=600)
    parser.add_argument("--min-width", type=int, default=0)
    parser.add_argument("--min-height", type=int, default=0)
    parser.add_argument("--defer-map", action="store_true",
                        help="map the window only when stdin reads 'map' (tests that need "
                             "the focus a freshly mapped window gets)")
    args = parser.parse_args()

    app = Gtk.Application(application_id=app_id_for(args.title))

    def on_activate(application):
        window = Gtk.ApplicationWindow(application=application, title=args.title)
        window.set_default_size(args.width, args.height)
        if args.min_width > 0 or args.min_height > 0:
            window.set_size_request(args.min_width, args.min_height)

        area = Gtk.DrawingArea()
        area.set_hexpand(True)
        area.set_vexpand(True)

        def draw(_area, cr, width, height):
            cr.set_source_rgb(0.20, 0.45, 0.85)
            cr.paint()

        area.set_draw_func(draw)
        window.set_child(area)

        def on_layout(surf, width, height):
            emit({"event": "size", "title": args.title, "surface_w": width,
                  "surface_h": height, "widget_w": window.get_width(),
                  "widget_h": window.get_height()})

        def on_map(_window):
            # The GdkSurface only exists once the window is realized.
            surface = window.get_surface()
            surface.connect("layout", on_layout)
            emit({"event": "ready", "title": args.title,
                  "surface_w": surface.get_width(), "surface_h": surface.get_height()})

        window.connect("map", on_map)
        if not args.defer_map:
            window.present()
            return

        emit({"event": "waiting", "title": args.title})

        def on_stdin(_stream, _condition):
            line = sys.stdin.readline().strip()
            if line == "map":
                window.present()
            elif line == "quit":
                application.quit()
            return True

        GLib.io_add_watch(sys.stdin, GLib.PRIORITY_DEFAULT, GLib.IO_IN, on_stdin)

    app.connect("activate", on_activate)
    GLib.timeout_add_seconds(120, lambda: (app.quit(), False)[1])
    app.run(sys.argv[:1])


if __name__ == "__main__":
    main()
