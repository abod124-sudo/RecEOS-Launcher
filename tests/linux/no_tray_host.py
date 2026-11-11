#!/usr/bin/env python3
"""A desktop without an SNI host must never hide the only launcher window."""
import os
from pathlib import Path
import tempfile
import time
import dbus
from desktop import Driver, wait

root = Path(tempfile.mkdtemp(prefix="receos-no-tray-"))
for key, subdir in [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache")]:
    os.environ[key] = str(root / subdir)
assert not dbus.SessionBus().name_has_owner("org.kde.StatusNotifierWatcher")
driver = Driver(Path("/usr/bin/rec-eos-launcher"), root)
try:
    processes = []
    for entry in Path("/proc").iterdir():
        try:
            if entry.name.isdigit() and entry.stat().st_uid == os.getuid() and (entry / "exe").resolve().name == "rec-eos-launcher":
                processes.append(entry)
        except (PermissionError, FileNotFoundError):
            pass
    assert len(processes) == 1, processes
    native = processes[0]
    config = driver.invoke("cmd_get_config")
    config["runInBackground"] = True
    assert driver.invoke("cmd_save_config", {"config": config})
    driver.async_js("setTimeout(()=>window.__TAURI__.window.getCurrentWindow().close(),1000);return true;")
    wait(lambda: not native.exists() or (native / "stat").read_text().split(")", 1)[1].split()[0] == "Z",
         "close exits when no tray host is available", timeout=5)
    print("PASS: Missing tray host closes normally instead of hiding an inaccessible window")
finally:
    driver.close()

driver = Driver(Path("/usr/bin/rec-eos-launcher"), root, ["--background"])
try:
    assert driver.async_js("return await window.__TAURI__.window.getCurrentWindow().isVisible();")
    print("PASS: Background startup remains visible when no tray host is available")
finally:
    driver.close()
