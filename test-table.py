#!/usr/bin/env python3
"""test-table: голое окно WebKitGTK с test-table.html.

Обычное окно 1024x768, никакого нативного UI: все кнопки живут в самой
странице. Кнопки фуллскрина/закрытия страница показывает только при
наличии нативного моста (window.webkit.messageHandlers.host), который
мы здесь и регистрируем; в обычном браузере их нет. Тёмный фон вебвью
сразу (без белой вспышки), app-id окна: test-table. Все записи — во
временный каталог.
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

# Нативный мост для страницы: window.webkit.messageHandlers.host.
# Регистрируем ДО загрузки — страница при старте проверяет его наличие
# и только тогда показывает кнопки фуллскрина/закрытия.
ucm = web.get_user_content_manager()
ucm.register_script_message_handler("host")

web.load_uri(PAGE)


# Все кнопки и состояние видов живут в самой странице (test-table.html);
# отсюда только дёргаем её функции по горячим клавишам и событиям окна.
def eval_js(script: str):
    def _js_done(_wv, res, _ud):
        try:
            web.evaluate_javascript_finish(res)
        except GLib.Error:
            pass  # страница ещё не загрузилась — останется вид по умолчанию

    web.evaluate_javascript(script, -1, None, None, None, _js_done, None)


def toggle_fullscreen(*_):
    if win.is_fullscreen():
        win.unfullscreen()
    else:
        win.fullscreen()


def sync_fullscreen_icon():
    eval_js("setFullscreen(%s);" % ("true" if win.is_fullscreen() else "false"))


def on_fullscreen_changed(_window, _pspec):
    sync_fullscreen_icon()


# Клики по HTML-кнопкам приходят сюда строками: "fullscreen" / "close".
def on_host_message(_ucm, value):
    msg = value.to_string()
    if msg == "fullscreen":
        toggle_fullscreen()
    elif msg == "close":
        win.close()


ucm.connect("script-message-received::host", on_host_message)
win.connect("notify::fullscreened", on_fullscreen_changed)


# После загрузки страницы синхронизируем иконку фуллскрина с состоянием окна.
def on_load_changed(_wv, event):
    if event == WebKit.LoadEvent.FINISHED:
        sync_fullscreen_icon()


web.connect("load-changed", on_load_changed)


# Горячие клавиши: Ctrl+1..4 — квадрант, Ctrl+A — все виды, Ctrl+M — следующий
# режим, Ctrl+F — фуллскрин, Ctrl+Q / Ctrl+W — выход. Русская раскладка
# учитывается по физическим клавишам: A=ф, M=ь, F=а, Q=й, W=ц.
# CAPTURE-фаза — перехватываем до того, как клавишу съест WebView.
def on_key_pressed(_ctrl, keyval, _keycode, state):
    if not state & Gdk.ModifierType.CONTROL_MASK:
        return False
    if Gdk.KEY_1 <= keyval <= Gdk.KEY_4:
        eval_js(f"setView({keyval - Gdk.KEY_0});")
    elif keyval in (Gdk.KEY_a, Gdk.KEY_A, Gdk.KEY_Cyrillic_ef, Gdk.KEY_Cyrillic_EF):
        eval_js("setView(0);")
    elif keyval in (Gdk.KEY_m, Gdk.KEY_M, Gdk.KEY_Cyrillic_softsign, Gdk.KEY_Cyrillic_SOFTSIGN):
        eval_js("cycleView();")
    elif keyval in (Gdk.KEY_f, Gdk.KEY_F, Gdk.KEY_Cyrillic_a, Gdk.KEY_Cyrillic_A):
        toggle_fullscreen()
    elif keyval in (Gdk.KEY_q, Gdk.KEY_Q, Gdk.KEY_Cyrillic_shorti, Gdk.KEY_Cyrillic_SHORTI,
                    Gdk.KEY_w, Gdk.KEY_W, Gdk.KEY_Cyrillic_tse, Gdk.KEY_Cyrillic_TSE):
        win.close()
    else:
        return False
    return True


keys = Gtk.EventControllerKey()
keys.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
keys.connect("key-pressed", on_key_pressed)
win.add_controller(keys)

win.set_child(web)
win.set_default_size(1024, 768)
win.present()

loop = GLib.MainLoop()
win.connect("close-request", lambda *_: loop.quit())
try:
    loop.run()
except KeyboardInterrupt:
    pass
