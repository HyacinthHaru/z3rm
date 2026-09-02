#!/bin/sh
# Rebuild the 9p filesystem served to the v86 guest: the static i686-musl
# mux_server, the no-dependency landing TUI, and their startup wrapper.
set -e
cd "$(dirname "$0")"
OUT=../../public/v86/fs
mkdir -p "$OUT"
: "${RUSTFLAGS:=-C linker=rust-lld -C strip=symbols -C panic=abort}"
export RUSTFLAGS
cargo build -p z3rm_guest_tui --target i686-unknown-linux-musl --release
cargo build -p mux_server --manifest-path ../../../crates/mux_server/Cargo.toml \
  --target i686-unknown-linux-musl --no-default-features --features guest --release
STAGE=$(mktemp -d)
cp ../../../target/i686-unknown-linux-musl/release/z3rm-tui "$STAGE/z3rm-tui"
cp ../../public/media/z3rm-terminal-grid.png "$STAGE/z3rm-terminal-grid.png"
cat > "$STAGE/z3rm" <<'SCRIPT'
#!/bin/sh
case "${1-}" in
  a|attach|landing)
    exec /mnt/z3rm-tui
    ;;
  *)
    printf '%s\n' 'usage: /mnt/z3rm {a|attach|landing}' >&2
    exit 2
    ;;
esac
SCRIPT
cp ../../../target/i686-unknown-linux-musl/release/z3rm-server "$STAGE/mux_server"

# §16.9 The site's own markdown, mounted for the in-guest reader. The docs the
# browser renders and the docs the terminal renders are the same files, so
# publishing a change to one publishes it to the other; nothing is rebuilt into
# the disk image.
python3 - ../../src/content/docs/en "$STAGE/docs" <<'PY'
import os, pathlib, shutil, sys

source, destination = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
destination.mkdir(parents=True, exist_ok=True)


def frontmatter(text):
    if not text.startswith("---\n"):
        return {}
    end = text.find("\n---", 4)
    if end == -1:
        return {}
    fields = {}
    for line in text[4:end].splitlines():
        key, separator, value = line.partition(":")
        if separator:
            fields[key.strip()] = value.strip().strip('"')
    return fields


documents = []
for path in sorted(source.rglob("*.md")):
    text = path.read_text(encoding="utf-8")
    fields = frontmatter(text)
    # The home entry is the landing route, which is the app itself.
    if fields.get("section") == "home":
        continue
    relative = path.relative_to(source)
    target = destination / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(path, target)
    try:
        order = int(fields.get("order", "999"))
    except ValueError:
        order = 999
    documents.append((fields.get("section", "zzz"), order, fields.get("title", relative.stem), relative.as_posix()))

# The sidebar's order: by section, then by the order each document declares.
# A reader who knows the site should find the list in the same shape.
documents.sort()
index = destination / "index.txt"
index.write_text(
    "".join(f"{path}\t{title}\n" for _, _, title, path in documents),
    encoding="utf-8",
)
print(f"packaged {len(documents)} docs")
PY
rm -f ../../public/v86/z3rm-server ../../public/v86/z3rm-server.bin
cp "$STAGE/mux_server" ../../public/v86/z3rm-server.bin
cat > "$STAGE/start-mux.sh" <<'SCRIPT'
#!/bin/sh
mkdir -p /dev/pts
mount -t devpts devpts /dev/pts 2>/dev/null || true
dmesg -n 1 2>/dev/null
stty -F /dev/ttyS0 raw -echo 2>/dev/null
printf 'Z3RM_MUX_READY'
export PATH=/mnt:$PATH
exec /mnt/mux_server --serial /dev/ttyS0
SCRIPT
chmod +x "$STAGE/start-mux.sh" "$STAGE/mux_server" "$STAGE/z3rm-tui" "$STAGE/z3rm"
python3 - "$OUT" <<'PY'
import os, sys
for filename in os.listdir(sys.argv[1]):
    if filename.endswith(".bin"):
        os.remove(os.path.join(sys.argv[1], filename))
PY
python3 tools/fs2json.py --out "$OUT/fs.json" "$STAGE"
python3 - "$STAGE" "$OUT" <<'PY'
import hashlib, os, sys
stage, out = sys.argv[1], sys.argv[2]
# Walk: the stage grew a docs/ tree, and listdir would hand back a directory
# to open as a file.
for root, _, files in os.walk(stage):
    for name in files:
        path = os.path.join(root, name)
        h = hashlib.sha256()
        with open(path, "rb", buffering=0) as fh:
            for b in iter(lambda: fh.read(128*1024), b""):
                h.update(b)
        with open(path, "rb") as fh:
            data = fh.read()
        with open(os.path.join(out, h.hexdigest()[:8] + ".bin"), "wb") as fh:
            fh.write(data)
PY
rm -rf "$STAGE"
echo "guest fs packaged into $OUT"
