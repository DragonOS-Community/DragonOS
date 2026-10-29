#!/usr/bin/env python3
"""Build a deterministic sysconfig payload (bin/sysconfig.tar).

Guarantees:
- tar members carry numeric owner/group (default 0:0) and explicit modes;
- fixed mtime, ordering and tar header fields, so the output is insensitive
  to the host UID/GID/umask;
- fail closed on symlinks, special nodes, or executable files that are not
  explicitly declared in the metadata spec;
- the source tree is never modified; runs as a regular user.
"""

from __future__ import annotations

import argparse
import io
import os
import re
import stat
import sys
import tarfile
import tempfile
from pathlib import Path

FIXED_MTIME = 0
SPEC_VERSION = 1
# Maximum allowed mode: reject setuid/setgid/sticky bits.
MAX_MODE = 0o777

REPO_ROOT = Path(__file__).resolve().parent.parent


class PayloadError(Exception):
    """Raised on invalid spec or source tree; main exits 1 on this."""


# ---------------------------------------------------------------------------
# Spec parsing
# ---------------------------------------------------------------------------


def _parse_mode(value, where: str) -> int:
    if not isinstance(value, str) or not re.fullmatch(r"[0-7]{3,4}", value):
        raise PayloadError(f"{where}: mode must be a 3-4 digit octal string, got {value!r}")
    mode = int(value, 8)
    if mode & ~MAX_MODE:
        raise PayloadError(f"{where}: setuid/setgid/sticky bits are not allowed: {value!r}")
    return mode


def _validate_rel_path(path: str, where: str) -> None:
    if not path or path.startswith("/") or "\\" in path:
        raise PayloadError(f"{where}: invalid path {path!r}")
    if any(part in ("", ".", "..") for part in path.split("/")):
        raise PayloadError(f"{where}: invalid path {path!r}")


def _parse_value_fallback(value: str, lineno: int):
    value = value.strip()
    if len(value) >= 2 and value.startswith('"') and value.endswith('"'):
        return value[1:-1]
    if re.fullmatch(r"[0-9]+", value):
        return int(value)
    if value.startswith("{") and value.endswith("}"):
        result = {}
        inner = value[1:-1].strip()
        if inner:
            for part in inner.split(","):
                m = re.fullmatch(r"\s*([A-Za-z0-9_-]+)\s*=\s*(.+?)\s*", part)
                if not m:
                    raise PayloadError(
                        f"spec line {lineno}: cannot parse inline table entry {part!r}"
                    )
                result[m.group(1)] = _parse_value_fallback(m.group(2), lineno)
        return result
    raise PayloadError(f"spec line {lineno}: unsupported value {value!r}")


def _parse_spec_fallback(text: str) -> dict:
    """Strict fallback parser used when tomllib (Python 3.11+) is unavailable.

    Only supports the restricted syntax used by the spec file: a top-level
    version key, a [defaults] section, and [files] entries of the form
    "path" = { mode = "0755" }. Anything else is an error, never silently
    ignored.
    """
    data: dict = {"defaults": {}, "files": {}}
    section = None
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].strip()
            if section not in ("defaults", "files"):
                raise PayloadError(f"spec line {lineno}: unsupported section [{section}]")
            continue
        m = re.fullmatch(r'([A-Za-z0-9_-]+|"[^"\n]+")\s*=\s*(.+)', line)
        if not m:
            raise PayloadError(f"spec line {lineno}: cannot parse {raw!r}")
        key, value = m.group(1), m.group(2)
        parsed = _parse_value_fallback(value, lineno)
        if section is None:
            if key != "version" or not isinstance(parsed, int):
                raise PayloadError(
                    f"spec line {lineno}: only 'version = <int>' is allowed at top level"
                )
            data["version"] = parsed
        elif section == "defaults":
            if key.startswith('"'):
                raise PayloadError(
                    f"spec line {lineno}: [defaults] keys must not be quoted"
                )
            data["defaults"][key] = parsed
        else:  # files
            if not key.startswith('"') or not isinstance(parsed, dict):
                raise PayloadError(
                    f'spec line {lineno}: [files] entries must look like '
                    f'"path" = {{ mode = "0755" }}'
                )
            data["files"][key[1:-1]] = parsed
    return data


def _load_spec_raw(spec_path: Path) -> dict:
    text = spec_path.read_text(encoding="utf-8")
    try:
        import tomllib
    except ImportError:
        return _parse_spec_fallback(text)
    try:
        return tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        raise PayloadError(f"invalid TOML in spec file: {exc}") from exc


class Spec:
    def __init__(self, uid: int, gid: int, dir_mode: int, file_mode: int,
                 overrides: dict[str, int]):
        self.uid = uid
        self.gid = gid
        self.dir_mode = dir_mode
        self.file_mode = file_mode
        self.overrides = overrides


def load_spec(spec_path: Path) -> Spec:
    raw = _load_spec_raw(spec_path)

    version = raw.get("version")
    if version != SPEC_VERSION:
        raise PayloadError(f"spec version must be {SPEC_VERSION}, got {version!r}")

    defaults = raw.get("defaults") or {}
    try:
        uid = defaults["uid"]
        gid = defaults["gid"]
        dir_mode = _parse_mode(defaults["directory-mode"], "defaults.directory-mode")
        file_mode = _parse_mode(defaults["regular-file-mode"], "defaults.regular-file-mode")
    except KeyError as exc:
        raise PayloadError(f"spec [defaults] is missing key: {exc}") from exc
    if not isinstance(uid, int) or not isinstance(gid, int) or uid < 0 or gid < 0:
        raise PayloadError("spec [defaults] uid/gid must be non-negative integers")

    overrides: dict[str, int] = {}
    for path, entry in (raw.get("files") or {}).items():
        where = f'files."{path}"'
        _validate_rel_path(path, where)
        if not isinstance(entry, dict) or set(entry) != {"mode"}:
            raise PayloadError(f"{where}: only the 'mode' key is supported")
        overrides[path] = _parse_mode(entry["mode"], where)

    return Spec(uid, gid, dir_mode, file_mode, overrides)


# ---------------------------------------------------------------------------
# Source tree scanning
# ---------------------------------------------------------------------------


def scan_tree(src: Path) -> dict[str, tuple[str, Path, int]]:
    """Return {relative_path: (kind, source_path, st_mode)}.

    kind is "dir" or "file". Uses lstat semantics and never follows symlinks;
    symlinks and special nodes are rejected immediately.
    """
    entries: dict[str, tuple[str, Path, int]] = {}
    for root, dir_names, file_names in os.walk(src, followlinks=False):
        root_path = Path(root)
        for name in dir_names + file_names:
            full = root_path / name
            rel = full.relative_to(src).as_posix()
            _validate_rel_path(rel, "source tree")
            st = os.lstat(full)
            if stat.S_ISLNK(st.st_mode):
                raise PayloadError(f"source tree contains a symlink (not supported): {rel}")
            if stat.S_ISDIR(st.st_mode):
                entries[rel] = ("dir", full, st.st_mode)
            elif stat.S_ISREG(st.st_mode):
                entries[rel] = ("file", full, st.st_mode)
            else:
                raise PayloadError(
                    f"source tree contains a special node (fifo/socket/device): {rel}"
                )
    return entries


def resolve_mode(rel: str, kind: str, st_mode: int, spec: Spec) -> int:
    if rel in spec.overrides:
        return spec.overrides[rel]
    if kind == "dir":
        return spec.dir_mode
    if st_mode & 0o111:
        raise PayloadError(
            f"executable file is not declared in the metadata spec [files]: {rel}\n"
            f'declare its mode in user/sysconfig-metadata.toml (e.g. "0755")'
        )
    return spec.file_mode


# ---------------------------------------------------------------------------
# Tar generation
# ---------------------------------------------------------------------------


def build_tar_bytes(src: Path, spec: Spec) -> bytes:
    entries = scan_tree(src)

    for declared in spec.overrides:
        if declared not in entries:
            raise PayloadError(
                f"spec declares a path that does not exist in the source tree: {declared}"
            )

    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as tf:
        for rel in sorted(entries, key=lambda p: p.encode("utf-8")):
            kind, full, st_mode = entries[rel]
            info = tarfile.TarInfo(rel)
            info.mtime = FIXED_MTIME
            info.uid = spec.uid
            info.gid = spec.gid
            info.uname = ""
            info.gname = ""
            info.mode = resolve_mode(rel, kind, st_mode, spec)
            if kind == "dir":
                info.type = tarfile.DIRTYPE
                tf.addfile(info)
            else:
                data = full.read_bytes()
                info.size = len(data)
                tf.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def write_if_changed(out: Path, data: bytes) -> bool:
    """Skip replacement when content is unchanged, so downstream make
    dependencies are not needlessly rebuilt."""
    if out.exists() and out.read_bytes() == data:
        return False
    out.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=out.parent, prefix=out.name + ".", suffix=".tmp")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
        os.replace(tmp, out)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return True


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--src", type=Path, default=REPO_ROOT / "user/sysconfig",
                        help="sysconfig source directory")
    parser.add_argument("--spec", type=Path,
                        default=REPO_ROOT / "user/sysconfig-metadata.toml",
                        help="metadata spec file")
    parser.add_argument("--out", type=Path, default=REPO_ROOT / "bin/sysconfig.tar",
                        help="output tar path")
    args = parser.parse_args(argv)

    try:
        spec = load_spec(args.spec)
        data = build_tar_bytes(args.src, spec)
    except PayloadError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
    except OSError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1

    changed = write_if_changed(args.out, data)
    state = "updated" if changed else "unchanged"
    print(f"sysconfig payload: {args.out} ({state}, {len(data)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
