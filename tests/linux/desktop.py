#!/usr/bin/env python3
"""Account-free native Tauri integration tests in an isolated Linux desktop.

Runs actual IPC and desktop D-Bus services. Synthetic game processes test
ownership/lifecycle only; they do not establish Proton/game compatibility.
"""
import argparse
import base64
import copy
import http.client
import json
import mmap
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time


def fixture_game(path):
    import ctypes
    held = open(path, "rb")
    mapped = mmap.mmap(held.fileno(), 0, access=mmap.ACCESS_READ)
    ctypes.CDLL(None).prctl(15, b"RecRoom.exe", 0, 0, 0)
    print("READY", flush=True)
    signal.pause()


if len(sys.argv) > 1 and sys.argv[1] == "--fixture-game":
    fixture_game(sys.argv[2])
    sys.exit(0)

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop, threads_init
from gi.repository import GLib


class Watcher(dbus.service.Object):
    """Minimal SNI host: validates native registration and real menu events."""
    def __init__(self, bus):
        self.items = []
        self.name = dbus.service.BusName("org.kde.StatusNotifierWatcher", bus)
        super().__init__(bus, "/StatusNotifierWatcher")

    @dbus.service.method("org.kde.StatusNotifierWatcher", in_signature="s", sender_keyword="sender")
    def RegisterStatusNotifierItem(self, service, sender=None):
        item = str(sender) + str(service) if str(service).startswith("/") else str(service) + "/StatusNotifierItem"
        self.items.append(item)
        self.StatusNotifierItemRegistered(item)

    @dbus.service.method("org.kde.StatusNotifierWatcher", in_signature="s")
    def RegisterStatusNotifierHost(self, host):
        pass

    @dbus.service.signal("org.kde.StatusNotifierWatcher", signature="s")
    def StatusNotifierItemRegistered(self, item):
        pass

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="ss", out_signature="v")
    def Get(self, interface, name):
        return self.GetAll(interface)[name]

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        return {"RegisteredStatusNotifierItems": dbus.Array(self.items, signature="s"),
                "IsStatusNotifierHostRegistered": dbus.Boolean(True), "ProtocolVersion": dbus.Int32(0)}


class Driver:
    def __init__(self, application, output, arguments=None):
        self.log = open(output / "webdriver.log", "a")
        command = ["/tools/bin/tauri-driver"]
        if os.environ.get("RECEOS_NATIVE_DRIVER"):
            command += ["--native-driver", os.environ["RECEOS_NATIVE_DRIVER"]]
        environment = dict(os.environ)
        if environment.get("RECEOS_APPLICATION_DISPLAY"):
            environment["DISPLAY"] = environment["RECEOS_APPLICATION_DISPLAY"]
        self.process = subprocess.Popen(command, stdout=self.log, stderr=self.log, start_new_session=True, env=environment)
        self.session = None
        self.connection = http.client.HTTPConnection("127.0.0.1", 4444, timeout=45)
        try:
            wait(lambda: self.request("GET", "/status"), "WebDriver startup")
            reply = self.request("POST", "/session", {"capabilities": {"alwaysMatch": {
                "browserName": "wry", "tauri:options": {"application": str(application), "args": arguments or []}}}})
            self.session = reply["sessionId"]
            self.request("POST", self.route("timeouts"), {"script": 35000})
            # The bridge/platform class precede the async config load. Wait for
            # branding to match that config before sending real user clicks.
            wait(lambda: self.js("return !!window.radium && document.body.classList.contains('platform-linux') && !!config.network && document.body.classList.contains('network-' + config.network)"), "frontend configuration and IPC")
        except Exception:
            self.close()
            raise

    def route(self, route):
        return f"/session/{self.session}/{route}"

    def request(self, method, route, body=None):
        data = None if body is None else json.dumps(body).encode()
        try:
            # Keep the proxy connection alive. urllib forces Connection: close,
            # which can race tauri-driver's pooled WebKitGTK connection.
            self.connection.request(method, route, body=data, headers={"Content-Type": "application/json"})
            response = self.connection.getresponse()
            result = json.loads(response.read())
            assert response.status < 400, result
        except (OSError, http.client.HTTPException):
            self.connection.close()
            raise
        value = result.get("value")
        if isinstance(value, dict) and "error" in value:
            raise AssertionError(value)
        return value

    def js(self, script):
        return self.request("POST", self.route("execute/sync"), {"script": script, "args": []})

    def async_js(self, script):
        body = "const done=arguments[arguments.length-1];(async()=>{" + script + "})().then(value=>done({ok:true,value}),error=>done({ok:false,error:String(error)}));"
        result = self.request("POST", self.route("execute/async"), {"script": body, "args": []})
        assert result["ok"], result
        return result.get("value")

    def invoke(self, name, args=None):
        return self.async_js("return await window.__TAURI__.core.invoke(" + json.dumps(name) + "," + json.dumps(args or {}) + ");")

    def click(self, css):
        element = self.request("POST", self.route("element"), {"using": "css selector", "value": css})
        route = self.route("element/" + element["element-6066-11e4-a52e-4f735466cecf"])
        wait(lambda: self.request("GET", route + "/displayed"), "Visible control " + css)
        self.request("POST", route + "/click", {})

    def screenshot(self, path):
        path.write_bytes(base64.b64decode(self.request("GET", self.route("screenshot"))))

    def close(self, preserve_children=False):
        native = []
        if preserve_children:
            # A real Proton game can outlive the launcher. Killing this test
            # driver's entire process group would also kill its runtime wrapper.
            children = {}
            for proc in Path('/proc').iterdir():
                try:
                    if not proc.name.isdecimal() or proc.stat().st_uid != os.getuid():
                        continue
                    parent = int(next(line.split()[1] for line in (proc / 'status').read_text().splitlines() if line.startswith('PPid:')))
                    children[int(proc.name)] = (parent, (proc / 'exe').readlink().name)
                except (OSError, StopIteration, ValueError):
                    pass
            descendants = {self.process.pid}
            while True:
                discovered = {pid for pid, (parent, _) in children.items() if parent in descendants}
                if discovered <= descendants:
                    break
                descendants |= discovered
            for pid in descendants - {self.process.pid}:
                # Delete-session can detach without terminating the native
                # application, especially through AppImage's wrapper. Stop
                # only our UI/driver processes, preserving Proton supervisors.
                if children[pid][1] in ('WebKitWebDriver', 'rec-eos-launcher'):
                    try:
                        native.append(os.pidfd_open(pid))
                    except ProcessLookupError:
                        pass
        if self.session:
            try:
                self.request("DELETE", f"/session/{self.session}")
            except Exception:
                pass
        try:
            if preserve_children:
                self.process.terminate()
            else:
                os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
        for fd in native:
            try:
                signal.pidfd_send_signal(fd, signal.SIGTERM)
            except ProcessLookupError:
                pass
            finally:
                os.close(fd)
        self.connection.close()
        self.log.close()


def wait(check, description, timeout=25):
    deadline = time.monotonic() + timeout
    error = None
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except Exception as exc:
            error = exc
        time.sleep(0.2)
    raise AssertionError(f"Timed out: {description}; last error: {error}")


def menu_action(bus, watcher, label):
    last = None
    for item in reversed(watcher.items):
        try:
            name, path = item.split("/", 1)
            proxy = bus.get_object(name, "/" + path)
            menu_path = proxy.Get("org.kde.StatusNotifierItem", "Menu", dbus_interface="org.freedesktop.DBus.Properties")
            menu = bus.get_object(name, str(menu_path))
            _, layout = menu.GetLayout(0, -1, [], dbus_interface="com.canonical.dbusmenu")

            def find(node):
                if str(node[1].get("label", "")).replace("_", "") == label:
                    return int(node[0])
                for child in node[2]:
                    found = find(child)
                    if found is not None:
                        return found
                return None
            found = find(layout)
            if found is None:
                raise AssertionError(f"Native tray label not found: {label}")
            menu.Event(found, "clicked", dbus.Int32(0, variant_level=1), dbus.UInt32(0), dbus_interface="com.canonical.dbusmenu")
            return True
        except Exception as error:
            last = error
    raise AssertionError(f"Cannot activate native tray action {label}: {last}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("application", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    home = Path(tempfile.mkdtemp(prefix="receos-desktop-"))
    for key, subdir in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache")]:
        os.environ[key] = str(home / subdir)
    os.environ["XDG_CURRENT_DESKTOP"] = "GNOME"
    applications = Path(os.environ["XDG_DATA_HOME"]) / "applications"
    applications.mkdir(parents=True)
    Path(os.environ["XDG_CONFIG_HOME"]).mkdir()
    opened_url = home / "opened-url.txt"
    handler = home / "capture-url"
    handler.write_text('#!/bin/sh\nprintf \'%s\\n\' "$1" > ' + str(opened_url) + '\n')
    handler.chmod(0o755)
    (applications / "receos-url-capture.desktop").write_text(
        '[Desktop Entry]\nType=Application\nName=Validation browser\nExec=' + str(handler) + ' %u\nMimeType=x-scheme-handler/https;\nNoDisplay=true\n')
    subprocess.run(["xdg-mime", "default", "receos-url-capture.desktop", "x-scheme-handler/https"], check=True)
    threads_init()
    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    watcher = Watcher(bus)
    loop = GLib.MainLoop()
    threading.Thread(target=loop.run, daemon=True).start()
    notifications_log = open(args.output / "notifications.log", "w")
    monitor = subprocess.Popen(["stdbuf", "-oL", "dbus-monitor", "--session", "type='method_call',interface='org.freedesktop.Notifications',member='Notify'"], stdout=notifications_log, stderr=subprocess.DEVNULL)
    notification_env = dict(os.environ, XDG_SESSION_TYPE="x11")
    notification_env.pop("WAYLAND_DISPLAY", None)
    dunst = subprocess.Popen(["dunst"], env=notification_env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    driver = None
    fixtures = []
    passed = []
    backend = None

    def record(name):
        passed.append(name)
        print("PASS: " + name, flush=True)
        (args.output / "results.json").write_text(json.dumps({"session": os.environ.get("GDK_BACKEND", "x11"),
            "applicationBackend": backend, "passed": passed, "home": str(home)}, indent=2))

    try:
        driver = Driver(args.application.resolve(), args.output)
        for entry in Path("/proc").iterdir():
            try:
                if entry.name.isdigit() and entry.stat().st_uid == os.getuid() and (entry / "exe").resolve().name == "rec-eos-launcher":
                    environment = dict(part.split(b"=", 1) for part in (entry / "environ").read_bytes().split(b"\0") if b"=" in part)
                    backend = environment[b"GDK_BACKEND"].decode()
                    expected = "x11" if args.application.name == "receos-appimage" else os.environ["GDK_BACKEND"]
                    assert backend == expected, (backend, expected)
                    if os.environ.get("RECEOS_APPLICATION_DISPLAY"):
                        assert environment[b"DISPLAY"].decode() == os.environ["RECEOS_APPLICATION_DISPLAY"]
                    break
            except (PermissionError, FileNotFoundError):
                continue
        assert backend, "Native application display backend was not observed"
        capabilities = driver.invoke("get_platform_capabilities")
        assert capabilities["linux"] and not capabilities["windowsSecurity"]
        assert driver.js("return getComputedStyle(document.querySelector('#btnExcludeAv')).display") == "none"
        driver.click("#nav-settings")
        driver.click("#setnav-proton")
        wait(lambda: driver.js("return !document.querySelector('#setpage-proton').hidden && $('protonStatus').dataset.state !== 'checking'"), "Proton settings page")
        assert driver.js("return $('protonStatus').dataset.state") == "error"
        record("Linux platform controls and Windows security controls")
        driver.click("#setnav-appearance")
        driver.js("const select=document.querySelector('#cfgTheme');select.value='win98';select.dispatchEvent(new Event('change',{bubbles:true}));")
        wait(lambda: driver.invoke("cmd_get_config")["theme"] == "win98", "theme persistence")
        assert driver.js("return document.body.classList.contains('theme-win98')")
        record("Theme change and native configuration persistence")
        assert driver.async_js("ensureChimeCtx();await chimeCtx.resume();playNotifChime();return chimeCtx.state;") == "running"
        time.sleep(0.6)
        assert driver.js("return chimeCtx.state") == "running"
        record("Notification audio runs through GStreamer and the silent audio service")
        driver.click("#setnav-general")
        assert driver.invoke("get_autostart") is False
        driver.click("#tgl-launchAtStartup")
        wait(lambda: driver.invoke("get_autostart") is True, "autostart enable")
        desktop = Path(os.environ["XDG_CONFIG_HOME"]) / "autostart/com.receos.launcher.desktop"
        subprocess.run(["desktop-file-validate", str(desktop)], check=True)
        assert "--background" in desktop.read_text()
        driver.click("#tgl-launchAtStartup")
        wait(lambda: not desktop.exists(), "autostart disable")
        record("XDG autostart opt-in, valid desktop entry and removal")
        wait(lambda: watcher.items, "native tray registration")
        # The native menu's first item opens the skinned panel (the Windows
        # right-click panel); its picks go through tray_menu_pick.
        driver.async_js("window.__validationPanel=[];await window.__TAURI__.event.listen('tray-panel',e=>window.__validationPanel.push(e.payload));return true;")
        menu_action(bus, watcher, "Rec EOS Launcher…")
        wait(lambda: driver.js("return window.__validationPanel.includes(true)"), "tray panel opens")
        driver.invoke("tray_menu_pick", {"id": "network:radium"})
        wait(lambda: driver.js("return document.body.classList.contains('network-radium')"), "tray panel network pick")
        driver.invoke("tray_menu_pick", {"id": "network:receos"})
        wait(lambda: driver.js("return document.body.classList.contains('network-receos')"), "tray panel return pick")
        driver.invoke("tray_menu_hide")
        wait(lambda: driver.js("return window.__validationPanel.includes(false)"), "tray panel closes")
        record("Native tray registration, skinned tray panel and its network picks")
        driver.click("#tgl-runInBackground")
        wait(lambda: driver.invoke("cmd_get_config")["runInBackground"], "tray setting")
        driver.async_js("await window.__TAURI__.event.listen('launcher-hidden',()=>window.__validationHidden=true);return true;")
        # WebKitGTK may suspend automation while unmapped. Return the driver
        # response before hiding and restore through D-Bus before querying it.
        driver.async_js("setTimeout(()=>window.__TAURI__.window.getCurrentWindow().close(),1000);return true;")
        time.sleep(2)
        menu_action(bus, watcher, "Open window")
        wait(lambda: driver.async_js("return await window.__TAURI__.window.getCurrentWindow().isVisible();"), "tray restore")
        wait(lambda: driver.js("return window.__validationHidden===true"), "native hide event")
        record("Close to tray and restore through the native menu")
        marker = "RecEOS account-free notification validation"
        driver.invoke("desktop_notify", {"cards": [{"app": "Rec EOS validation", "parts": [{"t": marker}]}]})
        wait(lambda: marker in (args.output / "notifications.log").read_text(), "desktop notification delivery")
        record("Desktop notification service receives launcher notifications")
        status = driver.invoke("get_linux_runtime_status", {"network": "receos"})
        assert status["ready"] is False and "Steam" in status["error"]
        assert driver.invoke("check_steam") is False
        record("Missing native Steam and Proton produce actionable readiness errors")
        ticket = subprocess.run([str(args.application.resolve()), "--stella-steam-ticket"],
                                input="", text=True, capture_output=True, timeout=15)
        answer = next(line.removeprefix("RADIUM-STEAM-TICKET ") for line in ticket.stdout.splitlines()
                      if line.startswith("RADIUM-STEAM-TICKET "))
        assert ticket.returncode == 1 and json.loads(answer).get("error"), ticket.stdout
        record("Native Linux Steam SDK helper fails cleanly without a Steam session")
        # Steam metadata fixtures exercise discovery/validation, not game
        # compatibility. No fixture runtime executable is ever run.
        steam = home / "Steam & Games"
        proton = steam / "steamapps/common/Proton 10.0"
        runtime = steam / "steamapps/common/SteamLinuxRuntime_sniper"
        proton.mkdir(parents=True)
        runtime.mkdir(parents=True)
        (proton / "proton").write_text("#!/bin/sh\nexit 99\n")
        (proton / "toolmanifest.vdf").write_text('"manifest" { "require_tool_appid" "1628350" }')
        (steam / "steamapps/appmanifest_1628350.acf").write_text('"AppState" { "installdir" "SteamLinuxRuntime_sniper" }')
        (runtime / "_v2-entry-point").write_text("#!/bin/sh\nexit 99\n")
        config = driver.invoke("cmd_get_config")
        config["linux"] = {"steamDir": str(steam), "networks": {}}
        assert driver.invoke("cmd_save_config", {"config": config})
        assert driver.invoke("get_linux_runtime_status", {"network": "receos"})["ready"]
        (runtime / "_v2-entry-point").unlink()
        missing = driver.invoke("get_linux_runtime_status", {"network": "receos"})
        assert not missing["ready"] and "1628350" in missing["error"]
        (runtime / "_v2-entry-point").write_text("#!/bin/sh\nexit 99\n")
        config["linux"]["networks"] = {"receos": {"compatDataDir": str(steam / "steamapps/compatdata/92")}}
        assert driver.invoke("cmd_save_config", {"config": config})
        refused_prefix = driver.invoke("get_linux_runtime_status", {"network": "receos"})
        assert not refused_prefix["ready"] and "outside Steam" in refused_prefix["error"]
        config["linux"]["networks"] = {"receos": {"compatDataDir": str(home / "Shared")}, "radium": {"compatDataDir": str(home / "Shared")}}
        assert driver.invoke("cmd_save_config", {"config": config})
        overlap = driver.invoke("get_linux_runtime_status", {"network": "receos"})
        assert not overlap["ready"] and "separate" in overlap["error"]
        config["linux"] = {"steamDir": "", "networks": {}}
        assert driver.invoke("cmd_save_config", {"config": config})
        record("Runtime discovery with spaces, missing runtime, official-prefix and cross-network protections")
        # Exercise the real cleanup IPC, not only its path predicate. A prefix
        # below a known game-data directory would otherwise be removed together
        # with that directory during extraction or uninstall.
        original_config = driver.invoke("cmd_get_config")
        library = home / "retention fixture/Games library with spaces"
        client = library / "Rec EOS"
        prefix = client / "RecRoom_Data/compat data"
        save = prefix / "pfx/drive_c/users/steamuser/save.dat"
        save.parent.mkdir(parents=True)
        save.write_bytes(b"retained prefix save")
        (client / "RecRoom.exe").write_bytes(b"Uninstall fixture only; never executed")
        protected = copy.deepcopy(original_config)
        protected["receos"]["installDir"] = str(library)
        protected["linux"] = {"steamDir": str(steam), "networks": {"receos": {"compatDataDir": str(prefix)}}}
        try:
            assert driver.invoke("cmd_save_config", {"config": protected})
            unsafe = driver.invoke("get_linux_runtime_status", {"network": "receos"})
            assert not unsafe["ready"] and "inside game client" in unsafe["error"]
            # Protection must remain available after Steam is removed too.
            protected["linux"]["steamDir"] = str(home / "missing Steam")
            assert driver.invoke("cmd_save_config", {"config": protected})
            for command in ("uninstall_client", "download_client"):
                refused = driver.invoke(command, {"network": "receos"})
                assert not refused["success"] and "inside game client" in refused["error"], refused
                assert save.read_bytes() == b"retained prefix save"
                assert (client / "RecRoom.exe").is_file()
            hidden = home / "unreadable prefix"
            hidden.mkdir()
            (hidden / "pfx").symlink_to(prefix / "pfx", target_is_directory=True)
            protected["linux"] = {"steamDir": str(steam), "networks": {"receos": {"compatDataDir": str(hidden)}}}
            assert driver.invoke("cmd_save_config", {"config": protected})
            hidden.chmod(0)
            try:
                unreadable = driver.invoke("get_linux_runtime_status", {"network": "receos"})
                assert not unreadable["ready"] and "Cannot inspect path" in unreadable["error"]
                protected["linux"]["steamDir"] = str(home / "missing Steam")
                assert driver.invoke("cmd_save_config", {"config": protected})
                for command in ("uninstall_client", "download_client"):
                    refused = driver.invoke(command, {"network": "receos"})
                    assert not refused["success"] and "Cannot inspect path" in refused["error"], refused
                    assert save.read_bytes() == b"retained prefix save"
                    assert (client / "RecRoom.exe").is_file()
            finally:
                hidden.chmod(0o700)
        finally:
            assert driver.invoke("cmd_save_config", {"config": original_config})
        record("Nested or unreadable Proton prefixes block cleanup without removing saves, even without Steam")
        refused = driver.async_js("try { await window.__TAURI__.core.invoke('download_update',{url:'https://github.com/abod124-sudo/RecEOS-Launcher/releases/download/v99/setup.exe',placeOnDesktop:false});return false;} catch(error){return String(error);}")
        assert "release downloads" in refused
        driver.js("showUpdateModal({currentVersion:'4.0.0',latestVersion:'99.0.0',downloadUrl:'',releaseUrl:'https://github.com/abod124-sudo/RecEOS-Launcher/releases/latest'});")
        assert driver.js("return document.querySelector('#updateNowBtn').textContent") == "Open Release Page"
        driver.click("#updateNowBtn")
        wait(lambda: opened_url.exists(), "release page opens through the native URL handler")
        assert opened_url.read_text().strip() == "https://github.com/abod124-sudo/RecEOS-Launcher/releases/latest"
        record("Linux refuses Windows update installers and offers release downloads")

        data = Path(os.environ["XDG_DATA_HOME"]) / "com.receos.launcher"
        stella = data / "client-stella"
        stella.mkdir(parents=True)
        (stella / "RecRoom.exe").write_bytes(b"MZ" + bytes(4094))
        missing_patch = driver.invoke("launch_game", {"config": {"network": "stella", "playMode": "screen"}})
        assert not missing_patch["success"] and "patch isn't installed" in missing_patch["error"]
        assert driver.invoke("check_game_running") is False
        record("Missing Stella patch blocks launch before any game process starts")

        # Real /proc mappings and pidfds, deliberately synthetic game images.
        client = Path(os.environ["XDG_DATA_HOME"]) / "com.receos.launcher/client-receos"
        client.mkdir(parents=True, exist_ok=True)
        image = client / "RecRoom.exe"
        image.write_bytes(b"MZ" + bytes(4094))
        prefix = client.parent / "proton/receos/pfx"
        prefix.mkdir(parents=True, exist_ok=True)
        unrelated_prefix = home / "unrelated-prefix"
        unrelated_prefix.mkdir()
        outside_image = home / "unrelated-client/RecRoom.exe"
        outside_image.parent.mkdir()
        outside_image.write_bytes(b"MZ" + bytes(4094))
        for selected, selected_image in [(prefix, image), (unrelated_prefix, image), (prefix, outside_image)]:
            env = dict(os.environ, WINEPREFIX=str(selected))
            child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--fixture-game", str(selected_image)], env=env, stdout=subprocess.PIPE, text=True)
            assert child.stdout.readline().strip() == "READY"
            fixtures.append(child)
        wait(lambda: driver.invoke("check_game_running") is True, "native game process discovery")
        # Restart the real native application with the synthetic game alive.
        config = driver.invoke("cmd_get_config")
        config["runInBackground"] = False
        assert driver.invoke("cmd_save_config", {"config": config})
        driver.close()
        driver = Driver(args.application.resolve(), args.output)
        assert driver.invoke("check_game_running") is True
        record("Launcher restart recovers an already-running owned process")
        wait(lambda: driver.js("return isGameRunning"), "frontend running state after restart")
        # Allow the documented two-second monitor to observe this process
        # before stopping it; then require the actual exit event in the UI.
        time.sleep(2.2)
        assert driver.invoke("kill_game") is True
        fixtures[0].wait(timeout=5)
        assert fixtures[1].poll() is None, "An unrelated prefix was stopped"
        assert fixtures[2].poll() is None, "An unrelated client directory was stopped"
        wait(lambda: driver.invoke("check_game_running") is False, "game exit state")
        wait(lambda: driver.js("return !isGameRunning"), "frontend receives native game-closed event")
        record("Real process discovery and Stop preserve unrelated prefixes and client paths")
        for child in fixtures[1:]:
            child.terminate()
            child.wait(timeout=5)
        driver.screenshot(args.output / "home.png")
        driver.click("#nav-settings")
        driver.click("#setnav-proton")
        driver.screenshot(args.output / "settings.png")
        config = driver.invoke("cmd_get_config")
        config["runInBackground"] = False
        assert driver.invoke("cmd_save_config", {"config": config})
        record("Configuration remains writable after desktop and lifecycle checks")
    finally:
        if driver:
            driver.close()
        for child in fixtures:
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=5)
        monitor.terminate()
        dunst.terminate()
        monitor.wait(timeout=5)
        dunst.wait(timeout=5)
        notifications_log.close()
        loop.quit()


if __name__ == "__main__":
    main()
