#!/usr/bin/env python3
"""test-table: голое окно WebKitGTK с test-table.html.

Обычное окно 1024x768, никакого UI, тёмный фон вебвью сразу (без белой
вспышки), app-id окна: test-table. Все записи — во временный каталог.
"""
import os
import sys

APP_ID = "test-table"
HERE = os.path.dirname(os.path.abspath(__file__))
PAGE = "file://" + os.path.join(HERE, "test-table.html")

# Перенаправляем ВСЕ стандартные XDG-каталоги во временное место,
# чтобы процесс ничего не писал в домашний каталог.
# Должно стоять ДО первого использования GLib (импорта gi).
_runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
_base = os.path.join(_runtime, APP_ID)
os.environ.update({
    "XDG_DATA_HOME": os.path.join(_base, "data"),
    "XDG_CACHE_HOME": os.path.join(_base, "cache"),
    "XDG_CONFIG_HOME": os.path.join(_base, "config"),
    "XDG_STATE_HOME": os.path.join(_base, "state"),
})
# Ускорение старта WebKit (раскомментировать по одной, с замером):
# os.environ["WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS"] = "1"  # без изоляции!
# os.environ["WEBKIT_DISABLE_COMPOSITING_MODE"] = "1"           # без GPU-композитинга
# os.environ["WEBKIT_DISABLE_DMABUF_RENDERER"] = "1"            # без DMA-BUF

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("WebKit", "6.0")
from gi.repository import Gdk, GLib, Gtk, WebKit

GLib.set_prgname(APP_ID)  # app-id окна на Wayland

win = Gtk.Window(title="Test Table")
web = WebKit.WebView()

# Тёмный фон вебвью — действует ДО загрузки страницы, никакой белой вспышки.
bg = Gdk.RGBA()
bg.parse("#1e1e1e")
web.set_background_color(bg)

web.load_uri(PAGE)

# Контролы поверх контента в правом верхнем углу: фуллскрин и закрытие.
overlay = Gtk.Overlay()
overlay.set_child(web)

controls = Gtk.Box(spacing=6)
controls.set_halign(Gtk.Align.END)
controls.set_valign(Gtk.Align.START)
controls.set_margin_top(8)
controls.set_margin_end(8)
controls.add_css_class("overlay-controls")

# Иконки — inline-SVG. Растеризуем с запасом (64x64); Gtk.Image с
# pixel_size жёстко показывает их как 16x16 — даунскейл чёткий на любом DPI.
def svg_icon(svg: str) -> Gtk.Image:
    texture = Gdk.Texture.new_from_bytes(GLib.Bytes.new(svg.encode()))
    img = Gtk.Image.new_from_paintable(texture)
    img.set_pixel_size(16)
    return img


# Уголки по краям — «развернуть на весь экран».
FS_ENTER_SVG = '''<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 16 16">
<path d="M3 6.5 V3 H6.5 M9.5 3 H13 V6.5 M13 9.5 V13 H9.5 M6.5 13 H3 V9.5"
 stroke="#eee" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>'''
# Уголки вершиной внутрь (раскрытием к углам) — «свернуть обратно».
FS_EXIT_SVG = '''<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 16 16">
<path d="M6.5 3 V6.5 H3 M9.5 3 V6.5 H13 M6.5 13 V9.5 H3 M9.5 13 V9.5 H13"
 stroke="#eee" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>'''
CLOSE_SVG = '''<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 16 16">
<path d="M4 4 L12 12 M12 4 L4 12"
 stroke="#eee" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>'''

pic_fs_enter = svg_icon(FS_ENTER_SVG)
pic_fs_exit = svg_icon(FS_EXIT_SVG)

btn_fs = Gtk.Button(child=pic_fs_enter)
btn_fs.set_tooltip_text("Fullscreen")
btn_fs.set_size_request(28, 28)
btn_close = Gtk.Button(child=svg_icon(CLOSE_SVG))
btn_close.set_tooltip_text("Close")
btn_close.set_size_request(28, 28)
controls.append(btn_fs)
controls.append(btn_close)
overlay.add_overlay(controls)


def toggle_fullscreen(*_):
    if win.is_fullscreen():
        win.unfullscreen()
    else:
        win.fullscreen()


def on_fullscreen_changed(window, _pspec):
    if window.is_fullscreen():
        btn_fs.set_child(pic_fs_exit)
        btn_fs.set_tooltip_text("Exit fullscreen")
    else:
        btn_fs.set_child(pic_fs_enter)
        btn_fs.set_tooltip_text("Fullscreen")


btn_fs.connect("clicked", toggle_fullscreen)
btn_close.connect("clicked", lambda *_: win.close())
win.connect("notify::fullscreened", on_fullscreen_changed)

# Стиль контролов: полупрозрачные тёмные кнопки поверх страницы.
css = Gtk.CssProvider()
css.load_from_string("""
.overlay-controls button {
    background: transparent;
    color: #eeeeee;
    border: none;
    box-shadow: none;
    outline: none;
    border-radius: 6px;
    padding: 0;
    min-width: 28px;
    min-height: 28px;
    font-size: 16px;
    line-height: 1;
}
.overlay-controls button:hover { background: rgba(70, 70, 70, 0.85); }
""")
Gtk.StyleContext.add_provider_for_display(
    Gdk.Display.get_default(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
)

win.set_child(overlay)
win.set_default_size(1024, 768)
win.present()

loop = GLib.MainLoop()
win.connect("close-request", lambda *_: loop.quit())
try:
    loop.run()
except KeyboardInterrupt:
    pass
