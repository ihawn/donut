"""CI: install over an earlier install, check the new version runs and user configuration is unchanged.

Run after `cargo build --release`. It packs target/release into a release archive, as the release job does,
and installs it with the bootstrap script for this OS into DONUT_INSTALL_DIR.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

root = Path(__file__).resolve().parents[1]
windows = os.name == "nt"
names = ["donut.exe", "qwen-build-next.exe"] if windows else ["donut", "qwen-build-next"]
with tempfile.TemporaryDirectory() as temp:
    temp = Path(temp)
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"), CI="true")
    env.setdefault("DONUT_INSTALL_DIR", str(temp / "bin"))
    config = temp / "config/donut/config.json"
    config.parent.mkdir(parents=True)
    config.write_text(json.dumps({"aliases": {"keep-me": {"engine": "codex"}}}), encoding="utf-8")
    before = config.read_bytes()
    archive = temp / ("donut.zip" if windows else "donut.tar.gz")
    if windows:
        with zipfile.ZipFile(archive, "w") as packed:
            for name in names:
                packed.write(root / "target/release" / name, f"donut/{name}")
    else:
        with tarfile.open(archive, "w:gz") as packed:
            for name in names:
                packed.add(root / "target/release" / name, f"donut/{name}")
    env["DONUT_INSTALL_SOURCE"] = str(archive)
    command = ["pwsh", "-File", str(root / "install.ps1")] if windows else ["sh", str(root / "install.sh")]
    for _ in range(2):  # The second run is the upgrade over the first.
        subprocess.run(command, env=env, check=True)
    binary = Path(env["DONUT_INSTALL_DIR"]) / names[0]
    expected = "donut " + os.environ.get("DONUT_RELEASE_VERSION", "")
    version = subprocess.check_output([str(binary), "-v"], text=True).strip()
    assert version.startswith(expected.strip()), version
    assert config.read_bytes() == before, "Upgrade changed user configuration"
    assert shutil.which("donut", path=env["DONUT_INSTALL_DIR"]), "donut is not in the install folder"
    print("PASS: installer upgraded Donut and preserved configuration")
