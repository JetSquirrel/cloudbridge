#!/usr/bin/env python3
"""Print the version `Cargo.lock` resolved for one package.

Two build steps have to agree with the lock file exactly rather than with
whatever is newest: the `wasm-bindgen` CLI must match the `wasm-bindgen`
crate, and the icon catalogue is copied out of the `gpui-kit-assets` version
the build resolved. Both ask here, and both fail loudly when the lock holds
no single answer.
"""

import pathlib
import sys
import tomllib

def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {pathlib.Path(sys.argv[0]).name} <package>", file=sys.stderr)
        return 2

    package = sys.argv[1]
    lock = tomllib.loads(pathlib.Path("Cargo.lock").read_text())
    versions = {entry["version"] for entry in lock["package"] if entry["name"] == package}

    if len(versions) != 1:
        found = ", ".join(sorted(versions)) or "none"
        print(
            f"error: expected exactly one {package} version in Cargo.lock, found {found}",
            file=sys.stderr,
        )
        return 1

    print(versions.pop())
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
